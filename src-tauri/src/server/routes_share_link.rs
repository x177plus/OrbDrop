//! 共享项的**免配对 HTTP 下载链接**（右键「复制下载链接」）。
//!
//! 链接形如 `http://<内网地址>:<端口>/s/<shareId>/<下载文件名>?path=<共享项内相对路径>`。
//! 它不要求对端配对、也不要求对端处在长连接：URL 本身就是这次下载的能力 —— 但只对**开着
//! 广播开关**的共享项生效（`broadcast_enabled`，与设备页的可见性用同一个开关）。关掉开关或
//! 移除共享项，这条 URL 立刻 404，不需要再单独撤一次令牌。
//!
//! **最后一段文件名是给浏览器兜底的**：正常浏览器认 `Content-Disposition`，但有些下载器/
//! WebView 只拿 URL 最后一段当文件名；带上 `<文件名>`（目录是 `<名字>.zip`）后两边都能得到
//! 正确的名字。handler 只把 `/s/` 之后的第一段当 shareId，后面的段忽略；旧链接
//! `/s/<shareId>` 仍然可用。
//!
//! **为什么可以用 http**：主端口的明文连接默认只服务回环，但共享链接是唯一的例外
//! （`net::dispatch_conn` 只对 `GET/HEAD /s/<一个路径段>` 放行明文，其余路径仍然只认 TLS）。
//! 这样另一台设备的浏览器打开链接就直接开始下载，不会先弹自签证书警告；链接本身就是能力，
//! 内容本来就是给局域网任意设备下载的。
//!
//! 为什么不是 `/file?share=...`：那条路在 `auth_routes` 里，远端必须带 Bearer token；而
//! 「复制链接到另一台设备、用浏览器直接下」的浏览器没有 token。这里把 gate 换成共享项自己的
//! 开播开关，和 `/dav/<shareId>` 的「shareId 即路径段」是同一种取舍（shareId 是 v4 UUID，
//! 不是可猜的短编号）。
//!
//! 文件直接流原字节；**目录边遍历边压成 ZIP 流**（`async_zip` 的 data descriptor 模式），
//! 不落临时文件、也不先算整棵树的大小 —— 首字节在遍历到第一个文件时就开始回。目录下载
//! 的 ZIP 顶层带共享项（或子目录）自己的名字，解压出来是一棵有根的树，而不是把文件倒进
//! 当前目录。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_zip::base::write::ZipFileWriter;
use async_zip::{Compression, ZipDateTime, ZipEntryBuilder};
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::compat::FuturesAsyncWriteCompatExt;
use tokio_util::io::ReaderStream;
use tracing::{debug, warn};

use crate::state::AppState;

/// ZIP 流与 HTTP 流之间的管道容量。有界：浏览器读得慢时，压包任务会停在 `write` 上，
/// 不会把整棵目录攒进内存。
const ZIP_STREAM_CAPACITY: usize = 64 * 1024;

#[derive(Debug, Deserialize, Default)]
pub struct ShareLinkQuery {
    /// 共享项内相对路径（`/` 或 `\` 分隔都认）。缺省/空串 = 共享项根自身。
    #[serde(default)]
    pub path: String,
}

