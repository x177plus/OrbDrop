use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio_util::io::{ReaderStream, StreamReader};
use tracing::{debug, error, warn};

use crate::server::auth::Caller;
use crate::state::AppState;

/// WebDAV 面（F-41 ~ F-43）：Beyond Compare 直连本机的共享项。
///
/// 路径命名空间 `/dav/<shareId>/<相对路径>`：用 **shareId** 而不是共享项名字——名字带空格/
/// 中文/重名，而且会和 `/shares`、`/tree` 这些 REST 路由抢前缀；id 是 UUID，天然 URL 安全。
///
/// 认证复用同一套中间件：`Authorization: Basic base64("x:<token>")`（BC 只支持 Basic，
/// 见 `auth.rs` 的挑战头），或回环地址（本机 UI / 本机的 BC）。
///
/// BC 5.2 实测行为（别改坏）：
/// - 先发一次**不带凭据**的 `HEAD/OPTIONS`，收到 `401 + WWW-Authenticate` 后才带 profile 里的凭据重试；
/// - `PROPFIND` 先 `Depth: 0`（路径**不带**尾斜杠，判断目标是文件还是目录），再 `Depth: 1`（列一层）；
/// - 目录的 `GET` 不能回 405（它会用 GET 探目录），所以这里回 200 + HTML 列表。
pub async fn dav_share_root(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    UrlPath(share_id): UrlPath<String>,
    req: Request<Body>,
) -> Response {
    dispatch(state, caller, share_id, String::new(), req).await
}

pub async fn dav_share_sub(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    UrlPath((share_id, path)): UrlPath<(String, String)>,
    req: Request<Body>,
) -> Response {
    dispatch(state, caller, share_id, path, req).await
}

async fn dispatch(
    state: Arc<AppState>,
    caller: Caller,
    share_id: String,
    rel_path: String,
    req: Request<Body>,
) -> Response {
    // 不可见与不存在都回 404：不向对端泄露「本机有这么个共享项」。
    let share = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .find(|s| s.id == share_id && caller.sees_share(&state, s))
            .cloned()
    };
    let Some(share) = share else {
        return (StatusCode::NOT_FOUND, "Share not found").into_response();
    };

    let target = match resolve_within(&share.path, &rel_path) {
        Ok(t) => t,
        Err(status) => return (status, "Path access denied").into_response(),
    };

    let method = req.method().clone();
    let headers = req.headers().clone();
    let result = match method.as_str() {
        "OPTIONS" => Ok(options_response()),
        "PROPFIND" => propfind_response(&share_id, &rel_path, &target, &headers).await,
        "GET" | "HEAD" => read_response(&target, &headers, method == Method::HEAD).await,
        "PUT" => write_response(&target, req).await,
        "DELETE" => delete_response(&target).await,
        "MKCOL" => mkcol_response(&target).await,
        "MOVE" => move_response(&share_id, &share.path, &target, &headers, false).await,
        "COPY" => move_response(&share_id, &share.path, &target, &headers, true).await,
        "PROPPATCH" => Ok(propstat_response(
            &share_id,
            &rel_path,
            StatusCode::FORBIDDEN,
            "Property updates are not supported",
        )),
        other => {
            warn!(method = other, "WebDAV 收到不支持的方法");
            Ok((
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, ALLOW_METHODS)],
                format!("{other} is not supported"),
            )
                .into_response())
        }
    };

    match result {
        Ok(resp) => resp,
        Err((status, msg)) => {
            debug!(?status, msg, "WebDAV 请求失败");
            (status, msg).into_response()
        }
    }
}

const ALLOW_METHODS: &str =
    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE, COPY";

/// `PUT` 的请求体上限（`Content-Length` 预检 + 流上硬截断都用它）。
///
/// 取 64MiB：与本仓现有"单个请求体"最大的那一档 `MANIFEST_BODY_LIMIT`（`server/mod.rs`，
/// `/push/invite` 等清单路由）同档。依据是不能按传输粒度来卡 —— 推送分块走的是
/// `transfer_block_mb`（默认几 MB）/ `BUNDLE_MAX_BYTES`（8MiB），而 BC 的"保存"是**整份
/// PUT 回来**（没有分块、没有续传），几十 MB 的日志/镜像保存一次是日常操作，卡在几 MB
/// 会把正常写回打死。64MiB 之上已超出"人在 BC 里点一次保存"的规模，正是这条要挡的
/// "写满共享方磁盘"。
const PUT_BODY_LIMIT: u64 = 64 << 20;

// ---------------------------------------------------------------------------
// 路径解析（共享项根目录内的相对路径 → 绝对路径，且不可逃逸）
// ---------------------------------------------------------------------------

