
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
    let mut items = Vec::new();
    for s in shares.iter().filter(|s| caller.sees_share(&state, s)) {
        // 真去问文件系统。早先用"名字里有没有点"猜（无扩展名算目录），猜错的代价不只是
        // 图标：对端按这个 `isDir` 决定"展开成树"还是"当成一个文件"，一旦把文件当成目录，
        // 展开就是一句 `Path is not a directory`（拖拽同样会被当成目录走）。
        let is_dir = tokio::fs::metadata(&s.path)
            .await
            .map(|m| m.is_dir())
            // 拿不到元数据（文件被删/无权限）时按"文件"处理：文件拖得动，目录也没有
            // "点开才报错"的尴尬。
            .unwrap_or(false);
        items.push(json!({
            "id": s.id,
            "name": s.name,
            "createdAt": s.created_at,
            "isDir": is_dir,
        }));
    }
    Json(json!({ "shares": items }))
}