use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::response::Json;
use serde_json::{json, Value};

use crate::server::auth::Caller;
use crate::state::AppState;

/// GET /shares —— 只返回「对调用者可见」的共享项（F-22 / F-25）。
///
/// 字段刻意只有 id / name / createdAt / isDir / size / itemCount：对端要的是「能展示、能打开」，
/// 本机绝对路径（path）和广播对象（别的设备 id）都不该出这扇门。
///
/// `size` 只对**文件**有意义，`itemCount` 只对**目录**有意义（且是**直接**子项数）——另一个恒为
/// null。两个数都拿不到时也是 null：界面宁可只说「文件夹 / 文件」，也不报一个没根据的数字。
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
        let meta = tokio::fs::metadata(&s.path).await.ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        // 目录的大小要递归遍历，这里不给（对端设备页只是列清单）；文件的大小是现成的。
        let size = meta.as_ref().filter(|m| !m.is_dir()).map(|m| m.len());
        let item_count = if is_dir {
            count_children(&s.path).await
        } else {
            None
        };
        items.push(json!({
            "id": s.id,
            "name": s.name,
            "createdAt": s.created_at,
            "isDir": is_dir,
            "size": size,
            "itemCount": item_count,
        }));
    }
    Json(json!({ "shares": items }))
}

/// 目录的直接子项数。**跳过 `.` 开头的项**：对端点开这个共享项时走的是 `/tree`，那里也只有
/// 这一条过滤（`routes_tree.rs`）—— 卡片上的数与展开后的行数对不上，用户会以为漏了东西。
///
/// 中途读错（权限、掉盘）返回 `None`：少一个数比报一个错的数好。
async fn count_children(path: &std::path::Path) -> Option<u64> {
    let mut read_dir = tokio::fs::read_dir(path).await.ok()?;
    let mut n = 0u64;
    loop {
        match read_dir.next_entry().await {
            Ok(Some(entry)) => {
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    n += 1;
                }
            }
            Ok(None) => return Some(n),
            Err(_) => return None,
        }
    }
}