/// 把 URL 里的相对路径解析到共享项根目录下。
///
/// 目标允许尚不存在（PUT / MKCOL）：此时逐级向上找最近的已存在祖先做 canonicalize 校验——
/// 直接 canonicalize 不存在的路径会失败，而只做字符串前缀比较挡不住 `link -> ..` 这类符号链接逃逸。
fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, StatusCode> {
    let root_canon = root.canonicalize().map_err(|e| {
        error!("共享项根目录无法解析 {root:?}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut candidate = root_canon.clone();
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\0') || part.contains(':') {
            return Err(StatusCode::FORBIDDEN);
        }
        candidate.push(part);
    }

    let mut probe = candidate.as_path();
    let existing = loop {
        if probe.exists() {
            break probe;
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return Err(StatusCode::FORBIDDEN),
        }
    };
    let existing_canon = existing.canonicalize().map_err(|e| {
        error!("路径无法解析 {existing:?}: {e}");
        StatusCode::FORBIDDEN
    })?;
    if !existing_canon.starts_with(&root_canon) {
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(candidate)
}

// ---------------------------------------------------------------------------
// OPTIONS / PROPFIND
// ---------------------------------------------------------------------------

fn options_response() -> Response {
    (
        StatusCode::OK,
        [
            (header::HeaderName::from_static("dav"), "1,2"),
            (header::HeaderName::from_static("ms-author-via"), "DAV"),
            (header::ALLOW, ALLOW_METHODS),
            (header::CONTENT_LENGTH, "0"),
        ],
    )
        .into_response()
}

#[derive(Debug, PartialEq, Eq)]
enum Depth {
    Zero,
    One,
}

fn parse_depth(headers: &HeaderMap) -> Depth {
    match headers.get("depth").and_then(|v| v.to_str().ok()) {
        Some("0") => Depth::Zero,
        // infinity 按 1 处理：目录树是惰性展开的，逐层 PROPFIND 才是我们支持的语义。
        _ => Depth::One,
    }
}

fn etag_for(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", meta.len(), mtime)
}

fn http_date(time: SystemTime) -> String {
    let dt: DateTime<Utc> = time.into();
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn iso_date(time: SystemTime) -> String {
    let dt: DateTime<Utc> = time.into();
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// href：共享项根目录用 `/dav/<shareId>/`，子项逐段百分号编码（保留 `/`）。
fn href_of(share_id: &str, rel: &str, is_dir: bool) -> String {
    let mut href = String::from("/dav/");
    href.push_str(share_id);
    href.push('/');
    if !rel.is_empty() {
        href.push_str(&percent_encode_path(rel));
    }
    if is_dir && !href.ends_with('/') {
        href.push('/');
    }
    href
}

fn percent_encode_path(rel: &str) -> String {
    let mut out = String::new();
    for b in rel.replace('\\', "/").bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn response_element(share_id: &str, rel: &str, path: &Path) -> Result<String, (StatusCode, String)> {
    let meta = std::fs::metadata(path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取元数据失败: {e}")))?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| share_id.to_string());
    let mut props = format!("<D:displayname>{}</D:displayname>", xml_escape(&name));
    if meta.is_dir() {
        props.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
        props.push_str("<D:getcontentlength>0</D:getcontentlength>");
    } else {
        props.push_str("<D:resourcetype/>");
        props.push_str(&format!(
            "<D:getcontentlength>{}</D:getcontentlength>",
            meta.len()
        ));
        props.push_str("<D:getcontenttype>application/octet-stream</D:getcontenttype>");
    }
    if let Ok(modified) = meta.modified() {
        props.push_str(&format!(
            "<D:getlastmodified>{}</D:getlastmodified>",
            http_date(modified)
        ));
        props.push_str(&format!(
            "<D:creationdate>{}</D:creationdate>",
            iso_date(modified)
        ));
    }
    props.push_str(&format!(
        "<D:getetag>{}</D:getetag>",
        xml_escape(&etag_for(&meta))
    ));

    Ok(format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        xml_escape(&href_of(share_id, rel, meta.is_dir())),
        props
    ))
}

async fn propfind_response(
    share_id: &str,
    rel: &str,
    target: &Path,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    if !target.exists() {
        return Err((StatusCode::NOT_FOUND, "Not found".into()));
    }
    let meta = std::fs::metadata(target)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取元数据失败: {e}")))?;

    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">",
    );
    body.push_str(&response_element(share_id, rel, target)?);

    if meta.is_dir() && parse_depth(headers) == Depth::One {
        let mut entries = tokio::fs::read_dir(target)
            .await
            .map_err(|e| (StatusCode::FORBIDDEN, format!("无权访问: {e}")))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("读取目录失败: {e}")))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name
            } else {
                format!("{}/{name}", rel.trim_end_matches('/'))
            };
            match response_element(share_id, &child_rel, &entry.path()) {
                Ok(el) => body.push_str(&el),
                // 单个子项读不到元数据（并发删除、无权限）不该让整个 PROPFIND 失败。
                Err((_, msg)) => debug!(child = %child_rel, msg, "PROPFIND 跳过子项"),
            }
        }
    }

    body.push_str("</D:multistatus>");
    Ok((
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response())
}

/// PROPPATCH 一律拒（返回 403 的 multistatus）：共享项只做读写，不存自定义属性。
fn propstat_response(share_id: &str, rel: &str, code: StatusCode, reason: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>{}</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 {} {}</D:status></D:propstat></D:response></D:multistatus>",
        xml_escape(&href_of(share_id, rel, false)),
        code.as_u16(),
        reason
    );
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET / HEAD（含 Range）
// ---------------------------------------------------------------------------

async fn read_response(
    target: &Path,
    headers: &HeaderMap,
    head_only: bool,
) -> Result<Response, (StatusCode, String)> {
    let meta = std::fs::metadata(target)
        .map_err(|_| (StatusCode::NOT_FOUND, "Not found".to_string()))?;

    if meta.is_dir() {
        let mut names: Vec<String> = Vec::new();
        let mut entries = tokio::fs::read_dir(target)
            .await
            .map_err(|e| (StatusCode::FORBIDDEN, format!("无权访问: {e}")))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("读取目录失败: {e}")))?
        {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
        names.sort();
        let items: String = names
            .iter()
            .map(|n| {
                format!(
                    "<li><a href=\"{}\">{}</a></li>",
                    percent_encode_path(n),
                    xml_escape(n)
                )
            })
            .collect();
        let body = format!("<html><body><h1>Index</h1><ul>{items}</ul></body></html>");
        let len = body.len();
        let mut resp = if head_only {
            Response::new(Body::empty())
        } else {
            Response::new(Body::from(body))
        };
        *resp.status_mut() = StatusCode::OK;
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );
        h.insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
        h.insert(header::HeaderName::from_static("dav"), "1,2".parse().unwrap());
        return Ok(resp);
    }

    let total = meta.len();
    let (start, end) = parse_range(headers, total);

    let mut file = tokio::fs::File::open(target)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("打开失败: {e}")))?;
    if start > 0 {
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("定位失败: {e}")))?;
    }
    let len = end.saturating_sub(start) + 1;
    // 空文件那一档：`total == 0` 时 `parse_range` 给的是 `(0, 0)`（`last` 是 `saturating_sub(1)`），
    // 直接套 `end - start + 1` 会算出 **1** —— 响应头声明 1 字节、正文 0 字节。按 HTTP 规范读的
    // 客户端（curl、BC 保存一个被清空的文件、任意 DAV 客户端）会当场报"传输提前结束"。
    let len = if total == 0 { 0 } else { len };

    let body = if head_only {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(len)))
    };
    let mut resp = Response::new(body);
    let partial = start > 0 || end + 1 < total;
    *resp.status_mut() = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    h.insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
    h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    h.insert(header::ETAG, etag_for(&meta).parse().unwrap());
    if let Ok(modified) = meta.modified() {
        h.insert(header::LAST_MODIFIED, http_date(modified).parse().unwrap());
    }
    if partial {
        h.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}").parse().unwrap(),
        );
    }
    Ok(resp)
}

