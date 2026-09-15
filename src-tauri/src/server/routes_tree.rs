use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Serialize;
use tracing::error;

use crate::server::auth::Caller;
use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

#[derive(Debug, Serialize)]
pub struct TreeResponse {
    pub entries: Vec<TreeEntry>,
}

/// GET /tree?share=<id>&path=<relative_path>
///
/// Returns a one-level directory listing for the given share and path.
pub async fn tree_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<TreeResponse>, (StatusCode, String)> {
    let share_id = params.get("share").ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "Missing 'share' query parameter".into())
    })?;
    let rel_path = params.get("path").map(|s| s.as_str()).unwrap_or("");

    // 不可见与不存在都回 404：不向对端泄露「本机有这么个共享项」。
    let share = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .find(|s| s.id == *share_id && caller.sees_share(&state, s))
            .cloned()
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Share '{}' not found", share_id)))?
    };
    let share_root = share.path;

    let target = if rel_path.is_empty() {
        share_root.clone()
    } else {
        share_root.join(rel_path)
    };

    // Path traversal guard: canonicalize and ensure it starts with share_root
    let canonical_target = target.canonicalize().map_err(|e| {
        error!("Failed to canonicalize {target:?}: {e}");
        (StatusCode::FORBIDDEN, "Path access denied".into())
    })?;

    let canonical_root = share_root.canonicalize().map_err(|e| {
        error!("Failed to canonicalize root {share_root:?}: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve share root".into())
    })?;

    if !canonical_target.starts_with(&canonical_root) {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".into()));
    }

    if !canonical_target.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "Path is not a directory".into()));
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(&canonical_target).await.map_err(|e| {
        (StatusCode::FORBIDDEN, format!("无权访问: {e}"))
    })?;

    while let Some(entry) = read_dir.next_entry().await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Read error: {e}"))
    })? {
        if let Ok(metadata) = entry.metadata().await {
            let is_dir = metadata.is_dir();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files on Unix (not needed on Windows, harmless)
            if name.starts_with('.') {
                continue;
            }

            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| {
                    let duration = t
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?;
                    let secs = duration.as_secs() as i64;
                    Some(chrono::DateTime::from_timestamp(secs, 0)?.to_rfc3339())
                })
                .unwrap_or_default();

            entries.push(TreeEntry {
                name,
                is_dir,
                size: metadata.len(),
                modified,
            });
        }
    }

    // Sort: directories first, then by name
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(Json(TreeResponse { entries }))
}