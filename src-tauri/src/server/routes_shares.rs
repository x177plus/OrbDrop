use std::path::Path;
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::response::Json;
use serde_json::{json, Value};

use crate::server::auth::Caller;
use crate::state::AppState;

/// GET /shares —— 只返回「对调用者可见」的共享项（F-22 / F-25）。
///
/// 字段刻意只有 id / name / createdAt / isDir：对端要的是「能展示、能打开」，
/// 本机绝对路径（path）和广播对象（别的设备 id）都不该出这扇门。
pub async fn shares_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let shares = state.config.lock().shares.clone();
    let items: Vec<Value> = shares
        .iter()
        .filter(|s| caller.sees_share(&state, s))
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "createdAt": s.created_at,
                "isDir": looks_like_dir(&s.path),
            })
        })
        .collect();
    Json(json!({ "shares": items }))
}

/// 与前端 `LocalTab.isDir` 同一套判断（无扩展名视为目录），
/// 保证本机列表与对端设备标签里的图标/可展开性一致。
fn looks_like_dir(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s.ends_with('\\') || s.ends_with('/') {
        return true;
    }
    match s.rsplit(['\\', '/']).next() {
        Some(name) => !name.contains('.'),
        None => false,
    }
}