/// 单段 Range（`bytes=a-b` / `bytes=a-` / `bytes=-n`）；不支持/非法一律按整文件返回。
fn parse_range(headers: &HeaderMap, total: u64) -> (u64, u64) {
    let last = total.saturating_sub(1);
    let Some(spec) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return (0, last);
    };
    let Some(rest) = spec.trim().strip_prefix("bytes=") else {
        return (0, last);
    };
    let first = rest.split(',').next().unwrap_or("").trim();
    let (a, b) = match first.split_once('-') {
        Some(pair) => pair,
        None => return (0, last),
    };
    if a.is_empty() {
        return match b.parse::<u64>() {
            Ok(n) if n > 0 => (total.saturating_sub(n.min(total)), last),
            _ => (0, last),
        };
    }
    let Ok(start) = a.parse::<u64>() else {
        return (0, last);
    };
    if start > last {
        return (0, last);
    }
    let end = match b.parse::<u64>() {
        Ok(e) => e.min(last),
        Err(_) => last,
    };
    if end < start {
        return (0, last);
    }
    (start, end)
}

// ---------------------------------------------------------------------------
// PUT / MKCOL / DELETE / MOVE / COPY
// ---------------------------------------------------------------------------

/// 一个 PUT 落在哪：先写同目录的 `<name>.orbdrop-part`，写完再原子改名
/// （与 `transfer`、`lib.rs::stream_peer_file` 同一套惯例）。
///
/// 必须**同目录**：跨卷 `rename` 会失败，而共享项目录与系统临时目录不保证在同一个卷上。
fn part_path(target: &Path) -> PathBuf {
    target.with_file_name(format!(
        "{}.orbdrop-part",
        target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload".into())
    ))
}