/// GET /s/{share_id}[/{filename}]?path=<relative>
///
/// 公开面（不经过 `auth_middleware`），认人规则在 handler 里：
/// 共享项存在且 `broadcastEnabled == true` 才服务，否则一律 404（不泄露「本机有这个 id」）。
///
/// 第二个路径段（下载文件名）只是给浏览器/下载器兜底用的，这里**只取第一段**当 shareId；
/// 实际下载的是 `path` 指向的那一项，文件名以 `Content-Disposition` 为准。
pub async fn share_link_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(rest): AxumPath<String>,
    Query(query): Query<ShareLinkQuery>,
) -> Result<Response, (StatusCode, String)> {
    let share_id = rest.split('/').next().unwrap_or_default().to_string();
    if share_id.is_empty() {
        return Err((StatusCode::NOT_FOUND, "Share not found".to_string()));
    }
    let share = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .find(|s| s.id == share_id && s.broadcast_enabled)
            .cloned()
    }
    .ok_or_else(|| (StatusCode::NOT_FOUND, "Share not found".to_string()))?;

    let rel_path = query.path;
    if !crate::server::safe_relative_share_path(&rel_path) {
        return Err((StatusCode::FORBIDDEN, "Path access denied".to_string()));
    }

    let target = if rel_path.is_empty() {
        share.path.clone()
    } else {
        share.path.join(&rel_path)
    };

    let canonical_target = target
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, "Path not found".to_string()))?;
    let canonical_root = share.path.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法解析共享项根目录".to_string(),
        )
    })?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".to_string()));
    }

    let metadata = tokio::fs::metadata(&canonical_target)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Path not found".to_string()))?;

    if metadata.is_dir() {
        // 先探一次目录能不能读：读不了就**在响应头之前**回 403，而不是先发一个空 ZIP
        // 再在后台任务里报错 —— 后者会让客户端下到一个"成功但什么都没有"的包。
        let mut probe = tokio::fs::read_dir(&canonical_target)
            .await
            .map_err(|_| (StatusCode::FORBIDDEN, "无法读取目录".to_string()))?;
        probe
            .next_entry()
            .await
            .map_err(|_| (StatusCode::FORBIDDEN, "无法读取目录".to_string()))?;

        let display_name = item_display_name(&share.name, &rel_path, &canonical_target);
        let disposition =
            content_disposition(&crate::server::share_download_name(&display_name, true))?;
        let zip_root = sanitize_zip_root(&display_name);

        let (reader, writer) = tokio::io::duplex(ZIP_STREAM_CAPACITY);
        let root = canonical_target.clone();
        // 目录遍历的边界是**共享项根**（不是这次请求的子目录）：子目录里若有指向共享根
        // 之外的 junction/symlink，不跟过去。
        let containment = canonical_root.clone();
        debug!(%share_id, path = %rel_path, "share link zip started");
        crate::task::spawn("share link zip", async move {
            if let Err(e) = write_zip(writer, root, zip_root, containment).await {
                // 响应头已经发出去了，这里只能让连接提前结束；客户端会得到一个不完整的
                // ZIP 并报解压失败 —— 这是诚实的失败方式（不能假装下完）。
                warn!(%share_id, error = %e, "share link zip aborted");
            }
        });

        let body = Body::from_stream(ReaderStream::with_capacity(reader, ZIP_STREAM_CAPACITY));
        let mut response = Response::new(body);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/zip"),
        );
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, disposition);
        no_store(&mut response);
        return Ok(response);
    }

    if !metadata.is_file() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Path is not a file or directory".to_string(),
        ));
    }

    let display_name = item_display_name(&share.name, &rel_path, &canonical_target);
    let disposition = content_disposition(&crate::server::share_download_name(&display_name, false))?;
    let file = tokio::fs::File::open(&canonical_target)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "File not found".to_string()))?;

    let body = Body::from_stream(ReaderStream::with_capacity(file, crate::net::STREAM_CHUNK));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    no_store(&mut response);
    Ok(response)
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
}

/// 下载文件（或 ZIP 包）的名字：共享项根用共享项自己的显示名，子项用它在磁盘上的名字。
fn item_display_name(share_name: &str, rel_path: &str, canonical: &Path) -> String {
    let raw = if rel_path.is_empty() {
        share_name.to_string()
    } else {
        canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| share_name.to_string())
    };
    if raw.trim().is_empty() {
        "download".to_string()
    } else {
        raw
    }
}

/// `Content-Disposition`：ASCII 回退名 + RFC 5987 的 UTF-8 原名。两种客户端都能拿到正确文件名，
/// 且这里只产出可见 ASCII，不会把文件名里的 CR/LF 带进响应头。
fn content_disposition(name: &str) -> Result<HeaderValue, (StatusCode, String)> {
    let ascii = ascii_filename(name);
    let encoded = rfc5987(name);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "生成下载文件名失败".to_string(),
        )
    })
}

fn ascii_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.trim().is_empty() {
        out = "download".to_string();
    }
    out
}

/// RFC 5987 的 `attr-char` 之外全部百分号编码（空格是 `%20`，不是 `+`）。
fn rfc5987(name: &str) -> String {
    const ATTR_CHAR: &[u8] = b"!#$&+-.^_`|~";
    let mut out = String::with_capacity(name.len());
    for b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || ATTR_CHAR.contains(b) {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// ZIP 条目里那一层根目录的名字：不能带路径分隔符，也不能带 Windows 解压时会被吞掉的
/// 结尾点/空格。
fn sanitize_zip_root(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim_end_matches(|c: char| c == '.' || c == ' ');
    let trimmed = trimmed.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "share".to_string()
    } else {
        trimmed.to_string()
    }
}

fn zip_entry(
    name: &str,
    compression: Compression,
    modified: Option<SystemTime>,
) -> ZipEntryBuilder {
    let builder = ZipEntryBuilder::new(name.to_string().into(), compression);
    match modified {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            builder.last_modification_date(ZipDateTime::from(&dt))
        }
        None => builder,
    }
}

