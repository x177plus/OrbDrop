//! 会话（聊天）：文本消息的本地真相 + 给界面的快照。
//!
//! 线上协议只有两条（`WsEvent::ChatText` / `WsEvent::ChatRead`，见 `ws::events`）——
//! 这一层不碰网络，只管"谁跟谁说过什么、哪条到没到、哪条已被对方读过"。
//!
//! **为什么落盘**：会话记录是用户数据（跟共享项、地址簿同一档），放在 `config.toml`
//! 旁边的 `chat.json` 里，重启后还在。不塞进 `config.toml`：那是"配置"，不该被聊天
//! 记录撑大，也不该因为手改一次配置被连带丢掉。
//!
//! **模型**：消息按**对端设备**存（`peer_id → 消息`），**没有群这个实体**。
//! 「群发」是一次投递动作：给选中的每台各存一份（**同一个消息 id**，各带各的送达状态），
//! 于是 A 群发给 B、C 之后，A 的会话列表里就是「与 B」「与 C」两条，各自带着这条消息；
//! 逐台状态由同一个 id 的几份副本拼出来（"送到了几台"）。对端收到的永远是一条普通消息
//! （它不知道也不需要在"某个群"里），回复各自回到与发送方的 1:1 —— 这正是本项目网络
//! 拓扑（两两配对、无组播）下唯一诚实的语义。
//!
//! **已读水位**：每台设备一条 `read_at`，`at <= read_at` 的**对端消息**算读过，未读数由
//! 它算出来（不逐条记"已读"）。水位用的 `at` 是**发送方**的时钟，回执原样带回去，
//! 所以只在自己的时钟域里比较，两端不需要对表。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::{data_dir, write_atomic};
use crate::state::AppState;

/// 每台设备最多留多少条：文本消息很小，但"会话"不该无限长下去。
/// 超了从头砍 —— 会话列表与详情都是看最近的，砍掉的是最够不着的那一头。
const MAX_PER_PEER: usize = 500;

/// 一条消息在一台设备上的状态（本机视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    /// 本机发出，已写进这条长连接（TCP 层面出去了）。
    Sent,
    /// 对端读过（收到它的 `ChatRead` 回执）。
    Read,
    /// 没送出去：发的时候对端没有长连接（或写出失败）。
    Failed,
    /// 收到的消息：本机视角没有"送达"可言。
    Received,
}

/// 会话里的一条文本消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// 消息 id 由发送方生成；群发的每台副本共用同一个 id（前端据此合并成一条）。
    pub id: String,
    pub from_me: bool,
    /// 对端消息的发送方名字（群视图要写"谁说的"）；本机发出的为空。
    pub from_name: String,
    pub text: String,
    /// 发送方的墙上时间（epoch 毫秒）：排序与已读水位都按它。
    pub at: u64,
    pub delivery: Delivery,
    /// 本机发出时这条消息的目标设备清单（群发时每份副本都带着它，界面上才能写"已发送至 N 台"）。
    pub targets: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerConvo {
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    read_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatFile {
    #[serde(default)]
    convos: HashMap<String, PeerConvo>,
}

/// 一台设备的会话，给界面看的那份。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerConvoView {
    pub peer_id: String,
    pub messages: Vec<ChatMessage>,
    pub read_at: u64,
    pub unread: u32,
}

/// 整份会话状态（命令返回 + `chat://changed` 事件的载荷）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub convos: Vec<PeerConvoView>,
    /// 全部未读（底栏那颗角标）。
    pub unread: u32,
}

pub struct ChatStore {
    path: PathBuf,
    inner: Mutex<ChatFile>,
}

impl ChatStore {
    pub fn new() -> Self {
        Self::with_path(data_dir().join("chat.json"))
    }