/// `PUT` 写回：**原子替换** + 有界。
///
/// 旧写法 `File::create(target)` 是就地把请求体写进最终路径：那会当场截断用户的原文件，
/// 写到一半断流（BC 取消保存、连接掉了、进程被杀）就**永久损坏原文件**，还在共享目录里
/// 留一个半截文件顶着真名。现在一律先写 `<name>.orbdrop-part`，**写成功才改名覆盖** ——
/// 目标路径上只有两种状态：原样不动，或是整份新内容。
async fn write_response(
    target: &Path,
    req: Request<Body>,
) -> Result<Response, (StatusCode, String)> {
    if target.is_dir() {
        return Err((StatusCode::METHOD_NOT_ALLOWED, "目标是目录".into()));
    }
    // 体积预检：`Content-Length` 摆在那儿就先拒，不必先读一遍再拒（BC 保存超大文件时立刻
    // 拿到明确原因，而不是等传完才 413）。放在建父目录之前 —— 被拒的请求不该在磁盘上留痕。
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > PUT_BODY_LIMIT {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("请求体过大: {len} 字节，上限 {PUT_BODY_LIMIT} 字节"),
            ));
        }
    }
    // 新建：父目录不存在就建（F-42「新建」要能直接落盘，不能要求调用方先 MKCOL）。
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| (StatusCode::CONFLICT, format!("创建父目录失败: {e}")))?;
        }
    }
    let existed = target.exists();

    let stream = req
        .into_body()
        .into_data_stream()
        .map_err(|e| std::io::Error::other(e.to_string()));
    let reader = StreamReader::new(stream);
    let part = part_path(target);
    // 建不出临时文件 = 这一层目录不可写：与旧写法（`File::create(target)` 失败）同码同文案。
    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("写入失败: {e}")))?;
    // 兜底：`Content-Length` 是调用方自己写的，可缺失也可撒谎（这条路由直接吃 body 流，
    // 绕过 axum 提取器那一档上限），真正卡住体积的是这里的 `take`。读 `PUT_BODY_LIMIT + 1`
    // 而不是正好 `PUT_BODY_LIMIT`：正好截断会把"刚好多一点"的写回**当成成功**，用户拿到
    // 一个被砍掉尾巴的文件 —— 那比直接拒掉更坏。
    let written = match tokio::io::copy(&mut reader.take(PUT_BODY_LIMIT + 1), &mut file).await {
        Ok(n) => n,
        Err(e) => {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败: {e}")));
        }
    };
    if written > PUT_BODY_LIMIT {
        drop(file);
        let _ = tokio::fs::remove_file(&part).await;
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("请求体过大: 超过 {PUT_BODY_LIMIT} 字节"),
        ));
    }
    file.flush().await.ok();
    // Windows 上打开着的文件删不掉（改名也会被占用打扰），先放句柄再改名/清残骸。
    drop(file);
    if let Err(e) = tokio::fs::rename(&part, target).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err((StatusCode::FORBIDDEN, format!("落盘失败: {e}")));
    }

    Ok(if existed {
        debug!(bytes = written, path = ?target, "WebDAV 覆盖写回");
        StatusCode::NO_CONTENT.into_response()
    } else {
        debug!(bytes = written, path = ?target, "WebDAV 新建写回");
        StatusCode::CREATED.into_response()
    })
}

async fn mkcol_response(target: &Path) -> Result<Response, (StatusCode, String)> {
    if target.exists() {
        return Err((StatusCode::METHOD_NOT_ALLOWED, "已存在".into()));
    }
    tokio::fs::create_dir(target)
        .await
        .map_err(|e| (StatusCode::CONFLICT, format!("建目录失败: {e}")))?;
    Ok(StatusCode::CREATED.into_response())
}

async fn delete_response(target: &Path) -> Result<Response, (StatusCode, String)> {
    let meta = std::fs::metadata(target)
        .map_err(|_| (StatusCode::NOT_FOUND, "Not found".to_string()))?;
    let result = if meta.is_dir() {
        tokio::fs::remove_dir_all(target).await
    } else {
        tokio::fs::remove_file(target).await
    };
    result.map_err(|e| (StatusCode::FORBIDDEN, format!("删除失败: {e}")))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// MOVE / COPY：Destination 头可以是绝对 URI 或绝对路径，都解析回同一命名空间。
async fn move_response(
    share_id: &str,
    root: &Path,
    target: &Path,
    headers: &HeaderMap,
    copy: bool,
) -> Result<Response, (StatusCode, String)> {
    if !target.exists() {
        return Err((StatusCode::NOT_FOUND, "Not found".into()));
    }
    let dest_header = headers
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::BAD_REQUEST, "缺少 Destination 头".to_string()))?;
    let dest_rel = destination_relative(share_id, dest_header)
        .ok_or((StatusCode::BAD_GATEWAY, "Destination 不在本共享项内".to_string()))?;
    let dest = resolve_within(root, &dest_rel)
        .map_err(|status| (status, "目标路径不可用".to_string()))?;

    let overwrite = !matches!(
        headers.get("overwrite").and_then(|v| v.to_str().ok()),
        Some("F")
    );
    if dest.exists() && !overwrite {
        return Err((StatusCode::PRECONDITION_FAILED, "目标已存在".into()));
    }
    let existed = dest.exists();
    if let Some(parent) = dest.parent() {
        if !parent.exists() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| (StatusCode::CONFLICT, format!("创建父目录失败: {e}")))?;
        }
    }
    if existed {
        let meta = std::fs::metadata(&dest)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("读取目标失败: {e}")))?;
        let _ = if meta.is_dir() {
            tokio::fs::remove_dir_all(&dest).await
        } else {
            tokio::fs::remove_file(&dest).await
        };
    }

    if copy {
        copy_recursive(target, &dest).await?;
    } else {
        tokio::fs::rename(target, &dest)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("移动失败: {e}")))?;
    }

    Ok(if existed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::CREATED.into_response()
    })
}