/// 把 `root` 目录写成 ZIP 流，顶层加 `root_name/`。
///
/// 遍历是**迭代**的（显式栈），不递归：共享项里可以有很深的目录，递归会吃光异步任务的
/// 栈。隐藏项与符号链接都跳过 —— 前者与 `/tree` 的口径一致（界面本来就不显示），后者是
/// 安全边界：链接可能指向共享根之外，跟过去等于把用户没共享的文件也打进去。目录每次入栈
/// 前再对 `containment`（共享项根）核一次 canonical 路径，挡住不报 `is_symlink` 的 junction。
async fn write_zip<W>(
    sink: W,
    root: PathBuf,
    root_name: String,
    containment: PathBuf,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut zip = ZipFileWriter::with_tokio(sink);
    let root_prefix = format!("{root_name}/");
    zip.write_entry_whole(zip_entry(&root_prefix, Compression::Stored, None), &[])
        .await?;

    let mut stack: Vec<(PathBuf, String)> = vec![(root, root_name)];
    // 同一棵真实目录只进一次：内部 junction 可以指回祖先，没有这张表就会无限展开。
    let mut visited: std::collections::HashSet<PathBuf> =
        stack.iter().map(|(p, _)| p.clone()).collect();
    while let Some((dir, prefix)) = stack.pop() {
        let mut read_dir = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) => {
                // 读不到这一层（权限/掉盘）：已经写过目录条目，就让它是个空目录；
                // 把这一层跳过去比让整个下载失败更好。
                warn!(dir = %dir.display(), error = %e, "share link zip: cannot read directory");
                continue;
            }
        };

        let mut entries = Vec::new();
        loop {
            match read_dir.next_entry().await {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => break,
                Err(e) => {
                    warn!(dir = %dir.display(), error = %e, "share link zip: directory read aborted");
                    break;
                }
            }
        }
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let file_type = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }

            let child_path = entry.path();
            let entry_name = format!("{prefix}/{name}");
            if file_type.is_dir() {
                // 显式再 canonicalize 一次：Windows 的 junction 在 `file_type()` 上不一定
                // 报 `is_symlink`，只看那一档可能把共享根之外的目录也走进去。
                let real_dir = match tokio::fs::canonicalize(&child_path).await {
                    Ok(p) if p.starts_with(&containment) => p,
                    Ok(p) => {
                        warn!(path = %p.display(), "share link zip: directory escapes the share root");
                        continue;
                    }
                    Err(e) => {
                        warn!(path = %child_path.display(), error = %e, "share link zip: cannot resolve directory");
                        continue;
                    }
                };
                if !visited.insert(real_dir.clone()) {
                    warn!(path = %real_dir.display(), "share link zip: directory already visited");
                    continue;
                }
                zip.write_entry_whole(
                    zip_entry(
                        &format!("{entry_name}/"),
                        Compression::Stored,
                        entry_modified(&entry).await,
                    ),
                    &[],
                )
                .await?;
                stack.push((real_dir, entry_name));
            } else if file_type.is_file() {
                let mut file = match tokio::fs::File::open(&child_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        warn!(path = %child_path.display(), error = %e, "share link zip: cannot open file");
                        continue;
                    }
                };
                let mut writer = zip
                    .write_entry_stream(zip_entry(
                        &entry_name,
                        Compression::Deflate,
                        entry_modified(&entry).await,
                    ))
                    .await?;
                let mut tokio_writer = writer.compat_write();
                tokio::io::copy(&mut file, &mut tokio_writer).await?;
                tokio_writer.flush().await?;
                // `EntryStreamWriter::close()` 才会写 data descriptor 与中央目录项；
                // 不能只 drop —— 那样下出来的是一个坏包。
                writer = tokio_writer.into_inner();
                writer.close().await?;
            }
        }
    }

    zip.close().await?;
    Ok(())
}

