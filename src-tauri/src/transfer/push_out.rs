//! 发送端"还没走完的推送"的落盘（跨重启那一半，见 `docs/p1-implementation.md` §33）。
//!
//! **为什么要有它**：接收端那一半（`resume.json` 的 `pending`）让"只关掉接收端"能自己接上；
//! 可发送端一重启，`PushStore.jobs` 就空了 —— 对端来续传时 `download_target` 只答 `Missing`
//! （HTTP 410），那条传输只能收尾。这份文件把**取文件的凭据**（job：白名单 + 对端身份 + 是否
//! 已配对）与界面那张卡（batch）留下来，重启后重建：对面照常能来要块 —— 数据面本来就是
//! **接收端主动来要**，所以不用重发邀请、不用重问同意、也不用碰协议。
//!
//! **为什么单独一个文件**（不塞进 `resume.json`）：老版本会重写 `resume.json`（它认识那个
//! 文件），"读进来 → 原样写回"会把不认识的顶层键抹掉；而 `push_out.json` 老版本**压根不认识**，
//! 所以降级也不会被抹掉（新版留在这儿，升回来还能用）。与 `groups.json` 从 `chat.json` 分家
//! 是同一条理由。
//!
//! **只记"能定下来"的时刻**：进度每 200ms 一跳，不写盘。写点 = "对面第一次真的开始拉"
//! （job 进 `Accepted`）与每次**状态跃迁**；删点 = 终态（`remove_job` / `drop_batch` 各一处）。
//! 落盘失败只记一行日志 —— 它是"重启后还能接着传"的优化，不该反过来把传输打断。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::{data_dir, write_atomic};

/// `push_out.json` 的形状版本（镜像 `groups.json` 的做法：写盘那一刻补上）。
pub const FORMAT_VERSION: u32 = 1;

/// 一条没走完的发送任务最多留多久（**从最后一次有动静算起**：状态跃迁或有字节在动）。
///
/// 与接收端 `pending` 的 7 天对齐，且**发送端不能比接收端先过期** —— 否则对面还在按它自己的
/// 台阶重试，这边已经开始答 410「不再提供」了（两处的窗口要一起改，见 `PENDING_TTL_SECS`）。
pub const OUTBOUND_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// 进度只把内存里的 `updated_at` 往前挪，**落盘最多这么久一次**（整份 JSON 重写，别按 5 次/秒来）。
const TOUCH_FLUSH: Duration = Duration::from_secs(300);

/// 一条**没走完**的发送任务（`PushJob` 的可落盘那一半）。
///
/// 清单的**身份**（每个文件的大小/时间戳）不在这里：`stream_send::serve` 每次都现场 stat 源文件，
/// 接收端拿现场那份跟自己的清单对账（不符就 `SOURCE_CHANGED`）—— 所以这里只要**白名单与解析表**
/// （`sources`：相对路径 → 本机绝对路径），一个 job 一生不变，写一次就够。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutJob {
    pub id: String,
    pub batch_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub name: String,
    pub is_dir: bool,
    /// 对端是否已配对：`authorize` 分档的依据（已配对必须带 token）。
    pub peer_paired: bool,
    /// 相对路径 → 本机绝对路径（`/push/stream` 的白名单 + 解析表）。
    pub sources: Vec<(String, String)>,
    /// **对面真的开始拉了**（收到"同意"或第一条进度）。它决定重启后这条要不要恢复成一张卡：
    /// - `true`：对面手上有 `pending`，会自己来要 → 恢复 job **+ 卡片**；
    /// - `false`：只恢复 job、**不显示卡片**（补那个"在拉但状态没上报成功"的窄窗口；
    ///   但不能让界面挂一张没人能点的「等待对方同意」）。
    pub accepted: bool,
    /// TTL 锚点（unix 秒）：最后一次状态跃迁或有进展的时刻。
    pub updated_at: i64,
}

/// 一批推送（界面那张卡）的落盘形状：够 `snapshot()` 画出来就行。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutBatch {
    pub id: String,
    pub name: String,
    pub count: usize,
    pub skipped: usize,
    pub per_target_total: u64,
    pub targets: Vec<OutTarget>,
    /// 用户这次**点的**那几项源（`push_paths` 的 `roots`，不是清单里那份解析过的绝对路径）。
    ///
    /// 只有"建这批的那一刻"知道它，但它决定了重启后能不能**把源监视挂回去**：`serve` 那道逐块
    /// 复核只管"别发出坏文件"，而"源一变就把这次推送撤掉"（`transfer::watch`）需要一个监视器 ——
    /// 少了它，用户改了源之后对面再点头，收到的是一批逐条失败的记录，而不是"这次推送被撤回"。
    #[serde(default)]
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutTarget {
    pub peer_id: String,
    pub peer_name: String,
    pub invite_id: String,
    pub paired: bool,
    /// `TargetState::key()` 的字面量（`invited` / `sending` / `done` / …）。
    pub state: String,
    pub received: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OutFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    jobs: Vec<OutJob>,
    #[serde(default)]
    batches: Vec<OutBatch>,
}

