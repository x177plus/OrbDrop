use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::credential;
use crate::state::{AppState, LinkIntent};
use crate::transfer;
use crate::ws::events::{ui, PeerInfo, WsEvent};
use crate::ws::emit_ui;

/// Inbound WebSocket session — a paired peer connected to our /events.
///
/// We reply to pings so the initiator's heartbeat works; the peer drives
/// liveness.  Incoming events are dispatched via [`handle_inbound`].
///
/// Read and write live in ONE task on purpose: the local "断开" tears the session
/// down by dropping the event sender (`WsManager::teardown`), which makes
/// `rx_events.recv()` yield `None` — that only closes the socket if the same task
/// also owns the read half. Split into two tasks, the read loop parks on `next()`
/// forever and the peer keeps believing the long connection is alive.
pub async fn run_inbound_session(state: Arc<AppState>, peer_id: String, ws: WebSocket) {
    info!(%peer_id, "inbound WS session starting");
    state.set_peer_online(&peer_id, true);
    // 被拨号的一端同样是"处于长连接"（F-16）：重启后这一侧也要能自动接回来。
    // 同一次写入清掉「断开」的暂停 —— 能走到这里，就说明本机不是暂停态（闸门放行了）。
    state.set_link_intent(&peer_id, LinkIntent::Wanted);
    state.ws.set_link_state(&peer_id, crate::ws::LinkState::Connected);

    let (mut sink, mut stream) = ws.split();
    let (tx_events, mut rx_events) = mpsc::unbounded_channel::<WsEvent>();
    let gen = state.ws.register(&peer_id, tx_events);
    state.emit_devices_changed();
    // 链路上来了：把上一次没送出去的「已读」补一次（回执丢了，对端那条消息就永远只是「已送达」）。
    crate::chat::flush_read_receipt(&state, &peer_id);
    // 也是"对端现在收得到"这个证据：排队等它的消息该发出去了（够不着时发的那些）。
    crate::chat::flush_queued(&state, &peer_id);
    // 同理，推送那边也补一次账：回执（完成/拒绝）可能丢在断链的那一刻，而对端其实早就
    // 收完了 —— 链路刚建立就是问它的最好时机（见 `transfer::reconcile`）。
    crate::transfer::reconcile(&state, &peer_id);

    loop {
        tokio::select! {
            event = rx_events.recv() => {
                let event = match event {
                    Some(e) => e,
                    None => break, // 本机断开：发送端已被丢掉 → 关掉这条连接
                };
                let json = match serde_json::to_string(&event) {
                    Ok(s) => s,
                    Err(e) => { error!(%e, "WS serialize"); continue; }
                };
                if sink.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                            handle_inbound(&state, &peer_id, event).await;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if sink.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!(%peer_id, "inbound WS closed");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!(%peer_id, "inbound WS err: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    state.ws.unregister(&peer_id, gen);
    // 本机「断开」的暂停标记要留着（那说明"为什么现在没连着"），其余情况把这个状态交回给
    // 拨号循环（它 1s 内会写成重连中/待命）。
    if !state.ws.is_paused(&peer_id) {
        state.ws.clear_link_state(&peer_id);
    }
    // 在线状态一般不在链路断开时翻：本机"断开"不代表对端不可达（推送仍走 HTTP），
    // 可达性交给 mDNS 与探活，避免设备行在断开瞬间消失再冒出来。
    //
    // **例外是没有可拨地址的对端**：探活要有地址才能问、mDNS 也看不到它（模拟器/NAT
    // 后面的那类），这条链路是它可达的**唯一**证据。断了就该如实回到"送不到"
    // （消息排队，等它下次拨进来时 `flush_queued` 送出去）—— 不翻的话设备行会一直挂着
    // 绿点，而本机其实一条消息都送不出去。
    if state.get_peer(&peer_id).is_some_and(|p| p.ip.is_empty() || p.port == 0) {
        state.set_peer_online(&peer_id, false);
    }
    state.emit_devices_changed();
    info!(%peer_id, "inbound WS session ended");
}

/// Validate the token from a WS handshake query and return the peer id.
///
/// Returns None if the token is unknown. Our own device id is never returned:
/// two instances on one machine share a credential store, so the same token can
/// appear under both ids, and resolving the wrong one leaves the *real* peer's
/// online flag stale forever.
pub fn peer_id_from_token(state: &AppState, token: &str) -> Option<String> {
    let own_id = state.config.lock().device_id.clone();
    // `peer_tokens()` 是带版本失效的缓存版（见 `credential::peer_tokens`）：这条函数在
    // **每个** HTTP 请求上都会被调一次，原来是"读盘 + 解析 + 建表"，2 万文件的推送就是
    // 2 万次读盘。语义与 `load_peers()` 一字不差。
    credential::peer_tokens()
        .iter()
        .find(|(id, t)| t == &token && *id != &own_id)
        .map(|(id, _)| id.clone())
}

// ---------------------------------------------------------------------------
// Inbound event dispatcher
// ---------------------------------------------------------------------------

pub async fn handle_inbound(state: &Arc<AppState>, peer_id: &str, event: WsEvent) {
    match event {
        WsEvent::PeerOnline { peer } => {
            // mDNS-driven; accept as an upsert for completeness.
            state.upsert_peer(&crate::config::PeerDevice {
                id: peer.id,
                name: peer.name,
                ip: String::new(),
                port: 0,
                online: true,
                paired: true,
                auto_connect: false, paused: false,
            });
            emit_ui(state, ui::DEVICES_CHANGED, &serde_json::json!({}));
        }
        WsEvent::PeerOffline { peer_id: _ } => {
            // WS drop itself drives offline; nothing to do here.
        }
        WsEvent::PushInvite { invite_id, sender, name, size, is_dir, count, files, caps } => {
            // 身份以**这条会话**为准，不用载荷里自称的那个：长连接是 token 换来的，
            // 而载荷里的 sender.id 只是对端自己填的字符串。
            let known = state.get_peer(peer_id);
            // 已绑定（已配对）的设备：身份是这条会话用 token 换来的，无需再问用户一次。
            let paired = known.as_ref().is_some_and(|p| p.paired);
            let display_name = known
                .as_ref()
                .map(|p| p.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| sender.name.clone());
            info!(%invite_id, peer_id, sender = %display_name, paired, "inbound push invite (ws)");
            let total = if is_dir {
                files.iter().map(|f| f.size).sum::<u64>()
            } else {
                size
            };
            // 这条路的回执走 WS，地址只在"万一套用得上"时记一下（地址簿里没地址就留空）。
            let callback = known
                .as_ref()
                .and_then(|p| (!p.ip.is_empty() && p.port != 0).then(|| (p.ip.clone(), p.port)));
            state.push.insert_invite(transfer::push_job::InboundInvite {
                invite_id: invite_id.clone(),
                sender: PeerInfo { id: peer_id.to_string(), name: display_name.clone() },
                name: name.clone(),
                size,
                is_dir,
                count,
                files,
                caps,
                callback,
                responder: None,
            });
            emit_ui(state, ui::PUSH_INVITE, &serde_json::json!({
                "inviteId": invite_id,
                "sender": { "id": peer_id, "name": display_name },
                "name": name,
                "size": total,
                "isDir": is_dir,
                "count": count,
                "auto": paired,
            }));
            // 已配对的设备推送**不需要接收方同意**（token 就是那份信任）：直接开收 —
            // 界面那条通知是"正在接收"而不是"要不要收"。未绑定的才挂卡片等用户点。
            if paired {
                if let Err(reason) = transfer::accept_invite(state.clone(), &invite_id) {
                    warn!(%invite_id, %reason, "auto-accept failed");
                    // 本机那条"正在接收"的通知得收尾：这条邀请已经不可能变成完成了。
                    transfer::reject_invite_with(state, &invite_id, &reason);
                }
                return;
            }
            // 到点没处理就自动拒绝：发送方那边一直在等回执，不能只有一边作废。
            transfer::schedule_invite_timeout(state.clone(), invite_id);
        }
        WsEvent::PushResponse { invite_id, accepted } => {
            info!(%invite_id, accepted, "push response");
            use transfer::push_job::{batch_set, PushStatus, TargetState};
            if accepted {
                state.push.update_status(&invite_id, PushStatus::Accepted);
            } else {
                // 拒绝：发送方不能再等下去 —— 清掉 job，并让界面把"等待对方同意"那张卡收掉。
                state.push.remove_job(&invite_id);
                batch_set(state, &invite_id, TargetState::Rejected, None, Some("对方拒绝了这次推送".into()), true);
            }
            emit_ui(state, ui::PUSH_RESPONSE, &serde_json::json!({
                "inviteId": invite_id,
                "accepted": accepted,
            }));
            if !accepted {
                emit_ui(state, ui::PUSH_DONE, &serde_json::json!({
                    "inviteId": invite_id,
                    "status": "failed",
                    "reason": "对方拒绝了这次推送",
                }));
            }
        }
        WsEvent::PushProgress { invite_id, received, total } => {
            use transfer::push_job::{batch_set, PushStatus, TargetState};
            state.push.update_status(&invite_id, PushStatus::Downloading { received, total });
            // 纯进度：按 REPORT_INTERVAL 合并（状态跃迁才立刻发，见 `batch_set`）。
            batch_set(state, &invite_id, TargetState::Sending, Some(received), None, false);
            emit_ui(state, ui::PUSH_PROGRESS, &serde_json::json!({
                "inviteId": invite_id,
                "received": received,
                "total": total,
            }));
        }
        WsEvent::PushDone { invite_id, status, reason } => {
            info!(%invite_id, %status, "push done");
            use transfer::push_job::{batch_set, PushStatus, TargetState};
            // 发送方撤销时这条邀请可能还挂在通知列表里（对方还没点同意）：
            // 一并撤掉 —— 界面收到下面这条 push://done 就会把卡片收走。
            state.push.remove_invite(&invite_id);
            state.push.update_status(&invite_id, if status == "completed" {
                PushStatus::Completed
            } else {
                PushStatus::Failed(reason.clone().unwrap_or_default())
            });
            // 批次那一格的终态先落定，再清 job（`batch_set` 只认批次自己那份状态）。
            let target_state = match status.as_str() {
                "completed" => TargetState::Done,
                "cancelled" => TargetState::Cancelled,
                _ => TargetState::Failed,
            };
            batch_set(state, &invite_id, target_state, None, reason.clone(), true);
            state.push.remove_job(&invite_id);
            emit_ui(state, ui::PUSH_DONE, &serde_json::json!({
                "inviteId": invite_id,
                "status": status,
                "reason": reason,
            }));
        }
        WsEvent::ShareChanged { share_id, path, kind } => {
            info!(%share_id, %path, %kind, "share changed");
            emit_ui(state, ui::SHARE_CHANGED, &serde_json::json!({
                "shareId": share_id,
                "path": path,
                "kind": kind,
            }));
        }
        WsEvent::SharesChanged => {
            // 对端共享项清单变了：让前端重新拉该设备的 /shares。
            emit_ui(state, ui::SHARES_CHANGED, &serde_json::json!({ "peerId": peer_id }));
        }
        WsEvent::Unpair { peer_id: who } => {
            // 兼容旧版本对端（新版本走 POST /unpair，不依赖长连接）。
            info!(%who, "received unpair over WS");
            let _ = credential::remove_peer(&who);
            state.ws.drop_peer(&who);
            state.unpair(&who);
            emit_ui(state, ui::DEVICES_CHANGED, &serde_json::json!({}));
        }
        WsEvent::ChatText { id, text, at } => {
            // 名字以**本机设备表**为准：长连接是 token 换来的身份，载荷里没有自称的名字。
            let name = state
                .get_peer(peer_id)
                .map(|p| p.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| peer_id.to_string());
            debug!(%id, %peer_id, "inbound chat text");
            state.chat.record_inbound(peer_id, &name, &id, &text, at);
            crate::group::emit(state);
            // 界面是不是**正开着**这个会话，只有界面知道（它自己会来 `chat_mark_read`）；
            // 这里不回已读回执 —— 收到 ≠ 读过。
        }
        WsEvent::ChatRead { up_to } => {
            debug!(%peer_id, up_to, "inbound chat read receipt");
            state.chat.mark_read_by_peer(peer_id, up_to);
            crate::group::emit(state);
        }
        // 群聊三条：落账那一段在 `group.rs`（WS 与 HTTP 兜底两条路共用同一个 dispatcher，
        // 所以这里只有转发）。
        WsEvent::GroupText { group_id, id, text, at, seq, rev, author, hops, reached } => {
            debug!(%group_id, %id, %peer_id, hops, "inbound group text");
            crate::group::on_group_text(
                state,
                peer_id,
                crate::group::GroupTextIn {
                    group_id,
                    id,
                    text,
                    at,
                    seq,
                    rev,
                    sender_id: author.id,
                    sender_name: author.name,
                    hops,
                    reached,
                },
            );
        }
        WsEvent::GroupRead { group_id, up_to } => {
            debug!(%group_id, %peer_id, up_to, "inbound group read receipt");
            crate::group::on_group_read(state, peer_id, &group_id, up_to);
        }
        WsEvent::GroupRoster { roster, sender } => {
            debug!(group = %roster.id, rev = roster.rev, %peer_id, "inbound group roster");
            crate::group::on_roster(state, peer_id, roster, &sender.name);
        }
        WsEvent::GroupSyncAsk { group_id, ask_id, since_at, holes } => {
            debug!(%group_id, %ask_id, %peer_id, since_at, "inbound group sync ask");
            crate::group::on_sync_ask(state, peer_id, &group_id, &ask_id, since_at, holes);
        }
        WsEvent::GroupSyncPost { group_id, ask_id, roster, has_more, messages } => {
            debug!(%group_id, %ask_id, %peer_id, count = messages.len(), has_more, "inbound group sync post");
            crate::group::on_sync_post(state, peer_id, &group_id, roster, has_more, messages);
        }
        WsEvent::GroupAck { group_id, up_to } => {
            debug!(%group_id, %peer_id, up_to, "inbound group ack");
            crate::group::on_ack(state, peer_id, &group_id, up_to);
        }
        // 推送对账（回执丢了怎么办）：对端在问它发出去的那几条邀请现在什么状态 ——
        // 按本机记录如实答（还拿着的 `waiting`、处理过的报结局、没见过的 `unknown`）。
        WsEvent::PushAsk { ask_id, invite_ids } => {
            let answer = crate::transfer::answer_ask(state, peer_id, &ask_id, &invite_ids);
            if !state.ws.send(peer_id, answer) {
                warn!(%peer_id, %ask_id, "对账答案发不回去（会话刚断）");
            }
        }
        // 本机发出去的邀请的对账答案：这才是权威 —— 本机之前那条"超时失败"可能只是回执丢了。
        WsEvent::PushState { ask_id, states } => {
            crate::transfer::apply_push_states(state, peer_id, &ask_id, &states);
        }
    }
}