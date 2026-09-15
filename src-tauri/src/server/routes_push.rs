use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tokio_util::io::ReaderStream;

use crate::state::AppState;

/// GET /push/download?id=<invite_id>&path=<rel_path>&offset=<u64>
///
/// Streams the requested file from the sender's source directory, starting at
/// `offset`.  This is the HTTP Range-style endpoint used by the receiver's
/// download loop (F-52 / F-60).
///
/// Returns 410 Gone if the job was cancelled or doesn't exist.
pub async fn push_download_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let id = params.get("id").ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "Missing 'id' parameter".into())
    })?;
    let file_path = params.get("path").ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "Missing 'path' parameter".into())
    })?;
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    // Look up the job
    let job = {
        let jobs = state.push.jobs.lock();
        jobs.get(id).cloned()
    };

    let (source_path, _files, _total) = match job {
        Some(j) => {
            let path = j.source_dir.join(file_path.trim_start_matches('/'));
            (path, j.files, j.total_size)
        }
        None => {
            // Check if job was cancelled/invalid
            return Err((StatusCode::GONE, "推送不存在或已取消".into()));
        }
    };

    // Path traversal guard
    let canonical = match source_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::NOT_FOUND, "文件不存在".into())),
    };

    if !canonical.is_file() {
        return Err((StatusCode::NOT_FOUND, "文件不存在".into()));
    }

    let mut file = tokio::fs::File::open(&canonical).await.map_err(|e| {
        (StatusCode::NOT_FOUND, format!("文件打开失败: {e}"))
    })?;

    let file_size = file
        .metadata()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("元数据读取失败: {e}")))?
        .len();

    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Seek失败: {e}")));
        }
    }

    use tokio::io::AsyncReadExt;
    let remaining = file_size.saturating_sub(offset);
    let limited = file.take(remaining);
    let stream = ReaderStream::new(limited);
    Ok(Body::from_stream(stream))
}