    /// 指定文件的构造（单测用：不碰真实数据目录）。
    pub fn with_path(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<ChatFile>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// 现在这一刻的快照。
    pub fn snapshot(&self) -> Snapshot {
        let guard = self.inner.lock();
        let mut views: Vec<PeerConvoView> = guard
            .convos
            .iter()
            .map(|(peer_id, c)| PeerConvoView {
                peer_id: peer_id.clone(),
                messages: c.messages.clone(),
                read_at: c.read_at,
                unread: unread_of(c),
            })
            .collect();
        // 稳定顺序：界面上"哪台有未读"不该随机跳动（会话列表自己按最近一条排）。
        views.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        let unread = views.iter().map(|v| v.unread).sum();
        Snapshot { convos: views, unread }
    }

    /// 本机发出一条消息：每台目标各存一份（同一个 id），送达状态逐台记。
    ///
    /// `targets` 已经由调用方探过活：`Sent` = 写进了那条长连接，`Failed` = 没连接。
    pub fn record_outbound(
        &self,
        id: &str,
        text: &str,
        at: u64,
        targets: &[(String, Delivery)],
    ) -> Snapshot {
        let ids: Vec<String> = targets.iter().map(|(p, _)| p.clone()).collect();
        {
            let mut guard = self.inner.lock();
            for (peer_id, delivery) in targets {
                let convo = guard.convos.entry(peer_id.clone()).or_default();
                convo.messages.push(ChatMessage {
                    id: id.to_string(),
                    from_me: true,
                    from_name: String::new(),
                    text: text.to_string(),
                    at,
                    delivery: *delivery,
                    targets: ids.clone(),
                });
                prune(convo);
            }
            persist(&self.path, &guard);
        }
        debug!(id, at, targets = targets.len(), "chat outbound recorded");
        self.snapshot()
    }

    /// 收到对端一条消息。
    pub fn record_inbound(&self, peer_id: &str, from_name: &str, id: &str, text: &str, at: u64) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            let convo = guard.convos.entry(peer_id.to_string()).or_default();
            convo.messages.push(ChatMessage {
                id: id.to_string(),
                from_me: false,
                from_name: from_name.to_string(),
                text: text.to_string(),
                at,
                delivery: Delivery::Received,
                targets: vec![peer_id.to_string()],
            });
            prune(convo);
            persist(&self.path, &guard);
        }
        self.snapshot()
    }

    /// 对端回执：它读到了 `at <= up_to` 的那些消息 → 我的那几份副本从"已送达"变"已读"。
    ///
    /// 只动 `Sent`：`Read` 不重复标，`Failed` 更不能被这一下洗白 —— 没送出去的东西
    /// 对端不可能读过，真出现这种回执照旧说明台账里有别的错。
    pub fn mark_read_by_peer(&self, peer_id: &str, up_to: u64) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            if let Some(convo) = guard.convos.get_mut(peer_id) {
                for m in convo.messages.iter_mut() {
                    if m.from_me && m.at <= up_to && m.delivery == Delivery::Sent {
                        m.delivery = Delivery::Read;
                    }
                }
            }
            persist(&self.path, &guard);
        }
        self.snapshot()
    }

    /// 本机读过这台设备的消息：水位推进（只增不减）。
    pub fn mark_read_local(&self, peer_id: &str, at: u64) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            let convo = guard.convos.entry(peer_id.to_string()).or_default();
            if at > convo.read_at {
                convo.read_at = at;
                persist(&self.path, &guard);
            }
        }
        self.snapshot()
    }

    /// 这台设备发来的消息里最大的一条 `at` —— 回执要带回去的那个水位（没消息 = 0）。
    pub fn inbound_watermark(&self, peer_id: &str) -> u64 {
        self.inner
            .lock()
            .convos
            .get(peer_id)
            .map(|c| c.messages.iter().filter(|m| !m.from_me).map(|m| m.at).max().unwrap_or(0))
            .unwrap_or(0)
    }

    /// 本机在这台设备的会话上读到哪了（已读水位）。
    pub fn read_watermark(&self, peer_id: &str) -> u64 {
        self.inner.lock().convos.get(peer_id).map(|c| c.read_at).unwrap_or(0)
    }

    /// 清空与一台设备的聊天记录（一对一会话的「清空聊天记录」）。
    pub fn clear_peer(&self, peer_id: &str) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            if let Some(convo) = guard.convos.get_mut(peer_id) {
                convo.messages.clear();
                convo.read_at = 0;
                persist(&self.path, &guard);
            }
        }
        self.snapshot()
    }

    /// 一次性清掉一台设备的会话（解除配对等：关系没了，对话也没有存着的道理）。
    pub fn drop_peer(&self, peer_id: &str) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            if guard.convos.remove(peer_id).is_some() {
                persist(&self.path, &guard);
            }
        }
        self.snapshot()
    }
}

impl Default for ChatStore {
    fn default() -> Self {
        Self::new()
    }
}

fn unread_of(convo: &PeerConvo) -> u32 {
    convo
        .messages
        .iter()
        .filter(|m| !m.from_me && m.at > convo.read_at)
        .count() as u32
}

fn prune(convo: &mut PeerConvo) {
    let excess = convo.messages.len().saturating_sub(MAX_PER_PEER);
    if excess > 0 {
        convo.messages.drain(..excess);
    }
}

/// 每次改动都落盘：消息很小、频率是人打字的速度，攒批只会让"刚刚说的话"多一层丢的风险。
fn persist(path: &PathBuf, file: &ChatFile) {
    match serde_json::to_vec(file) {
        Ok(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = write_atomic(path, &bytes) {
                warn!(path = %path.display(), %e, "chat store persist failed");
            }
        }
        Err(e) => warn!(%e, "chat store serialize failed"),
    }
}

