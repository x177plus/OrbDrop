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
    // 队列**有界**（容量与理由见 `crate::ws::EVENT_QUEUE_CAPACITY`）：对端不再读时，
    // 写那一臂会卡住、`rx` 再没人 poll —— 队列满了由 `WsManager::send` 掐掉这条会话
    // （丢掉 sender = 下面的 `rx.recv()` 立刻拿到 `None` = 本机"断开"的既有收尾），
    // 而不是在无界队列里把本机内存堆满。消费方这一侧不需要任何等待/丢弃逻辑。
    let (tx_events, mut rx_events) = mpsc::channel::<WsEvent>(crate::ws::EVENT_QUEUE_CAPACITY);
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
                            // 回执不在这里处理：拒收那条回话由 `group::reject_text` 当场发出去。
                            let _ = handle_inbound(&state, &peer_id, event).await;
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
    //
    // 这一条同样要看"刚听到过它"：链路断掉那一刻它可能刚发来一条消息（那正是它这一侧
    // 还活着的证据，见 `AppState::heard`），那就等探活在 `HEARD_TTL` 之后如实落回来，
    // 而不是断链这一下就把它画成离线。
    if state.get_peer(&peer_id).is_some_and(|p| p.ip.is_empty() || p.port == 0) {
        state.set_peer_offline_unless_heard(&peer_id);
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

/// 一条入站事件落账之后要给对方的回执。**收不下**的有两条：群消息，与图片消息的内容：
///
/// - HTTP 兜底那一跳按它回状态码（`Accepted` = 204，`GroupRefused` = 409 + 理由，
///   `ImageRefused` = 400）；
/// - 长连接那一条路的回话已经由 `group::reject_text` 当场发了（`GroupTextRejected`）；
///   图片那一条**没有回话通道**（长连接上的入站事件是单向的），只能记日志 —— 发送方
///   那边那条消息照样显示"已送达"，而本机确实没记下它（见 `chat::accept_inbound_image`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum InboundResult {
    /// 收下了（或这条事件本来就没有回执可言）。
    Accepted,
    /// 群消息收不下：理由代码见 `crate::group::REASON_*`。
    GroupRefused(&'static str),
    /// 图片消息的内容收不下（base64 坏了 / 超过上限 / 写盘失败）：本机**没有**记下这条消息。
    ImageRefused,
}

pub async fn handle_inbound(state: &Arc<AppState>, peer_id: &str, event: WsEvent) -> InboundResult {
    // 入站证据的**唯一落点**（长连接与 HTTP 那几条会话路由都汇到这里）：能从它那边收到东西，
    // 就是"它在网上"——而本机拨不拨得回去是另一回事（AP 隔开单播、跨段、只有借来的中转），
    // 见 `AppState::heard`。落在这里而不是每个臂里：少一处漏一处，而漏掉的那一处表现就是
    // "会说话的设备画成离线"。
    state.note_heard_online(peer_id);
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
                auto_connect: false, paused: false, relay: None,
            });
            emit_ui(state, ui::DEVICES_CHANGED, &serde_json::json!({}));
        }
        WsEvent::PeerOffline { peer_id: _ } => {
            // WS drop itself drives offline; nothing to do here.
        }
        WsEvent::PushInvite { invite_id, sender, name, size, is_dir, count, files, caps, at } => {
            // 身份以**这条会话**为准，不用载荷里自称的那个：长连接是 token 换来的，
            // 而载荷里的 sender.id 只是对端自己填的字符串。
            //
            // 名字反过来了：载荷里那个**是**要收的 —— 而这条连接是 token 换来的身份，
            // 这个名字算**核实过**的（`note_peer_name`：对端改名跟着走）。少了这一步，
            // 下面那句"设备表优先"取到的是占位名（= 设备 id），一张推送卡上就会顶着一串 deviceid。
            state.note_peer_name(peer_id, &sender.name);
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
                // 已配对设备的推送免同意自动收，没有"先挑目录"的时机 → 落盘根恒为下载目录。
                root: None,
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
                // 这条文件消息在会话时间线上的位置（发送方点的，见 `WsEvent::PushInvite::at`）。
                "at": at,
            }));
            // 已配对的设备推送**不需要接收方同意**（token 就是那份信任）：直接开收 —
            // 界面那条通知是"正在接收"而不是"要不要收"。未绑定的才挂卡片等用户点。
            if paired {
                if let Err(reason) = transfer::accept_invite(state.clone(), &invite_id, None) {
                    warn!(%invite_id, %reason, "auto-accept failed");
                    // 本机那条"正在接收"的通知得收尾：这条邀请已经不可能变成完成了。
                    transfer::reject_invite_with(state, &invite_id, &reason);
                }
                return InboundResult::Accepted;
            }
            // 到点没处理就自动拒绝：老对端那边一直挂着等回执，不能只有一边作废。
            // **带 `await1` 的对端不排这个超时**（守卫在 `schedule_invite_timeout` 里）：
            // 它等的是"报告"，两边都没有时钟。
            transfer::schedule_invite_timeout(state.clone(), invite_id);
        }
        WsEvent::PushResponse { invite_id, accepted, reason } => {
            info!(%invite_id, accepted, "push response");
            use transfer::push_job::{batch_set, PushStatus, TargetState};
            // 对端给了原因就用它的：「对方拒绝」与「本机收不下（地址未知之类）」是两件事，
            // 卡片上那句话该说准（老对端不带 reason → 回到原来那句）。
            let why = reason.unwrap_or_else(|| "对方拒绝了这次推送".into());
            if accepted {
                state.push.update_status(&invite_id, PushStatus::Accepted);
            } else {
                // 拒绝：发送方不能再等下去 —— 清掉 job，并让界面把"等待对方同意"那张卡收掉。
                state.push.remove_job(&invite_id);
                batch_set(state, &invite_id, TargetState::Rejected, None, Some(why.clone()), true);
            }
            emit_ui(state, ui::PUSH_RESPONSE, &serde_json::json!({
                "inviteId": invite_id,
                "accepted": accepted,
                "reason": why,
            }));
            if !accepted {
                emit_ui(state, ui::PUSH_DONE, &serde_json::json!({
                    "inviteId": invite_id,
                    "status": "failed",
                    "reason": why,
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
            // 发送方说这条结束了 → 接收端那份"待续传任务"一并作废（重启后不该再自己开始收）。
            // 发送方这边没有这一份，这行是个空操作。
            state.push.resume.drop_pending(&invite_id);
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
            // **这条是流量，不是新闻**：对端一次构建能在 300ms 的窗口里发来几百条逐文件事件
            // （实测 1300 条/秒），每一条都记一条日志、发一次 Tauri 事件，前端就每一条重拉一次
            // 整层树。按共享项**前缘节流**（见 `AppState::share_changed_due`）：窗口内第一条
            // 立刻照旧走完全程，之后 300ms 内的重复只记一条 `debug` 就丢掉。
            // 日志也随之降到"每次真的上报一条"，RUST_LOG=debug 时才看得到。
            if !state.share_changed_due(&share_id) {
                debug!(%share_id, %path, %kind, "share changed（窗口内的重复，已合并）");
                return InboundResult::Accepted;
            }
            debug!(%share_id, %path, %kind, "share changed");
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
            //
            // **只用会话身份**（`peer_id` 是这条连接用 token 换来的），报文里那个 `who`
            // 一律不采信：旧对端发的就是它自己，一致；不一致说明这一帧想借别人的连接
            // 去解掉第三方的配对（删凭据 + 断链 + 移出地址簿），那是越权，丢掉。
            if who != peer_id {
                warn!(%peer_id, claimed = %who, "WS unpair 的自称 id 与连接身份不一致，忽略");
                return InboundResult::Accepted;
            }
            info!(%peer_id, "received unpair over WS");
            let _ = credential::remove_peer(peer_id);
            state.ws.drop_peer(peer_id);
            state.unpair(peer_id);
            emit_ui(state, ui::DEVICES_CHANGED, &serde_json::json!({}));
        }
        WsEvent::ChatText { id, text, at, from_name } => {
            // 名字：载荷里那个自报的名字算**核实过**的（这条连接是 token 换来的身份）→
            // 直接更新设备行（`note_peer_name`：对端改名跟着走），再从表里取。表里那份是权威
            // （本机备注 F-101 走的是另一个字段）。以前这里没有 `from_name`，取到的一直是
            // 占位名（= 设备 id），还被原样写进会话记录。
            state.note_peer_name(peer_id, &from_name);
            let name = state
                .get_peer(peer_id)
                .map(|p| p.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| peer_id.to_string());
            debug!(%id, %peer_id, "inbound chat text");
            let (fresh, _) = state.chat.record_inbound(peer_id, &name, &id, &text, at);
            crate::group::emit(state);
            // 手机上人没看着屏幕时，这条通知才是他知道"有人在说话"的唯一途径（进程活着、
            // 界面可能早就没了）—— 见 `notify` 的说明。重复投递不再响一遍。
            if fresh {
                crate::notify::message(&crate::notify::Convo::Direct(peer_id.to_string()), &name, &text).await;
            }
            // 界面是不是**正开着**这个会话，只有界面知道（它自己会来 `chat_mark_read`）；
            // 这里不回已读回执 —— 收到 ≠ 读过。
        }
        WsEvent::ChatImage { id, at, name, size: _, data, from_name } => {
            // 与 `ChatText` 同一个口径：载荷里的名字算核实过的（token 换来的身份）→ 直接更新。
            state.note_peer_name(peer_id, &from_name);
            let sender = crate::group::display_name(state, peer_id, &from_name);
            debug!(%id, %peer_id, name = %name, "inbound chat image");
            // 内容先落盘再记账（见 `chat::accept_inbound_image`）：落不下来就不记。
            match crate::chat::accept_inbound_image(state, peer_id, &sender, &id, &name, &data, at) {
                Ok(fresh) => {
                    crate::group::emit(state);
                    if fresh {
                        // 通知里写的是一句"发了张图"：图片本身不必进通知（手机上那条通知
                        // 只有一行字的位置）。重复投递不再响一遍。
                        crate::notify::message(
                            &crate::notify::Convo::Direct(peer_id.to_string()),
                            &sender,
                            "[图片]",
                        )
                        .await;
                    }
                }
                Err(reason) => {
                    // 长连接这一跳没有回执通道可以说这件事：如实记一行，那条消息于是不在本机
                    // 会话里出现（"至少一次"投递的固有缺口 —— 宁可漏一条，也不留一条打不开的空图。
                    // HTTP 那一跳会把这个原因回给发送方，那边落「未送达」）。
                    warn!(%id, %peer_id, %name, %reason, "收到一条图片消息但内容没能落下来");
                    return InboundResult::ImageRefused;
                }
            }
        }
        WsEvent::ChatRead { up_to } => {
            debug!(%peer_id, up_to, "inbound chat read receipt");
            state.chat.mark_read_by_peer(peer_id, up_to);
            crate::group::emit(state);
        }
        // 群聊三条：落账那一段在 `group.rs`（WS 与 HTTP 兜底两条路共用同一个 dispatcher，
        // 所以这里只有转发）。
        WsEvent::GroupText { group_id, id, text, at, seq, rev, author, hops, reached, image, name, size, data } => {
            debug!(%group_id, %id, %peer_id, hops, image, "inbound group text");
            // 通知里要"哪个群、谁说的"，这两样都得在 `author` 被搬进事件里之前留一份
            // （发言人那半截用 `group::display_name`：与落账写进消息里的**同一个名字**）。
            let notify_author = author.id.clone();
            let notify_claimed = author.name.clone();
            // 拒收回执要用的两样：`id` 与 `group_id` 马上会被搬进 `GroupTextIn`。
            let (nack_group, nack_msg) = (group_id.clone(), id.clone());
            let outcome = crate::group::on_group_text(
                state,
                peer_id,
                crate::group::GroupTextIn {
                    group_id: group_id.clone(),
                    id,
                    text: text.clone(),
                    at,
                    seq,
                    rev,
                    sender_id: author.id,
                    sender_name: author.name,
                    hops,
                    reached,
                    image,
                    name,
                    size,
                    data,
                },
            );
            match outcome {
                crate::group::GroupTextOutcome::Recorded => {
                    // 群名给标题（没起名的那一档界面也是回落到成员名，这里退回中性的一档）、
                    // 发言人给正文。
                    let title = state
                        .groups
                        .group(&group_id)
                        .map(|g| g.name.trim().to_string())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "群聊".to_string());
                    let who = crate::group::display_name(state, &notify_author, &notify_claimed);
                    // 图片消息在通知里写「[图片]」（手机那条通知只有一行字的位置，
                    // 而这里的 `text` 对图片来说是空串）。
                    let body = if image { "[图片]".to_string() } else { text.clone() };
                    crate::notify::message(
                        &crate::notify::Convo::Group(group_id),
                        &title,
                        &format!("{who}：{body}"),
                    )
                    .await;
                }
                // 邀请卡还挂着（消息已经攒下了）：不响通知 —— 用户还没点「加入」，
                // 此刻响一声就是"我没进这个群，却有人跟我说话"。
                crate::group::GroupTextOutcome::Buffered => {}
                // 收不下：**当场明说**（长连接这条路的"写成功"不能当真），并把理由交给
                // HTTP 那一跳（见 `routes_chat`：同一件事回 409，发送方那边两条路都落得下）。
                refused => {
                    crate::group::reject_text(state, peer_id, &nack_group, &nack_msg, refused.reason());
                    return InboundResult::GroupRefused(refused.reason());
                }
            }
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
        // 群聊：对端说"这一条我收不下"（长连接那条路的"写成功 = 已送达"要由它来纠正）。
        WsEvent::GroupTextRejected { group_id, id, reason } => {
            debug!(%group_id, %id, %peer_id, %reason, "inbound group text rejected");
            crate::group::on_group_text_rejected(state, peer_id, &group_id, &id, &reason);
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
    // 走到这里 = 这一条收下了（群消息那一条收不下时在上面就返回了）。
    InboundResult::Accepted
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn state(tag: &str) -> Arc<AppState> {
        let path = std::env::temp_dir().join(format!("orbdrop_ws_{tag}_{}.toml", uuid::Uuid::new_v4()));
        Arc::new(AppState::new(crate::config::Config::default(), path))
    }

    /// WS 上的 `Unpair` 只认**连接自己的身份**：报文里那个 id 一律不采信。
    ///
    /// 旧对端发的就是它自己（一致 → 照旧生效）；不一致的那一帧是在借别人的连接去解掉
    /// 第三方的配对（删凭据 + 断链 + 移出地址簿）—— 必须丢掉。
    #[tokio::test]
    async fn unpair_over_ws_only_acts_on_the_session_identity() {
        let st = state("unpair");
        crate::credential::set_peer_credential("device-attacker", "tok-a", &"a".repeat(64)).unwrap();
        crate::credential::set_peer_credential("device-victim", "tok-v", &"b".repeat(64)).unwrap();
        let victim_seen = st.get_peer("device-victim").is_some();

        // 攻击者拿自己的连接，声称要解掉"受害者"。
        let _ = handle_inbound(
            &st,
            "device-attacker",
            WsEvent::Unpair { peer_id: "device-victim".into() },
        )
        .await;
        assert!(
            crate::credential::get_peer_token("device-victim").unwrap().is_some(),
            "第三方的凭据不许被别人的一帧解掉"
        );
        assert_eq!(
            st.get_peer("device-victim").is_some(),
            victim_seen,
            "第三方的地址簿条目也不该被动"
        );

        // 自己的 id（旧对端的正常写法）照旧生效。
        let _ = handle_inbound(
            &st,
            "device-attacker",
            WsEvent::Unpair { peer_id: "device-attacker".into() },
        )
        .await;
        assert!(
            crate::credential::get_peer_token("device-attacker").unwrap().is_none(),
            "自称与连接身份一致时按旧语义解掉"
        );

        crate::credential::remove_peer("device-victim").ok();
    }

    /// 入站一条会话消息要做两件事：把**占位名**换成它自报的真名（那是"先显示一串 deviceid"
    /// 的根因），并把"听到过它"落成在线（本机拨不回去的那些靠这条证据，见 `AppState::heard`）。
    #[tokio::test]
    async fn an_inbound_chat_fills_the_placeholder_name_and_marks_it_online() {
        let st = state("chat_name");
        // 这一行正是"跨段 / 经中转"那类的样子：拿 id 顶着名字、没有地址、离线。
        st.observe_verified_addr("dev-hw", "10.20.30.40", 3890);
        assert_eq!(st.get_peer("dev-hw").unwrap().name, "dev-hw");
        assert!(!st.get_peer("dev-hw").unwrap().online);

        let _ = handle_inbound(
            &st,
            "dev-hw",
            WsEvent::ChatText {
                id: "m1".into(),
                text: "你好".into(),
                at: 42,
                from_name: "小米 15".into(),
            },
        )
        .await;

        let peer = st.get_peer("dev-hw").unwrap();
        assert_eq!(peer.name, "小米 15", "占位名要被载荷里那个真名填上");
        assert!(peer.online, "刚听到过它 = 它在网上（哪怕本机拨不动它）");
        let convo = st
            .chat
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "dev-hw")
            .expect("这条消息该被记下来");
        assert_eq!(
            convo.messages[0].msg.from_name, "小米 15",
            "会话记录里那条也要是真名（以前写进去的是那串 deviceid）"
        );

        // 它自己**又报了一个新名字**（同一条 token 会话上）：跟着更新 —— 设备行的名字是对端
        // 自己的名字，最新的自报就作数（用户自己的命名走**备注** F-101，不在这一格）。
        // 老规矩（"真名一个字都不动"）在这里是反的：那会让一台改了名的手机永远停在旧名字。
        let _ = handle_inbound(
            &st,
            "dev-hw",
            WsEvent::ChatText {
                id: "m2".into(),
                text: "再一句".into(),
                at: 43,
                from_name: "小米 15 Pro".into(),
            },
        )
        .await;
        assert_eq!(st.get_peer("dev-hw").unwrap().name, "小米 15 Pro");
    }
}