struct Inner {
    file: OutFile,
    dirty: bool,
    last_flush: Instant,
}

pub struct PushOutStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl PushOutStore {
    pub fn new() -> Self {
        Self::with_path(data_dir().join("push_out.json"))
    }

    /// 指定文件的构造（单测用：不碰真实数据目录）。
    pub fn with_path(path: PathBuf) -> Self {
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<OutFile>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(Inner {
                file,
                dirty: false,
                last_flush: Instant::now(),
            }),
        }
    }

    /// 记下（或更新）一条没走完的发送任务。**立刻落盘**：这是一次状态跃迁（"从这一刻起这条
    /// 该被接手"），而这之后用户随时可能把应用关掉。
    pub fn save_job(&self, job: OutJob) {
        let mut guard = self.inner.lock();
        match guard.file.jobs.iter_mut().find(|j| j.id == job.id) {
            Some(slot) => *slot = job,
            None => guard.file.jobs.push(job),
        }
        guard.dirty = true;
        self.persist_locked(&mut guard);
    }

    /// 这条结束了（传完 / 失败 / 取消 / 过期）—— 重启后不该再恢复。
    pub fn drop_job(&self, invite_id: &str) {
        let mut guard = self.inner.lock();
        let before = guard.file.jobs.len();
        guard.file.jobs.retain(|j| j.id != invite_id);
        if guard.file.jobs.len() != before {
            guard.dirty = true;
            self.persist_locked(&mut guard);
        }
    }

    pub fn save_batch(&self, batch: OutBatch) {
        let mut guard = self.inner.lock();
        match guard.file.batches.iter_mut().find(|b| b.id == batch.id) {
            Some(slot) => {
                // `roots` 只有**建这批的那一刻**知道（用户这次选的那几项），后来的状态跃迁
                // 不该把它抹掉 —— 空的那份当"没带新信息"，保留盘上现有的。
                let roots = if batch.roots.is_empty() { slot.roots.clone() } else { batch.roots };
                *slot = OutBatch { roots, ..batch };
            }
            None => guard.file.batches.push(batch),
        }
        guard.dirty = true;
        self.persist_locked(&mut guard);
    }

    /// 这批不用再恢复了（整批走完 / 被删 / 过期）。
    pub fn drop_batch(&self, batch_id: &str) {
        let mut guard = self.inner.lock();
        let before = guard.file.batches.len();
        guard.file.batches.retain(|b| b.id != batch_id);
        if guard.file.batches.len() != before {
            guard.dirty = true;
            self.persist_locked(&mut guard);
        }
    }

    /// 有进展：把 TTL 锚点往前挪。**默认不落盘**（进度是每 200ms 的事），攒到 [`TOUCH_FLUSH`]
    /// 才写一次 —— 这样"传了 6 天还在传"的那条不会被 7 天窗口误杀，而"对面早就没了"的那条照旧会过期。
    pub fn touch_job(&self, invite_id: &str) {
        let mut guard = self.inner.lock();
        let now = now_secs();
        let mut touched = false;
        // 同一条批次也一起往前挪（它跟着这条任务活着）。
        if let Some(job) = guard.file.jobs.iter_mut().find(|j| j.id == invite_id) {
            job.updated_at = now;
            touched = true;
        }
        if !touched {
            return;
        }
        guard.dirty = true;
        if guard.last_flush.elapsed() >= TOUCH_FLUSH {
            self.persist_locked(&mut guard);
        }
    }

    /// 读一份快照（启动重建用）。
    pub fn snapshot(&self) -> (Vec<OutJob>, Vec<OutBatch>) {
        let guard = self.inner.lock();
        (guard.file.jobs.clone(), guard.file.batches.clone())
    }

    /// 过期的那些**邀请 id**（含它们的批次 id，调用方按"整批收尾"用）。
    ///
    /// 判据是**内存里**那份 `updated_at`（进度会顺手往前挪），所以取的时候不会误杀在传的那些。
    pub fn expired(&self, now: i64) -> Vec<(String, String)> {
        let guard = self.inner.lock();
        guard
            .file
            .jobs
            .iter()
            .filter(|j| now.saturating_sub(j.updated_at) > OUTBOUND_TTL_SECS)
            .map(|j| (j.id.clone(), j.batch_id.clone()))
            .collect()
    }

    fn persist_locked(&self, guard: &mut Inner) {
        // 版本位在**写盘这一刻**补上（同 `group.rs` 的纪律）。
        guard.file.version = FORMAT_VERSION;
        match serde_json::to_vec(&guard.file) {
            Ok(bytes) => {
                if let Some(parent) = self.path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match write_atomic(&self.path, &bytes) {
                    Ok(()) => {
                        guard.dirty = false;
                        guard.last_flush = Instant::now();
                    }
                    Err(e) => warn!(path = %self.path.display(), %e, "发送端任务落盘失败"),
                }
            }
            Err(e) => warn!(%e, "发送端任务序列化失败"),
        }
    }
}