/// 现在（epoch 毫秒）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 快照 → 事件的载荷（`chat://changed` 与命令返回共用同一个形状）。
pub fn snapshot_value(snap: &Snapshot) -> serde_json::Value {
    serde_json::to_value(snap).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// 编排：命令与集成测试共用这两条路
// ---------------------------------------------------------------------------

/// 发一条文本消息给这些设备（`chat_send` 命令与集成测试共用这一条路）。
///
/// 一台设备存一份副本（**同一个消息 id**），送达状态在这一刻写定：写进长连接 = `Sent`，
/// 对端没有长连接 = `Failed`。**不排队重发** —— 与推送"不可达就地失败"同一个取舍：
/// 会话里的话过期补发没有意义（对面此刻要的是别的东西）。
///
/// 群发（≥2 台）与单发走的是同一条路：**没有群实体**，就是给每台各存一份（同一个 id）。
///
/// 返回（整份快照，这条消息的 id）：id 给界面**逐台读状态**用（每台的副本共用它），
/// 界面就不用去猜"哪条是刚发出去的"。
pub fn send_text(state: &Arc<AppState>, targets: &[String], text: &str) -> (Snapshot, String) {
    let id = uuid::Uuid::new_v4().to_string();
    let at = now_ms();
    let deliveries: Vec<(String, Delivery)> = targets
        .iter()
        .map(|peer_id| {
            let sent = state.ws.send(
                peer_id,
                crate::ws::events::WsEvent::ChatText { id: id.clone(), text: text.to_string(), at },
            );
            (peer_id.clone(), if sent { Delivery::Sent } else { Delivery::Failed })
        })
        .collect();
    let snap = state.chat.record_outbound(&id, text, at, &deliveries);
    (snap, id)
}

/// 会话命令里"发一条消息"的返回：整份快照 + 这条消息的 id。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub id: String,
    pub snapshot: Snapshot,
}

/// 界面读过这些设备的会话（`chat_mark_read` 命令与集成测试共用这一条路）。
///
/// 水位取的是**对端发来的最大 `at`**（不是"现在几点"）：回执带的是发送方的时钟，对端拿它
/// 跟自己的消息比即可。没读到新东西就不发（别每次聚焦都往对端丢一条空回执）。
pub fn mark_read(state: &Arc<AppState>, peer_ids: &[String]) {
    for peer_id in peer_ids {
        let watermark = state.chat.inbound_watermark(peer_id);
        if watermark == 0 || watermark <= state.chat.read_watermark(peer_id) {
            continue;
        }
        state.chat.mark_read_local(peer_id, watermark);
        // 回执丢了不重试：链路上来时会按本地水位补一次（`flush_read_receipt`）。
        let _ = state
            .ws
            .send(peer_id, crate::ws::events::WsEvent::ChatRead { up_to: watermark });
    }
}

