use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Serialize;

use crate::server::auth::Caller;
use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatResponse {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

/// GET /stat?share=<id>&path=<relative_path>
///
/// 一项（文件或目录）的元数据。
///
/// 拖拽下载（F-46）要它：拖单个**文件**出去时，拖放数据对象得先报出这个文件的大小与
/// 修改时间（`FILEDESCRIPTOR`），而 `/tree` 只列目录、对"共享项根就是一个文件"的情况
/// 直接回 400 —— 那条路上没有别的尺寸来源。
pub async fn stat_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<StatResponse>, (StatusCode, String)> {
    let share_id = params.get("share").ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "Missing 'share' query parameter".into(),
        )
    })?;
    let rel_path = params.get("path").map(|s| s.as_str()).unwrap_or("");

    // 不可见与不存在都回 404：不向对端泄露「本机有这么个共享项」。
    let share_root = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .find(|s| s.id == *share_id && caller.sees_share(&state, s))
            .map(|s| s.path.clone())
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("Share '{share_id}' not found"),
                )
            })?
    };

    if !crate::server::safe_relative_share_path(rel_path) {
        return Err((StatusCode::FORBIDDEN, "Path access denied".into()));
    }

    let target = if rel_path.is_empty() {
        share_root.clone()
    } else {
        share_root.join(rel_path)
    };
    let canonical = target
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, "路径不存在".to_string()))?;
    let canonical_root = share_root.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法解析共享项根目录".to_string(),
        )
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err((StatusCode::FORBIDDEN, "路径越界".into()));
    }

    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取元数据失败: {e}")))?;

    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| {
            let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some(chrono::DateTime::from_timestamp(secs, 0)?.to_rfc3339())
        })
        .unwrap_or_default();

    Ok(Json(StatResponse {
        name: canonical
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        is_dir: metadata.is_dir(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        modified,
    }))
}