async fn copy_recursive(src: &Path, dest: &Path) -> Result<(), (StatusCode, String)> {
    let meta = std::fs::metadata(src)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取源失败: {e}")))?;
    if meta.is_dir() {
        tokio::fs::create_dir_all(dest)
            .await
            .map_err(|e| (StatusCode::CONFLICT, format!("建目录失败: {e}")))?;
        let mut entries = tokio::fs::read_dir(src)
            .await
            .map_err(|e| (StatusCode::FORBIDDEN, format!("读取源目录失败: {e}")))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("读取源目录失败: {e}")))?
        {
            let child_dest = dest.join(entry.file_name());
            Box::pin(copy_recursive(&entry.path(), &child_dest)).await?;
        }
        Ok(())
    } else {
        tokio::fs::copy(src, dest)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("复制失败: {e}")))?;
        Ok(())
    }
}

/// 从 Destination 头里剥出「本共享项内的相对路径」。
fn destination_relative(share_id: &str, raw: &str) -> Option<String> {
    let after_scheme = match raw.find("://") {
        Some(i) => match raw[i + 3..].find('/') {
            Some(j) => &raw[i + 3 + j..],
            None => return None,
        },
        None => raw,
    };
    let path = after_scheme.split(['?', '#']).next()?;
    let prefix = format!("/dav/{share_id}");
    let rest = path.strip_prefix(&prefix)?.trim_start_matches('/');
    let mut out = String::new();
    for segment in rest.split('/') {
        if segment.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&percent_decode(segment)?);
    }
    Some(out)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 仅供测试：HTTP 层走回环会变成 `Caller::Local`，对端可见性只能在单元层面构造。