async fn entry_modified(entry: &tokio::fs::DirEntry) -> Option<SystemTime> {
    entry.metadata().await.ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShareItem;

    /// 本机在局域网上的地址：用一次不发包的 UDP connect 让内核挑出出口网卡。
    /// （与 `routes_dav` 测试里那份同一个办法；没有局域网出口时返回 `None`，测试跳过。）
    fn local_lan_ip() -> Option<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let ip = socket.local_addr().ok()?.ip();
        if ip.is_loopback() {
            None
        } else {
            Some(ip)
        }
    }

    #[tokio::test]
    async fn share_link_works_over_lan_plain_http_and_other_paths_are_dropped() {
        let dir = std::env::temp_dir().join(format!("orbdrop_link_lan_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lan.txt");
        std::fs::write(&file, b"from another device").unwrap();

        let mut config = crate::config::test_config();
        let mut share = ShareItem::new(file.clone());
        share.broadcast_enabled = true;
        let share_id = share.id.clone();
        config.shares.push(share);
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!("orbdrop_link_cfg_{}.toml", uuid::Uuid::new_v4())),
        ));
        let port = crate::server::start_server(state.clone(), 4101)
            .await
            .expect("起服务");
        let Some(ip) = local_lan_ip() else {
            crate::server::stop_server(&state).await;
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };

        // 另一台设备的浏览器真实会走的那条路：**非回环 + 明文 + 无 token**。
        // `dispatch_conn` 对 `/s/<id>` 与 `/s/<id>/<文件名>` 放行明文，浏览器打开即开始下载。
        let plain = reqwest::Client::new()
            .get(format!("http://{ip}:{port}/s/{share_id}/lan.txt"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .expect("局域网明文共享链接");
        assert_eq!(plain.status(), reqwest::StatusCode::OK);
        assert_eq!(
            plain.bytes().await.unwrap().as_ref(),
            b"from another device"
        );

        // 旧链接（不带文件名段）也仍然可用。
        let legacy = reqwest::Client::new()
            .get(format!("http://{ip}:{port}/s/{share_id}"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .expect("旧链接");
        assert_eq!(legacy.status(), reqwest::StatusCode::OK);

        // 同一个端口上的普通明文路径仍然要被丢掉：`/shares` 是认证路由，不能因为
        // 共享链接开了明文口子就退化成可选 TLS。
        let blocked = reqwest::Client::new()
            .get(format!("http://{ip}:{port}/shares"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        assert!(blocked.is_err(), "非共享链接的局域网明文必须连不上，实际拿到 {blocked:?}");

        // TLS 那条路照旧可用（老链接/手改成 https 也不坏）。
        let tls = crate::net::client()
            .get(format!("https://{ip}:{port}/s/{share_id}"))
            .send()
            .await
            .expect("局域网 TLS 请求");
        assert_eq!(tls.status(), reqwest::StatusCode::OK);
        assert_eq!(tls.bytes().await.unwrap().as_ref(), b"from another device");

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn share_link_serves_broadcast_file_without_pairing() {
        let dir = std::env::temp_dir().join(format!("orbdrop_link_file_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, b"hello from a public link").unwrap();

        let mut config = crate::config::test_config();
        let mut share = ShareItem::new(file.clone());
        share.broadcast_enabled = true;
        let share_id = share.id.clone();
        config.shares.push(share);
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!("orbdrop_link_cfg_{}.toml", uuid::Uuid::new_v4())),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4097)
            .await
            .expect("起服务");

        // 带一个**外来 Origin**：auth 中间件会把它当远端（要求 token），而这条公开路由
        // 必须照样能下 —— 这正是"复制到另一台设备的浏览器里直接打开"的路径。
        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/s/{share_id}"))
            .header(reqwest::header::ORIGIN, "https://evil.example")
            .send()
            .await
            .expect("请求");
        let status = resp.status();
        let disposition = resp
            .headers()
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp.bytes().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(body.as_ref(), b"hello from a public link");
        assert_eq!(
            disposition,
            "attachment; filename=\"hello.txt\"; filename*=UTF-8''hello.txt"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn share_link_is_404_when_broadcast_is_off() {
        let dir = std::env::temp_dir().join(format!("orbdrop_link_off_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secret.txt");
        std::fs::write(&file, b"not for unpaired devices").unwrap();

        let mut config = crate::config::test_config();
        let share = ShareItem::new(file.clone()); // 默认 broadcast_enabled = false
        let share_id = share.id.clone();
        config.shares.push(share);
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!("orbdrop_link_cfg_{}.toml", uuid::Uuid::new_v4())),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4098)
            .await
            .expect("起服务");

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/s/{share_id}"))
            .send()
            .await
            .expect("请求");
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn share_link_packs_a_directory_as_a_streaming_zip() {
        let dir = std::env::temp_dir().join(format!("orbdrop_link_zip_{}", uuid::Uuid::new_v4()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.txt"), b"alpha").unwrap();
        std::fs::write(sub.join("b.txt"), b"beta").unwrap();
        std::fs::write(dir.join(".hidden"), b"nope").unwrap();

        let mut config = crate::config::test_config();
        let mut share = ShareItem::new(dir.clone());
        share.name = "My Folder".to_string();
        share.broadcast_enabled = true;
        let share_id = share.id.clone();
        config.shares.push(share);
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!("orbdrop_link_cfg_{}.toml", uuid::Uuid::new_v4())),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4099)
            .await
            .expect("起服务");

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/s/{share_id}"))
            .send()
            .await
            .expect("请求");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/zip")
        );
        let body = resp.bytes().await.unwrap().to_vec();
        assert!(body.starts_with(b"PK\x03\x04"), "必须是 ZIP 流");

        // 用同一个 crate 的读端验一遍：条目名、隐藏项过滤与内容都要对得上。
        #[allow(deprecated)]
        let reader = async_zip::base::read::mem::ZipFileReader::new(body)
            .await
            .expect("解析 ZIP");
        let entries = reader.file().entries();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.filename().as_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"My Folder/".to_string()),
            "顶层要有文件夹条目"
        );
        assert!(names.contains(&"My Folder/a.txt".to_string()));
        assert!(names.contains(&"My Folder/sub/b.txt".to_string()));
        assert!(
            !names.iter().any(|n| n.contains(".hidden")),
            "隐藏项与 /tree 一样不打包"
        );

        let index = names
            .iter()
            .position(|n| n == "My Folder/sub/b.txt")
            .unwrap();
        let entry_reader = reader.reader_without_entry(index).await.unwrap();
        use tokio_util::compat::FuturesAsyncReadCompatExt;
        let mut tokio_reader = entry_reader.compat();
        let mut data = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut tokio_reader, &mut data)
            .await
            .unwrap();
        assert_eq!(data, b"beta");

        // `path` 参数能指到子目录，URL 最后一段带 ZIP 名（右键树节点复制链接走的就是这个形状）。
        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/s/{share_id}/sub.zip"))
            .query(&[("path", "sub")])
            .send()
            .await
            .expect("请求");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.bytes().await.unwrap();
        assert!(
            body.windows("sub/b.txt".len()).any(|w| w == b"sub/b.txt"),
            "子目录 ZIP 的根应是 sub/ 而不是共享项根"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_disposition_never_leaks_control_characters() {
        let value = content_disposition("a\r\nb \"q\".zip").unwrap();
        let text = value.to_str().unwrap();
        assert!(!text.contains('\r') && !text.contains('\n'));
        assert!(text.contains("filename*=UTF-8''"), "要带 RFC 5987 原名");
        assert_eq!(sanitize_zip_root(".."), "share", "ZIP 根不能是 ..");
        assert_eq!(sanitize_zip_root("a/b"), "a_b");
    }

    #[test]
    fn download_url_carries_the_file_name_and_share_path_round_trip() {
        let url = crate::share_download_url_at(
            "192.168.1.5",
            3890,
            "share-uuid",
            "a b/中#1.txt",
            "报告 1.txt",
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.scheme(), "http");
        assert_eq!(parsed.host_str(), Some("192.168.1.5"));
        assert_eq!(parsed.port(), Some(3890));
        assert_eq!(
            parsed.path(),
            "/s/share-uuid/%E6%8A%A5%E5%91%8A%201.txt",
            "URL 最后一段必须是可读的文件名（浏览器兜底用）"
        );
        let path = parsed
            .query_pairs()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.into_owned());
        assert_eq!(path.as_deref(), Some("a b/中#1.txt"));

        // 旧链接（不带文件名段）仍然生成得出来。
        let legacy =
            crate::share_download_url_at("192.168.1.5", 3890, "share-uuid", "", "").unwrap();
        assert_eq!(
            url::Url::parse(&legacy).unwrap().path(),
            "/s/share-uuid",
            "文件名段为空时保持旧形状"
        );
    }
}
