//! `GET /history` —— 共享项设备把自己的更新记录给可见对端看。
//!
//! 记录只存在共享项设备上：只有它知道谁在写（`/dav` token / `/push/upload` sender /
//! 本机 watcher）。其他设备不维护副本，右键「更新记录」时来问这里。
//!
//! 可见性与 `/tree` 完全一致：共享项存在、开着广播、调用方此刻处于长连接（`Caller::sees_share`）。
//! 返回的 `mine` 是相对**查询方**算的：`actor_id == 调用方 id` 即“本机”。

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Serialize;

use crate::server::auth::Caller;
use crate::share::history::{HistoryView, MAX_QUERY_LIMIT};
use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct HistoryParams {
    pub share: String,
    #[serde(default)]
    pub path: String,
    /// `1` / `true` = 目录视图（返回前缀下每个文件的最新一条）。
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<HistoryView>,
}

pub async fn history_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    if !crate::ids::is_segment(&params.share) {
        return Err((StatusCode::BAD_REQUEST, "共享项标识不合法".into()));
    }
    let visible = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .any(|share| share.id == params.share && caller.sees_share(&state, share))
    };
    if !visible {
        return Err((StatusCode::NOT_FOUND, "Share not found".into()));
    }
    if !crate::server::safe_relative_share_path(&params.path) {
        return Err((StatusCode::FORBIDDEN, "Path access denied".into()));
    }
    let is_dir = params
        .dir
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
    let limit = params.limit.unwrap_or(200).clamp(1, MAX_QUERY_LIMIT);
    // `mine` 相对调用方：对端调用方是它自己的 id；本机回环调用方是本机 device id。
    let caller_id = match &caller {
        Caller::Peer(peer_id) => peer_id.clone(),
        Caller::Local => state.own_id().to_string(),
    };
    let entries = if is_dir {
        state
            .history
            .query_dir(&params.share, &params.path, limit)
    } else {
        state
            .history
            .query_file(&params.share, &params.path, limit)
    };
    Ok(Json(HistoryResponse {
        entries: entries
            .iter()
            .map(|entry| HistoryView::from_entry(entry, &caller_id))
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_share() -> (Arc<AppState>, String) {
        let root = std::env::temp_dir().join(format!(
            "orbdrop_history_route_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/a.txt"), b"a").unwrap();
        let mut config = crate::config::test_config();
        let mut share = crate::config::ShareItem::new(root);
        share.broadcast_enabled = true;
        let share_id = share.id.clone();
        config.shares.push(share);
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!(
                "orbdrop_history_route_cfg_{}.toml",
                uuid::Uuid::new_v4()
            )),
        ));
        (state, share_id)
    }

    #[tokio::test]
    async fn history_is_visible_to_a_linked_peer_and_marks_its_own_entries() {
        let (state, share_id) = state_with_share();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::ws::events::WsEvent>(4);
        state.ws.register("peer-1", tx);
        state.history.record(
            &share_id,
            "dir/a.txt",
            "modified",
            None,
            state.own_id(),
            "本机",
        );
        state.history.record(
            &share_id,
            "dir/b.txt",
            "modified",
            None,
            "peer-1",
            "对端",
        );
        let response = history_handler(
            State(state.clone()),
            Extension(Caller::Peer("peer-1".into())),
            Query(HistoryParams {
                share: share_id.clone(),
                path: "dir".into(),
                dir: Some("1".into()),
                limit: Some(10),
            }),
        )
        .await
        .unwrap();
        let entries = response.0.entries;
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == "dir/b.txt" && entry.mine),
            "peer-1 自己的写入应标成本机"
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == "dir/a.txt" && !entry.mine),
            "共享方自己的写入对 peer-1 应标成对端"
        );
        // 测试临时目录由系统清理；这里只断言接口行为。
    }
}