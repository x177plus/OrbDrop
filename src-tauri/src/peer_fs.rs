//! 对端共享项的 HTTP 访问：元数据（`/tree`、`/stat`）与文件字节（`/file`）。
//!
//! 从 `drag.rs` 抽出来，因为拖放的两条路（暂存转存 / CFAPI 镜像）与目录树遍历三处都要用同一份。
//! 这里只做"问对端"这一件事，不认识任何本地落盘形态 —— 调用方决定字节往哪去。
//!
//! 到对端的一切都要过 TLS 并按钉住的指纹校验身份（`net::client_builder`），随手
//! `Client::new()` 会在自签证书上直接失败。

use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::state::AppState;

/// 一个远端文件在服务端的坐标。
#[derive(Debug, Clone)]
pub struct PeerFile {
    /// `http://ip:port`
    pub base: String,
    pub token: String,
    pub share_id: String,
    /// 共享项内相对路径（`/` 分隔）
    pub path: String,
}

/// 对端一次 `/file` 要多少字节。资源管理器可能要一整块（比如 100MB），流允许"少给一点"，
/// 所以切成小块，内存不会被文件大小牵着走；提前转存与镜像水合都按这个粒度拉。
pub const READ_CHUNK: u64 = 4 * 1024 * 1024;

/// 问对端用的 HTTP 客户端（元数据与内容共用）。
pub fn client() -> reqwest::Client {
    crate::net::client_builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// 解析某个对端的基地址与 token（"能不能问它"这一步）。
///
/// 拖放与镜像两条路都要它；失败信息是给用户看的，所以要分得清"设备不在列表"与"未配对"。
pub fn coords(state: &Arc<AppState>, peer_id: &str) -> Result<(String, String), String> {
    let peer = state.get_peer(peer_id).ok_or_else(|| "设备不在列表中".to_string())?;
    let token = crate::credential::get_peer_token(peer_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "未配对，无法读取对方的共享项".to_string())?;
    Ok((crate::net::peer_base(peer.dial().0, peer.dial().1), token))
}

/// 对端 `/tree` 回 400 = "这不是目录"（区别于"共享项不可见/不存在"的 404）。
///
/// 用它把"走目录走错了"识别出来，好退化成"拖这一个文件"而不是把用户卡住。
pub const NOT_A_DIRECTORY: &str = "对端说它不是目录";

/// 向对端要一段文件字节（`GET /file?offset=&len=`）。
///
/// 服务端按 `len` 截断，所以**拿回来的比要的少 = 已经到文件末尾**：按需流与提前转存
/// 都靠这条判结束，短读不是错误。
pub async fn fetch_file_range(
    client: &reqwest::Client,
    file: &PeerFile,
    offset: u64,
    want: u64,
) -> Result<Vec<u8>, String> {
    let url = format!("{}/file", file.base);
    let resp = client
        .get(&url)
        .query(&[
            ("share", file.share_id.as_str()),
            ("path", file.path.as_str()),
            ("offset", &offset.to_string()),
            ("len", &want.to_string()),
        ])
        .bearer_auth(&file.token)
        .send()
        .await
        .map_err(|e| format!("读取对端文件失败（share={} path={:?}）: {e}", file.share_id, file.path))?;
    if !resp.status().is_success() {
        let status = resp.status();
        // 把 share/path 与**响应体**都带上：403 只有两种来源，体里分得清清楚楚 ——
        // "Path access denied" = 路径解析不了（对端旧版本拖着"共享项根是文件"那一档时，
        // path 是空串、对端 `share_root.join("")` 多一个结尾分隔符）；
        // "Path traversal denied" = 越界。光看状态码没法区分是谁的错。
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "读取对端文件失败: HTTP {status}（share={} path={:?}）: {}",
            file.share_id,
            file.path,
            body.trim()
        ));
    }
    Ok(resp.bytes().await.map_err(|e| format!("读取对端文件中断: {e}"))?.to_vec())
}

/// 对端一条目录项。
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

/// 对端一项的元数据。
#[derive(Debug, Clone)]
pub struct Stat {
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

pub async fn fetch_tree(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<Vec<TreeEntry>, String> {
    let url = format!("{base}/tree");
    let resp = client
        .get(&url)
        .query(&[("share", share_id), ("path", path)])
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("读取对端目录失败 ({url}): {e}"))?;
    if !resp.status().is_success() {
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(format!("读取对端目录失败: {NOT_A_DIRECTORY}（{path}）"));
        }
        return Err(format!("读取对端目录失败: HTTP {}（{path}）", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("解析目录失败: {e}"))?;
    let entries = body
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "对端目录响应异常".to_string())?;
    Ok(entries
        .iter()
        .map(|entry| TreeEntry {
            name: entry.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            is_dir: entry.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false),
            size: entry.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            modified: entry.get("modified").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        })
        .collect())
}

