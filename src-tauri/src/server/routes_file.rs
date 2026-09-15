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

    let target = share_root.join(rel_path);

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
    let stream = ReaderStream::new(limited);
    Ok(Body::from_stream(stream))
}