#[cfg(test)]
async fn dispatch_for_test(
    state: Arc<AppState>,
    caller: Caller,
    share_id: &str,
    rel_path: &str,
    method: Method,
) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(format!("/dav/{share_id}/{rel_path}"))
        .body(Body::empty())
        .unwrap();
    dispatch(state, caller, share_id.to_string(), rel_path.to_string(), req)
        .await
        .status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShareItem;
    use reqwest::{Client, Method as HttpMethod, StatusCode as HttpStatus};

    fn fixture_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbdrop_dav_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("hello.txt"), b"hello world").unwrap();
        std::fs::write(dir.join("sub").join("nested.txt"), b"nested content").unwrap();
        dir
    }

    /// 每个测试用自己的端口段：并行跑时若都从同一个基址往上探，会互相抢端口，
    /// 表现为偶发 `IncompleteMessage`（请求落在一个正在关停的监听上）。
    ///
    /// 绑 `0.0.0.0`（`start_server`）而不是回环：这一组里有三条用例要**拨本机的内网地址**，
    /// 模拟"BC 从另一台设备过来"（见 `local_lan_ip`）—— 绑回环就拨不到了。
    async fn serve(base_port: u16) -> (Arc<AppState>, u16, PathBuf, String) {
        let root = fixture_root();
        let mut config = crate::config::test_config();
        let share = ShareItem::new(root.clone());
        let share_id = share.id.clone();
        config.shares.push(share);
        let config_path =
            std::env::temp_dir().join(format!("orbdrop_dav_cfg_{}.toml", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState::new(config, config_path));
        let port = crate::server::start_server(state.clone(), base_port)
            .await
            .expect("server start");
        (state, port, root, share_id)
    }

    fn propfind() -> HttpMethod {
        HttpMethod::from_bytes(b"PROPFIND").unwrap()
    }

    /// 本机在局域网上的地址：用一次不发包的 UDP connect 让内核挑出出口网卡。
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

    /// BC 的 WebDAV 客户端先**裸发**一次请求，必须收到带 `WWW-Authenticate: Basic` 的 401，
    /// 它才会拿 profile 里的账号密码重试；只回 401 不带挑战头，BC 直接判定连接失败。
    /// （回环地址走 `Caller::Local` 免认证，所以这条必须走本机局域网地址；而对端那一跳现在
    /// 是 TLS —— 用本机那份自签身份按 TOFU 认下来，见 `net::PinnedVerifier`。）
    #[tokio::test]
    async fn non_loopback_dav_requests_get_a_basic_challenge() {
        let (state, port, root, share_id) = serve(4100).await;
        let Some(ip) = local_lan_ip() else {
            // 没有局域网出口（离线机器）：跳过，别把环境问题当成功能问题
            crate::server::stop_server(&state).await;
            let _ = std::fs::remove_dir_all(root);
            return;
        };

        let client = crate::net::client();
        let resp = client
            .get(format!("https://{ip}:{port}/dav/{share_id}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::UNAUTHORIZED, "非回环必须要求认证");
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            challenge.starts_with("Basic"),
            "BC 只支持 Basic，挑战头必须声明它，实际是 {challenge:?}"
        );

        // 裸 URL（不带凭据）也要走同一条路：BC 的第一次探测正是这种请求
        let resp = client
            .request(propfind(), format!("https://{ip}:{port}/dav/{share_id}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::UNAUTHORIZED);

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// **局域网上的明文请求一律连不上**：同一个端口上明文只服务回环（`net::dispatch_conn`）。
    /// 这条是"加密不是可选的"那半句的实现保证 —— 少了它，任何客户端都能退回明文。
    #[tokio::test]
    async fn plaintext_from_the_lan_is_dropped() {
        let (state, port, root, share_id) = serve(4105).await;
        let Some(ip) = local_lan_ip() else {
            crate::server::stop_server(&state).await;
            let _ = std::fs::remove_dir_all(root);
            return;
        };

        let plain = Client::new()
            .get(format!("http://{ip}:{port}/dav/{share_id}/"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        assert!(
            plain.is_err(),
            "局域网地址上的明文请求必须被丢掉，实际拿到 {plain:?}"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// BC 真实形状的凭据（非回环 + Basic + 我们写进 profile 的账号）必须能过认证：
    /// 账号是 `bc::DAV_USER`，密码是该设备的 token。用户名不参与鉴权，但 token 必须对得上。
    #[tokio::test]
    async fn app_profile_credentials_authenticate_over_the_lan() {
        let Some(ip) = local_lan_ip() else {
            return; // 没有局域网出口（离线机器）：跳过
        };
        let (state, port, root, share_id) = serve(4120).await;
        // BC 只看得见"开着广播 + 当刻长连接"的设备侧共享项，这里把两个条件都摆齐。
        state.config.lock().shares[0].broadcast_enabled = true;
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::ws::EVENT_QUEUE_CAPACITY);
        state.ws.register("dav-peer", tx);

        let token = format!("dav-tok-{}", uuid::Uuid::new_v4());
        crate::credential::set_peer("dav-peer", &token).unwrap();

        let client = crate::net::client();
        let url = format!("https://{ip}:{port}/dav/{share_id}/");
        let ok = client
            .get(&url)
            .basic_auth(crate::bc::DAV_USER, Some(&token))
            .send()
            .await
            .unwrap();
        assert!(
            ok.status().is_success(),
            "profile 里的账号密码必须能认证，实际 {}",
            ok.status()
        );

        let bad = client
            .get(&url)
            .basic_auth(crate::bc::DAV_USER, Some("wrong-token"))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), HttpStatus::UNAUTHORIZED, "token 对不上就得 401");

        crate::credential::remove_peer("dav-peer").ok();
        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// BC 先看 OPTIONS 的 DAV 能力声明，再看 PROPFIND 的属性：这两样缺一个，
    /// 远端目录在 BC 里就是"加载不了"（实测：BC 5.2 走的正是这套）。
    #[tokio::test]
    async fn options_advertises_dav_and_propfind_reports_properties() {
        let (state, port, root, share_id) = serve(4110).await;
        let client = Client::new();
        let url = format!("http://127.0.0.1:{port}/dav/{share_id}/");

        let resp = client
            .request(HttpMethod::OPTIONS, &url)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::OK);
        let dav = resp
            .headers()
            .get("dav")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            dav.contains('1'),
            "OPTIONS 必须声明 DAV 能力（BC 靠它判断这是 WebDAV）：status={} headers={:?}",
            resp.status(),
            resp.headers()
        );

        let resp = client
            .request(propfind(), &url)
            .header("Depth", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::MULTI_STATUS);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("<D:collection/>"),
            "目录要报成 collection: {body}"
        );
        assert!(
            body.contains("<D:getlastmodified>"),
            "缺 getlastmodified: {body}"
        );
        assert!(body.contains("hello.txt"));
        assert!(body.contains("sub/"), "子目录 href 要以 / 结尾: {body}");
        assert!(body.contains("<D:getcontentlength>11</D:getcontentlength>"));

        // Depth: 0 只报自己：BC 靠它（且路径不带尾斜杠）判断这一层是文件还是目录。
        let resp = client
            .request(propfind(), format!("http://127.0.0.1:{port}/dav/{share_id}/sub"))
            .header("Depth", "0")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::MULTI_STATUS);
        let body = resp.text().await.unwrap();
        assert!(body.contains("<D:collection/>"));
        assert!(!body.contains("nested.txt"), "Depth 0 不该带子项: {body}");

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// 写回的三种动作（新建 / 覆盖 / 删除）都要真落到共享方磁盘上，
    /// 且新文件落在还不存在的子目录里也要能写（BC 保存新建文件时不一定先 MKCOL）。
    #[tokio::test]
    async fn put_get_range_and_delete_touch_real_files() {
        let (state, port, root, share_id) = serve(4120).await;
        let client = Client::new();
        let file_url = format!("http://127.0.0.1:{port}/dav/{share_id}/newdir/a.txt");

        let resp = client.put(&file_url).body("written").send().await.unwrap();
        assert_eq!(resp.status(), HttpStatus::CREATED);
        assert_eq!(
            std::fs::read_to_string(root.join("newdir/a.txt")).unwrap(),
            "written"
        );

        let resp = client.get(&file_url).send().await.unwrap();
        assert_eq!(resp.status(), HttpStatus::OK);
        assert_eq!(resp.text().await.unwrap(), "written");

        let resp = client
            .get(&file_url)
            .header("Range", "bytes=0-2")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["content-range"], "bytes 0-2/7");
        assert_eq!(resp.text().await.unwrap(), "wri");

        // 覆盖写回：文件已存在 → 204，内容换新。
        let resp = client.put(&file_url).body("rewritten").send().await.unwrap();
        assert_eq!(resp.status(), HttpStatus::NO_CONTENT);
        assert_eq!(
            std::fs::read_to_string(root.join("newdir/a.txt")).unwrap(),
            "rewritten"
        );

        let resp = client
            .delete(format!("http://127.0.0.1:{port}/dav/{share_id}/newdir"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::NO_CONTENT);
        assert!(!root.join("newdir").exists(), "目录删除要递归");

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// **空文件**：GET/HEAD 都必须是「0 字节」，不能声明 1 字节（P1-8）。
    ///
    /// `parse_range` 对 `total == 0` 给的是 `(0, 0)`，旧写法 `end - start + 1` 于是算出 1 ——
    /// 响应头写 1、正文 0，客户端（curl 直接 `transfer closed with 1 bytes remaining to read`）
    /// 与 BC（保存一个被清空的文件）都会读失败。空文件很常见（`.gitkeep`、刚 truncate 的写回）。
    #[tokio::test]
    async fn an_empty_file_is_served_as_zero_bytes() {
        let (state, port, root, share_id) = serve(4140).await;
        let client = Client::new();
        let url = format!("http://127.0.0.1:{port}/dav/{share_id}/empty.txt");

        let put = client.put(&url).body("").send().await.unwrap();
        assert_eq!(put.status(), HttpStatus::CREATED);
        assert_eq!(std::fs::metadata(root.join("empty.txt")).unwrap().len(), 0);

        let get = client.get(&url).send().await.unwrap();
        assert_eq!(get.status(), HttpStatus::OK);
        assert_eq!(
            get.headers()["content-length"], "0",
            "声明 1 字节就是错的：正文只有 0 字节，客户端会当传输提前结束"
        );
        assert_eq!(get.text().await.unwrap(), "");

        let head = client.head(&url).send().await.unwrap();
        assert_eq!(head.status(), HttpStatus::OK);
        assert_eq!(head.headers()["content-length"], "0");

        // 非空文件照旧（别把空文件那条修到别的档上）。
        let big = format!("http://127.0.0.1:{port}/dav/{share_id}/one.txt");
        client.put(&big).body("x").send().await.unwrap();
        let one = client.get(&big).send().await.unwrap();
        assert_eq!(one.headers()["content-length"], "1");

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// MKCOL / COPY / MOVE：BC 在对端建目录、复制到另一侧、重命名都靠这三个方法。
    #[tokio::test]
    async fn mkcol_copy_and_move_stay_inside_the_share() {
        let (state, port, root, share_id) = serve(4130).await;
        let client = Client::new();
        let mkcol = HttpMethod::from_bytes(b"MKCOL").unwrap();
        let dir_url = format!("http://127.0.0.1:{port}/dav/{share_id}/mk");

        let resp = client.request(mkcol.clone(), &dir_url).send().await.unwrap();
        assert_eq!(resp.status(), HttpStatus::CREATED);
        let resp = client.request(mkcol, &dir_url).send().await.unwrap();
        assert_eq!(resp.status(), HttpStatus::METHOD_NOT_ALLOWED, "重复 MKCOL 要报错");

        let src = format!("http://127.0.0.1:{port}/dav/{share_id}/mk/src.txt");
        client.put(&src).body("payload").send().await.unwrap();

        let copy_dest = format!("http://127.0.0.1:{port}/dav/{share_id}/copied.txt");
        let resp = client
            .request(HttpMethod::from_bytes(b"COPY").unwrap(), &src)
            .header("Destination", &copy_dest)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::CREATED);
        assert_eq!(
            std::fs::read_to_string(root.join("copied.txt")).unwrap(),
            "payload"
        );

        let move_dest = format!("http://127.0.0.1:{port}/dav/{share_id}/moved.txt");
        let resp = client
            .request(HttpMethod::from_bytes(b"MOVE").unwrap(), &src)
            .header("Destination", &move_dest)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::CREATED);
        assert_eq!(
            client.get(&src).send().await.unwrap().status(),
            HttpStatus::NOT_FOUND
        );
        assert_eq!(
            client.get(&move_dest).send().await.unwrap().status(),
            HttpStatus::OK
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// 越界路径与「不可见共享项」都必须回 404/403：不泄露本机有什么、更不泄露本机有什么文件。
    #[tokio::test]
    async fn rejects_traversal_and_shares_the_caller_cannot_see() {
        let (state, port, root, share_id) = serve(4140).await;
        let client = Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/dav/{share_id}/..%2F..%2Fsecret.txt"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::FORBIDDEN);

        // 对端看得见的前提是「开播 + 此刻长连接」；这里两者都不成立 → 与不存在同款 404。
        let hidden = dispatch_for_test(
            state.clone(),
            Caller::Peer("ghost-peer".into()),
            &share_id,
            "",
            Method::GET,
        )
        .await;
        assert_eq!(hidden, StatusCode::NOT_FOUND);

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// 拼一个 PUT 请求（`dispatch` 之下的这一层直接吃 body 流，测试也直接在函数层调）。
    fn put_request(body: Body, content_length: Option<u64>) -> Request<Body> {
        let mut builder = Request::builder().method(Method::PUT).uri("/dav/x/a.txt");
        if let Some(len) = content_length {
            builder = builder.header(header::CONTENT_LENGTH, len.to_string());
        }
        builder.body(body).unwrap()
    }

    /// **PUT 的体积上限**：`Content-Length` 说超了就当场拒（413），原文件逐字不动。
    ///
    /// 旧写法在这条路由上没有任何上限（上限只挂在 `/push/*`），而这个处理函数直接吃
    /// `req.into_body().into_data_stream()`，绕过提取器那一档 → 对端可以把共享方磁盘写满。
    /// 这里 body 是空的、只有声明，所以**不该发生任何真实读写**：既能验证"预检在用声明的
    /// 长度"，也能验证"被拒的请求一个字节都不落盘"。
    #[tokio::test]
    async fn an_over_long_put_is_refused_before_anything_is_written() {
        let root = fixture_root();
        let target = root.join("hello.txt");
        let before = std::fs::read(&target).unwrap();

        let err = write_response(
            &target,
            put_request(Body::empty(), Some(PUT_BODY_LIMIT + 1)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            err.1.contains("过大") && err.1.contains("上限"),
            "拒绝原因要说清为什么、上限是多少: {}",
            err.1
        );
        assert_eq!(std::fs::read(&target).unwrap(), before, "原文件必须逐字不变");
        assert!(!part_path(&target).exists(), "被拒的写回不该留下临时文件");

        // 边界：正好等于上限的声明要放行（判据是 `>`，别把"等于"也一起拒了）。
        let fresh = root.join("edge.txt");
        let resp = write_response(&fresh, put_request(Body::from("ok"), Some(PUT_BODY_LIMIT)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "ok");

        let _ = std::fs::remove_dir_all(root);
    }

    /// **兜底那一道**：`Content-Length` 可以缺失、也可以撒谎，所以流本身也要有界。
    ///
    /// 用的是"没有声明长度、真的往上传超限数据"的流；写超了就整体作废 —— 目标路径上既没有
    /// 半截文件，也没有临时文件。注意判据必须是"写出 `PUT_BODY_LIMIT` 字节就算超"：写成
    /// `take(PUT_BODY_LIMIT)` 会把超限的写回**当成成功**并交出被砍掉尾巴的文件，这条用例会挂。
    #[tokio::test]
    async fn a_stream_without_content_length_is_capped_too() {
        let root = fixture_root();
        let target = root.join("streamed.bin");

        // 懒生成：一次只有一块（1MiB）在内存里，总量 65MiB > 上限。
        let chunks = (0..65)
            .map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'x'; 1 << 20])));
        let body = Body::from_stream(futures_util::stream::iter(chunks));

        let err = write_response(&target, put_request(body, None))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!target.exists(), "超限的写回不能在目标路径上留下任何东西");
        assert!(
            !part_path(&target).exists(),
            "超限的写回必须把临时文件也清掉"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// **覆盖写到一半断流 → 原文件内容不变**（本次修的核心可观察结果）。
    ///
    /// 旧写法 `File::create(target)` 在这条路上会当场截断原文件，断在哪里就烂在哪里，还留
    /// 一个半截文件顶着真名；现在是"写临时文件 → 成功才改名"，失败分支把临时文件删掉。
    #[tokio::test]
    async fn an_interrupted_overwrite_leaves_the_original_file_intact() {
        let root = fixture_root();
        let target = root.join("hello.txt");
        let before = std::fs::read(&target).unwrap();

        let body = Body::from_stream(futures_util::stream::iter(vec![
            Ok(axum::body::Bytes::from_static(b"half of a new file")),
            Err(std::io::Error::other("对端断开")),
        ]));
        let err = write_response(&target, put_request(body, Some(64)))
            .await
            .unwrap_err();
        // 与旧写法同一个状态码与文案（这条不是本次要改的行为）。
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(std::fs::read(&target).unwrap(), before, "原文件必须逐字不变");

        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".orbdrop-part"))
            .collect();
        assert!(leftovers.is_empty(), "断流后残留了临时文件: {leftovers:?}");

        let _ = std::fs::remove_dir_all(root);
    }

    /// 失败分支的落点：`write_response` 不覆盖目录（405，与旧写法一致）——
    /// 顺手钉住"原子改名这一改没把这条判断挪走"。
    #[tokio::test]
    async fn a_put_onto_a_directory_is_still_method_not_allowed() {
        let root = fixture_root();
        let err = write_response(
            &root.join("sub"),
            put_request(Body::from("x"), Some(1)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::METHOD_NOT_ALLOWED);
        assert!(root.join("sub").is_dir(), "目录必须原样还在");

        let _ = std::fs::remove_dir_all(root);
    }
}