pub async fn fetch_stat(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<Stat, String> {
    let url = format!("{base}/stat");
    let resp = client
        .get(&url)
        .query(&[("share", share_id), ("path", path)])
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("读取对端文件信息失败 ({url}): {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("读取对端文件信息失败: HTTP {}", resp.status()));
    }
    let value: serde_json::Value = resp.json().await.map_err(|e| format!("解析文件信息失败: {e}"))?;
    Ok(Stat {
        is_dir: value.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false),
        size: value.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
        modified: value.get("modified").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
    })
}

/// 一项的元数据（`/stat` 或列父目录得来的都归到这里）。
#[derive(Debug, Clone, Copy)]
pub struct EntryMeta {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// 问对端"这一项是什么、多大、什么时候改的"。
///
/// 优先 `/stat`（对端是新版本时一次请求搞定）；对端是老版本（没有 `/stat`）就退回
/// **列父目录**再挑出那一条 —— `/tree` 哪个版本都有。两条都不行返回 `None`，
/// 调用方按"大小未知"处理。
pub async fn probe_entry(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<Option<EntryMeta>, String> {
    let stat_err = match fetch_stat(client, base, token, share_id, path).await {
        Ok(stat) => {
            return Ok(Some(EntryMeta {
                is_dir: stat.is_dir,
                size: stat.size,
                mtime: parse_rfc3339_secs(&stat.modified),
            }))
        }
        Err(e) => e,
    };
    let (parent, leaf) = match path.rsplit_once('/') {
        Some((parent, leaf)) => (parent, leaf),
        None => ("", path),
    };
    if leaf.is_empty() {
        // 共享项根：没有"父目录"可列。对端是老版本（没有 /stat）时只能按调用方的判断走，
        // 大小未知 —— 描述符里不写 FD_FILESIZE，内容照样读到流末尾为止。
        warn!(%stat_err, "问不到共享项根的元数据，按调用方的判断走（大小未知）");
        return Ok(None);
    }
    // 退回"列父目录再挑出那一条"（`/tree` 哪个版本都有）。挑不到就说明这一项压根不在，
    // 这时候必须报错 —— 比如越界路径（`../x`）：不能让拖拽"看着成功了"，到最后复制才失败。
    let entries = fetch_tree(client, base, token, share_id, parent)
        .await
        .map_err(|e| format!("{stat_err}；退回列父目录也失败：{e}"))?;
    match entries.iter().find(|e| e.name == leaf) {
        Some(entry) => Ok(Some(EntryMeta {
            is_dir: entry.is_dir,
            size: entry.size,
            mtime: parse_rfc3339_secs(&entry.modified),
        })),
        None => Err(format!("对端没有这一项：{path}")),
    }
}

pub fn join_slash(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

pub fn parse_rfc3339_secs(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp())
}

/// 一段路径名：剔掉本机不接受的字符与结尾的空格/点（对端可能是 macOS，名字里会有 `:`）。
/// 整体非法（空、`.`、`..`）返回 None。
pub fn plain_component(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_end_matches([' ', '.']);
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_component_cleans_and_rejects() {
        assert_eq!(plain_component("12:30.log"), Some("12_30.log".to_string()));
        assert_eq!(plain_component("尾 点. "), Some("尾 点".to_string()));
        assert_eq!(plain_component("a\\b"), Some("a_b".to_string()), "反斜杠是分隔符，不能当名字");
        assert_eq!(plain_component(""), None);
        assert_eq!(plain_component("."), None);
        assert_eq!(plain_component(".."), None);
        assert_eq!(plain_component("   "), None);
    }

    #[test]
    fn rfc3339_round_trip_to_secs() {
        assert_eq!(parse_rfc3339_secs("2026-09-16T12:00:00+00:00"), Some(1_789_560_000));
        assert_eq!(parse_rfc3339_secs(""), None);
    }

    #[test]
    fn join_slash_keeps_the_root_empty() {
        assert_eq!(join_slash("", "a.txt"), "a.txt");
        assert_eq!(join_slash("sub", "a.txt"), "sub/a.txt");
    }
}
