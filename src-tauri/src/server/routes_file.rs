use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tracing::error;

use crate::server::auth::Caller;
use crate::state::AppState;

/// GET /file?share=<id>&path=<relative_path>&offset=<u64>&len=<u64>
///
/// Streams file bytes. `offset` seeks into the file (default 0),
/// `len` caps the number of bytes returned (default: to EOF).
/// Supports partial reads for F-60 Range-style access.
pub async fn file_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let share_id = params.get("share").ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "Missing 'share' parameter".into())
    })?;
    let rel_path = params.get("path").ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "Missing 'path' parameter".into())
    })?;

    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let len = params.get("len").and_then(|v| v.parse::<u64>().ok());

    let share_root = {
        let config = state.config.lock();
        let share = config
            .shares
            .iter()
            // 不可见与不存在都回 404：不向对端泄露「本机有这么个共享项」。
            .find(|s| s.id == *share_id && caller.sees_share(&state, s))
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Share '{}' not found", share_id)))?;
        share.path.clone()
    };

    // 空 `path` = 共享项根自身。**不能**写成 `share_root.join("")`：那样会带上结尾分隔符
    // （`F:\x\a.txt\`），`canonicalize` 对文件立刻报 `ERROR_INVALID_NAME`(os 267) → 本 handler
    // 回 403，对端看到的就是"起拖成功、落下报未指定的错误、文件始终不出现"（拖的正是
    // "共享项根就是一个文件"这一类）。`/stat`、`/tree` 都有这条空路径判断，只有这里漏了。
    let target = if rel_path.is_empty() { share_root.clone() } else { share_root.join(rel_path) };

    // Path traversal guard: canonicalize and ensure it stays under the share root.
    let canonical_target = target.canonicalize().map_err(|e| {
        error!("Failed to canonicalize {target:?}: {e}");
        (StatusCode::FORBIDDEN, "Path access denied".into())
    })?;
    let canonical_root = share_root.canonicalize().map_err(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve share root".into())
    })?;

    if !canonical_target.starts_with(&canonical_root) {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".into()));
    }

    if !canonical_target.is_file() {
        return Err((StatusCode::BAD_REQUEST, "Path is not a file".into()));
    }

    let mut file = tokio::fs::File::open(&canonical_target).await.map_err(|e| {
        (StatusCode::NOT_FOUND, format!("File not found: {e}"))
    })?;

    let file_size = file.metadata().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read metadata: {e}")))?
        .len();

    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Seek failed: {e}")))?;
    }

    let remaining = file_size.saturating_sub(offset);
    let read_len = len.map(|l| l.min(remaining)).unwrap_or(remaining);

    let limited = file.take(read_len);
    // 帧容量只影响**分帧大小**，不影响返回的字节总数，也不动"短读 = 到末尾"那条判据：
    // 对端（含 CFAPI 镜像水合那条路）一次 `len` 要多少还是拿多少。
    let stream = ReaderStream::with_capacity(limited, crate::net::STREAM_CHUNK);
    Ok(Body::from_stream(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ShareItem};

    /// 共享项根**就是一个文件**时，空 `path` 必须当"根自身"。
    ///
    /// 回归的是这条链：`share_root.join("")` 会带上结尾分隔符（`F:\x\a.txt\`），
    /// `canonicalize` 对文件报 `ERROR_INVALID_NAME`(os 267) → 本 handler 回 403。
    /// 拖拽下载拖的正是"共享项根就是一个文件"这一类（`path` 为空），于是对端表现为
    /// 起拖成功、落下报"未指定的错误"、文件始终不出现。
    #[tokio::test]
    async fn file_root_share_is_served_with_an_empty_path() {
        let dir = std::env::temp_dir().join(format!("orbdrop_file_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("single.txt");
        std::fs::write(&file, b"hello from a file share").unwrap();

        let mut config = Config::default();
        let share = ShareItem::new(file.clone()); // 共享项根指向一个文件
        let share_id = share.id.clone();
        config.shares.push(share);
        let config_path =
            std::env::temp_dir().join(format!("orbdrop_file_cfg_{}.toml", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState::new(config, config_path));
        let port = crate::server::start_server(state.clone(), 4070).await.expect("起服务");

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/file"))
            .query(&[("share", share_id.as_str()), ("path", ""), ("offset", "0")])
            .send()
            .await
            .expect("请求");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "空 path 要落到共享项根自身，不能 403"
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"hello from a file share");

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}