impl Default for PushOutStore {
    fn default() -> Self {
        Self::new()
    }
}

/// unix 秒（与 `resume.rs` 同一套口径）。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, batch: &str, accepted: bool, updated_at: i64) -> OutJob {
        OutJob {
            id: id.into(),
            batch_id: batch.into(),
            peer_id: "peer-a".into(),
            peer_name: "PC-A".into(),
            name: "报告".into(),
            is_dir: true,
            peer_paired: true,
            sources: vec![
                ("报告/a.bin".into(), "D:/src/a.bin".into()),
                ("报告/空.txt".into(), "D:/src/空.txt".into()),
            ],
            accepted,
            updated_at,
        }
    }

    fn batch(id: &str) -> OutBatch {
        OutBatch {
            id: id.into(),
            name: "报告".into(),
            count: 2,
            skipped: 0,
            per_target_total: 4096,
            targets: vec![OutTarget {
                peer_id: "peer-a".into(),
                peer_name: "PC-A".into(),
                invite_id: "inv-1".into(),
                paired: true,
                state: "sending".into(),
                received: 1024,
                error: None,
            }],
            roots: vec!["D:/src/报告".into()],
        }
    }

    fn store(name: &str) -> (PushOutStore, PathBuf) {
        let path = std::env::temp_dir().join(format!("orbdrop_out_{name}_{}", uuid::Uuid::new_v4()));
        (PushOutStore::with_path(path.clone()), path)
    }

    /// 任务与卡片**逐字段活过一次重启**：白名单那两列、是否已配对、卡片的逐台行都在。
    #[test]
    fn an_unfinished_job_survives_a_restart_field_for_field() {
        let (s, path) = store("survive");
        s.save_job(job("inv-1", "b1", true, now_secs()));
        s.save_batch(batch("b1"));

        let reopened = PushOutStore::with_path(path.clone());
        let (jobs, batches) = reopened.snapshot();
        assert_eq!(jobs.len(), 1);
        let j = &jobs[0];
        assert_eq!(j.id, "inv-1");
        assert_eq!(j.peer_id, "peer-a");
        assert!(j.peer_paired && j.accepted);
        assert_eq!(j.sources[0], ("报告/a.bin".to_string(), "D:/src/a.bin".to_string()));
        assert_eq!(jobs[0].sources.len(), 2);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].targets[0].invite_id, "inv-1");
        assert_eq!(batches[0].targets[0].state, "sending");
        assert_eq!(batches[0].targets[0].received, 1024);
        assert_eq!(batches[0].per_target_total, 4096);
        let _ = std::fs::remove_file(path);
    }

    /// 终态就删：传完/取消/失败之后，重启不该再恢复它，也不该再留那张卡。
    #[test]
    fn terminal_states_drop_both_job_and_batch() {
        let (s, path) = store("drop");
        s.save_job(job("inv-1", "b1", true, now_secs()));
        s.save_batch(batch("b1"));
        s.drop_job("inv-1");
        s.drop_batch("b1");
        let (jobs, batches) = s.snapshot();
        assert!(jobs.is_empty() && batches.is_empty());
        let reopened = PushOutStore::with_path(path.clone());
        let (jobs, batches) = reopened.snapshot();
        assert!(jobs.is_empty() && batches.is_empty(), "删了就该落盘删掉");
        let _ = std::fs::remove_file(path);
    }

    /// 同一个 id 再存一次是**覆盖**（状态跃迁会反复写同一条），不该攒出两条。
    #[test]
    fn saving_the_same_job_twice_overwrites() {
        let (s, path) = store("upsert");
        s.save_job(job("inv-1", "b1", false, now_secs()));
        s.save_job(job("inv-1", "b1", true, now_secs()));
        let (jobs, _) = s.snapshot();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].accepted, "后写的那份说了算");
        let _ = std::fs::remove_file(path);
    }

    /// TTL：**从最后一次有动静算起** 7 天。有进展（`touch_job`）就往前挪 —— 在传的那条不会被误杀。
    #[test]
    fn the_ttl_window_is_measured_from_the_last_activity() {
        let (s, path) = store("ttl");
        let now = now_secs();
        s.save_job(job("old", "b1", true, now - 8 * 86400));
        s.save_job(job("fresh", "b2", true, now - 6 * 86400));
        let due: Vec<String> = s.expired(now).into_iter().map(|(id, _)| id).collect();
        assert_eq!(due, vec!["old".to_string()]);

        s.touch_job("old");
        assert!(s.expired(now).is_empty(), "刚有进展就不该过期");
        let _ = std::fs::remove_file(path);
    }
}