/// 链路刚接上：把「我读到哪了」补给对端。
///
/// 已读回执只在界面读到的那一刻发一次 —— 那时链路断了/对端刚重启，这一下就丢了，而回执
/// 丢了就是把"已读"**永久**丢掉（对端那条消息会一直挂着「已送达」，谁也没机会再纠正）。
/// 链路一接上补一次是幂等的：`up_to` 就是本地已读水位，对端拿它跟自己的消息比即可。
///
/// 没有已读水位（`0`）就什么都不发：**收到 ≠ 读过**，别顺手替用户把没看过的消息标成已读。
pub fn flush_read_receipt(state: &AppState, peer_id: &str) {
    let up_to = state.chat.read_watermark(peer_id);
    if up_to == 0 {
        return;
    }
    let _ = state
        .ws
        .send(peer_id, crate::ws::events::WsEvent::ChatRead { up_to });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> ChatStore {
        let path = std::env::temp_dir().join(format!("orbdrop_chat_{name}_{}", uuid::Uuid::new_v4()));
        let store = ChatStore::with_path(path.clone());
        // 每个用例自己一份文件，跑完就删（并行跑也不互相踩）。
        let _ = std::fs::remove_file(path);
        store
    }

    fn peers(ids: &[&str]) -> Vec<(String, Delivery)> {
        ids.iter().map(|id| (id.to_string(), Delivery::Sent)).collect()
    }

    /// 群发就是"每台各一条"：A 发给 B、C 之后，世界里只多出"与 B""与 C"两条会话，
    /// 各自挂着同一条消息（同一个 id）——**没有第三个"群"那样的东西**。
    #[test]
    fn outbound_fans_out_one_copy_per_target() {
        let s = store("fanout");
        let snap = s.record_outbound("m1", "在吗", 100, &peers(&["a", "b"]));

        assert_eq!(snap.convos.len(), 2);
        for view in &snap.convos {
            assert_eq!(view.messages.len(), 1);
            let m = &view.messages[0];
            // 同一个 id：逐台状态（"送到了几台"）靠它把几份副本认成同一条。
            assert_eq!(m.id, "m1");
            assert!(m.from_me);
            assert_eq!(m.targets, vec!["a".to_string(), "b".to_string()]);
        }
        assert_eq!(snap.unread, 0);
    }

    #[test]
    fn inbound_counts_unread_until_read() {
        let s = store("unread");
        s.record_inbound("a", "PC-A", "m1", "你好", 100);
        s.record_inbound("a", "PC-A", "m2", "在吗", 200);
        let snap = s.snapshot();
        assert_eq!(snap.unread, 2);
        assert_eq!(snap.convos[0].unread, 2);
        assert_eq!(s.inbound_watermark("a"), 200);

        s.mark_read_local("a", 100);
        assert_eq!(s.snapshot().unread, 1);
        s.mark_read_local("a", 200);
        assert_eq!(s.snapshot().unread, 0);
        // 水位只增不减：旧回执（或乱序到达的）不能把已读又变回未读。
        s.mark_read_local("a", 50);
        assert_eq!(s.read_watermark("a"), 200);
    }

    #[test]
    fn peer_read_receipt_marks_only_sent_copies() {
        let s = store("receipt");
        s.record_outbound("m1", "早", 100, &[(String::from("a"), Delivery::Sent)]);
        s.record_outbound("m2", "没送出去的", 200, &[(String::from("a"), Delivery::Failed)]);
        s.mark_read_by_peer("a", 300);

        let convo = s.snapshot().convos.into_iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(convo.messages[0].delivery, Delivery::Read);
        // Failed 不会被回执洗白。
        assert_eq!(convo.messages[1].delivery, Delivery::Failed);
    }

    /// 老版本（群里那套）写下的 `chat.json` 里多出来一个 `groups` 字段：读进来看不懂就忽略，
    /// 别因为一个多余的键让整份历史读不出来。
    #[test]
    fn a_legacy_file_with_a_groups_key_still_loads() {
        let path = std::env::temp_dir().join(format!("orbdrop_chat_legacy_{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"convos":{"a":{"messages":[{"id":"m1","fromMe":false,"fromName":"PC-A","text":"你好","at":100,"delivery":"received","targets":["a"]}],"readAt":0}},"groups":[["a","b"]]}"#,
        )
        .unwrap();
        let s = ChatStore::with_path(path.clone());
        let snap = s.snapshot();
        assert_eq!(snap.convos.len(), 1);
        assert_eq!(snap.convos[0].messages[0].text, "你好");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn clear_peer_drops_that_history_only() {
        let s = store("clear");
        s.record_inbound("a", "PC-A", "m1", "你好", 100);
        s.record_inbound("b", "PC-B", "m2", "在吗", 200);
        s.mark_read_local("a", 100);
        let snap = s.clear_peer("a");
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert!(a.messages.is_empty());
        assert_eq!(a.unread, 0);
        // 别的设备那份原样留着（群发过去的那条在别人会话里也是独立的一份）。
        let b = snap.convos.iter().find(|c| c.peer_id == "b").unwrap();
        assert_eq!(b.messages.len(), 1);
    }

    #[test]
    fn survives_a_restart() {
        let path = std::env::temp_dir().join(format!("orbdrop_chat_restart_{}", uuid::Uuid::new_v4()));
        {
            let s = ChatStore::with_path(path.clone());
            s.record_inbound("a", "PC-A", "m1", "你好", 100);
            s.record_outbound("m2", "收到", 200, &peers(&["a"]));
            s.mark_read_local("a", 100);
        }
        let reopened = ChatStore::with_path(path.clone());
        let snap = reopened.snapshot();
        let convo = snap.convos.into_iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(convo.messages.len(), 2);
        assert_eq!(convo.unread, 0);
        assert_eq!(reopened.read_watermark("a"), 100);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_is_capped_from_the_old_end() {
        let s = store("cap");
        for i in 0..(MAX_PER_PEER as u64 + 10) {
            s.record_inbound("a", "PC-A", &format!("m{i}"), "x", i + 1);
        }
        let convo = s.snapshot().convos.into_iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(convo.messages.len(), MAX_PER_PEER);
        // 砍掉的是最早的，留下的是最新的。
        assert_eq!(convo.messages.last().unwrap().at, MAX_PER_PEER as u64 + 10);
        assert_eq!(convo.messages[0].at, 11);
    }
}