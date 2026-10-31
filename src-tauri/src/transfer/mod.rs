pub mod browse_dl;
pub mod download;
pub mod push_job;
pub mod push_out;
pub mod resume;
pub mod stream;
pub mod stream_send;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::credential;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::{FileEntry, PeerInfo, WsEvent};

use push_job::{InboundInvite, PushJob, PushStatus};

/// 一条邀请在接收方屏幕上停留多久：到点自动拒绝。
///
/// **只对老对端（不带 `await1`，见 `push_job::CAP_AWAIT`）有效**：它们那条 HTTP 邀请是
/// **挂着等同步响应**的，本机这边没人管的话，发送方会一直挂在那条请求上 —— 两边状态必须
/// 同时作废。带 `await1` 的对端走"投递即回 + 决定走报告"，这条邀请**没有时钟**：
/// 卡片留到用户点、或发送方撤销为止（守卫在 [`schedule_invite_timeout`] 里）。
pub const INVITE_TTL: Duration = Duration::from_secs(60);

/// 接收端撞上 `410` 时给用户的那句话（两条数据面共用一份，不许各写各的）。
///
/// **不能写「发送方已取消」**：`410` 只说明"对端这会儿没有这条 job 了"（`download_target` 答
/// `Missing`），而撤销、收尾、**它那台重启过**、它自己把这条判失败 —— 这四件事在线上长得一模
/// 一样。特别是"两端都重启过"那条路：接收端会自动来续传（见 `resume_pending`），撞上的正是
/// 它，写成"已取消"就是一句假话（用户会以为对面点了取消，其实对面只是关了一次应用）。
pub const SENDER_GONE: &str = "发送方已不再提供这份推送（它可能重启过，也可能已取消）";

/// 老对端那条 HTTP 邀请请求的**网络**时限（不是"等用户"的时限）。
///
/// 比 `INVITE_TTL` 多一点余量，让"对方没点、自动作废"这条正常路径能走到回执，而不是本机
/// 先断在网上。带 `await1` 的对端投递即回，这条线根本碰不到（它等的是回执通道那一条）。
const HTTP_INVITE_TIMEOUT: Duration = Duration::from_secs(75);

// ---------------------------------------------------------------------------
// Source manifest
// ---------------------------------------------------------------------------

/// 一份待推送的源：清单 + **每条清单路径对应的本机绝对路径**（F-50 / F-52）。
///
/// 清单里的相对路径**就是落盘后的样子**（`A.txt` / `D/…` / `甲目录/…` + `散文件.txt`），
/// 对端只把它接到 `<下载目录>/<发送方名>/` 后面；`sources` 是本机侧的翻译表。
/// 多选推送之所以能落成"多文件就是多文件"，靠的就是这两件事：发送方在生成时就决定了
/// 每条路径的顶层名字（各源自己的名字），对端不需要再猜、也不再套"推送名"那一层。
#[derive(Clone)]
pub struct PushSource {
    /// 只用于显示（通知/卡片里"推的是什么"）；**不参与落盘路径**。
    pub name: String,
    pub files: Vec<FileEntry>,
    /// 清单相对路径 → 本机绝对路径（与 `files` 一一对应）。
    ///
    /// 是**表**而不是列表：下载端点每次请求都要按路径找一条（见 `download_target`），
    /// 列表就是每请求一次线性扫描 —— 2 万文件的目录配 2 万次请求，累计约 2×10⁸ 次
    /// 字符串比较。路径在清单里唯一（同名去重保证了这一点），所以键不会撞。
    pub sources: HashMap<String, PathBuf>,
    pub total: u64,
    /// 清单是否**权威**（下载端点按清单取文件）。单文件推送是唯一的例外：
    /// 对端只有一条名字，没有清单。
    pub is_dir: bool,
    /// 用户这次选中了几项（1 = 单文件/单目录；>1 = 多选）。
    pub count: usize,
    /// 清单里**被跳过的项数**（链接、读不到的项）。>0 时界面必须说出来 —— 清单少了东西却
    /// 不说，用户会以为"就这些"然后推漏（同一份道理见 `fs_browse::list_dir`）。
    pub skipped: usize,
}

/// 把用户选中的一批文件/目录整理成推送清单。
///
/// **清单里的路径就是"对端落盘后的样子"**：每个源以**自己的名字**当顶层路径 ——
/// 单文件的 `A.txt`、单目录的 `D/…`、多选的 `甲目录/…` + `散文件.txt`；对端只把它们
/// 原样落到 `<下载目录>/<发送方名>/` 下面。**没有"推送名子目录"这一层**：
/// `A.txt/A.txt`、`3 项/a.txt` 那种都是多余嵌套 —— 直接传多文件就是多文件，不是在目录里。
///
/// `name` 只剩下显示用途（通知/卡片里"推的是什么"）：单源 = 它自己，
/// 多源 = 这批源的共同父目录名，跨目录/跨盘取不到就用「N 项」。
///
/// 每项可以带一个**覆盖名**（`Option<String>`，F-119）：清单里写什么名字、本机从哪条路径
/// 读字节，本来就是两件事（`files` 是"告诉对端的样子"，`sources` 是"本机的翻译表"）——
/// 覆盖名只是把前者换成调用方给的串，**源路径一个字节都不动**。第一个用它的场合是
/// "发送应用"：源是设备上某个 `/data/app/…/base.apk`（只读、不可能改名，也不可能拷一份
/// 出来），而对端看到的应当是 `应用名_versionName(versionCode)_包名.apk`（见
/// [`resume::apk_file_name`]）。
///
/// 覆盖名一律先过 [`resume::sanitize_file_name`]：它是**别的应用给的字符串**，而对端落盘
/// 那关遇到非法字符是整次接收判失败（理由见那个函数自己）。净化后为空则退回源自己的名字。
pub async fn manifest_for_named_paths(
    items: &[(PathBuf, Option<String>)],
) -> Result<PushSource, String> {
    if items.is_empty() {
        return Err("没有可推送的文件".into());
    }

    let name = if items.len() == 1 {
        // 单源：清单名**就是**那条路径（见 `is_dir` 那段），所以覆盖名要在这里就给出去 ——
        // 对端"只认名字与大小"那条单文件路拿的正是它。
        let (path, rename) = &items[0];
        shown_name_of(path, rename.as_deref())
    } else {
        common_parent_name(&items.iter().map(|(p, _)| p).collect::<Vec<_>>())
            .unwrap_or_else(|| format!("{} 项", items.len()))
    };

    let mut files: Vec<FileEntry> = Vec::new();
    let mut sources: HashMap<String, PathBuf> = HashMap::new();
    let mut total = 0u64;
    let mut skipped = 0usize;
    let mut used: Vec<String> = Vec::new();
    let mut single_file = false;
    // 算一次：`data_dir()` 要读环境变量，别在走查里每个文件项都问一遍。
    let own_data_dir = crate::config::data_dir();

    for (path, rename) in items {
        let base = file_name_of(path);
        // 下面这几道闸门看的是**源自己**的名字与路径（要读的就是它），覆盖名不参与判定 ——
        // 覆盖名只决定"对端看到什么"，不该因为它是另一个字符串就放行一个本来不能推的源。
        // 顶层源是用户**明确点的**：链接/系统目录/自家数据目录一律**当场报错**，不静默跳过 ——
        // 静默跳到最后一个文件都不剩，用户只会看到一句"没有可推送的文件"，根本不知道自己
        // 做错了什么。（走查里遇到的那些才跳过 + 计数：那是清单少一项，不是选错了源。）
        // 用 `symlink_metadata`：必须分辨"这一项本身是链接"，`metadata` 会跟过去看目标。
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|e| format!("无法访问 {}: {e}", path.display()))?;
        if crate::unsendable::is_link(&meta.file_type()) {
            return Err(format!("「{base}」是一个链接，不能作为推送源"));
        }
        if crate::unsendable::is_system_name(&base, crate::unsendable::sits_in_volume_root(path)) {
            return Err(format!("「{base}」是系统自己的目录/文件，不能推送"));
        }
        if crate::unsendable::is_mirror_path(path, &crate::mirror::root()) {
            return Err("镜像目录（拖放用的本地占位符）不能推送".into());
        }
        // 数据目录**整棵子树**都不行：只挡"数据目录本身"会漏掉 `credentials.dat` 这类
        // （见 `unsendable::is_own_data_path`），那等于直接把本机凭据推给对端。
        if crate::unsendable::is_own_data_path(path, &own_data_dir) {
            return Err("OrbDrop 自己的数据目录不能推送（里面是本机的设置与凭据）".into());
        }
        // 同名去重：`C:\a\报告.txt` 与 `D:\b\报告.txt` 不能在清单里撞成同一条路径
        // （Windows 的文件名比较本来就大小写不敏感，比对时照此办理）。
        // 去重比的是**对端看到的名字**：覆盖名（下面 `shown`）才是写进清单的那一个。
        let shown = shown_name_of(path, rename.as_deref());
        let mut prefix = shown.clone();
        let mut n = 2;
        while used.iter().any(|u| u.eq_ignore_ascii_case(&prefix)) {
            prefix = dedupe_name(&shown, n);
            n += 1;
        }
        used.push(prefix.clone());

        if meta.is_file() {
            single_file = items.len() == 1;
            files.push(FileEntry {
                path: prefix.clone(),
                size: meta.len(),
                mtime: stream::mtime_secs(&meta),
            });
            // 清单里存**已经解析过的绝对路径**：下载端点每次请求都要拿它开文件，而
            // `canonicalize` 在 Windows 上是逐段解析（20 万次请求就是白白烧掉的时间）。
            // 解析失败（清单建好前被删/ACL 挡住）退回原路径：这条文件照样进清单，
            // 真取不到时由下载端点如实报"文件不存在"，而不是在这里静默把它抹掉。
            let abs = tokio::fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| path.clone());
            sources.insert(prefix, abs);
            total += meta.len();
        } else {
            let (sub, sub_total, sub_skipped) = collect_dir_manifest(path).await?;
            skipped += sub_skipped;
            // 目录**只解析根一次**：走查交回来的相对路径都是 `read_dir` 亲眼见过的条目，
            // 逐条再 `canonicalize` 一遍（2 万条 = 2 万次逐段解析）换不到任何东西。
            let root = tokio::fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| path.clone());
            for f in sub {
                let rel = format!("{prefix}/{}", f.path);
                sources.insert(rel.clone(), root.join(&f.path));
                files.push(FileEntry {
                    path: rel,
                    size: f.size,
                    mtime: f.mtime,
                });
            }
            total += sub_total;
        }
    }

    Ok(PushSource {
        name,
        files,
        sources,
        total,
        // 单文件推送没有清单可言（对端只有一条名字，见 `spawn_download`）——
        // 这就是 `is_dir` 的真正含义：**清单是否权威**。
        is_dir: !single_file,
        count: items.len(),
        skipped,
    })
}

/// 没有覆盖名的那条路（桌面端选择框、拖进会话、镜像……语义见
/// [`manifest_for_named_paths`]）。
pub async fn manifest_for_paths(paths: &[PathBuf]) -> Result<PushSource, String> {
    let items: Vec<(PathBuf, Option<String>)> = paths.iter().map(|p| (p.clone(), None)).collect();
    manifest_for_named_paths(&items).await
}

/// 一项在清单里叫什么：有覆盖名就用它（先净化），没有就是源自己的名字。
///
/// 净化后为空（名字整份都是非法字符）也算"没有覆盖名"—— 空名字不是名字，退回源名至少
/// 还能落到盘上，而不是在清单里留一条谁都打不开的路径。
fn shown_name_of(path: &Path, rename: Option<&str>) -> String {
    match rename.map(resume::sanitize_file_name) {
        Some(shown) if !shown.is_empty() => shown,
        _ => file_name_of(path),
    }
}

pub(crate) fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名".into())
}

/// 同名源的去重名：编号插在扩展名**前面**（`报告.txt` → `报告 (2).txt`），
/// 扩展名保持不变 —— 对端落盘后仍然是"同一种文件"。
fn dedupe_name(base: &str, n: usize) -> String {
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{base} ({n})"),
    }
}

/// 这批路径的共同父目录名（都同一个父目录才算；跨盘/跨目录返回 None）。
fn common_parent_name(paths: &[&PathBuf]) -> Option<String> {
    let first = paths.first()?.parent()?;
    if paths.iter().skip(1).any(|p| p.parent() != Some(first)) {
        return None;
    }
    first.file_name().map(|s| s.to_string_lossy().to_string())
}

/// 一个目录下的**非空文件**清单（相对该目录，'/' 分隔）+ 总字节 + **跳过的项数**。
///
/// 跳过谁、为什么不整条失败：
/// - **链接**（符号链接/junction）：会回环（`%LOCALAPPDATA%\Application Data` 指向自己的
///   父目录，几百层地重复同一棵树），也会把同一份内容按两条路径列两遍；
/// - **读不到的项**（ACL 拒绝、正好被删、网络盘中途掉线）：一个子目录挡不住整批。
///
/// 这两类都**计数**回报给界面 —— 清单少了东西却不说，用户会以为"就这些"然后推漏（这就是
/// `list_dir` 那条"静默少几项比报错更坏"的另一面）。计数的是**项**：一个链接、一个读不到的
/// 目录各算一项，不是它里面有多少个文件。
///
/// 系统自己的名字（`System Volume Information`、`pagefile.sys`…）**不进这个计数**：选择框
/// 里本来就不列它们，用户没指望过；推 `C:\` 时天天弹一句"跳过了 20 项"只会把真话淹掉。
/// 顶层源撞上它们则是另一回事 —— 那是用户明确点的，`manifest_for_paths` 当场报错。
///
/// **根目录自己**读不到仍然整条失败：那是用户点的那个源，界面得说出"这个目录读不了"。
async fn collect_dir_manifest(dir: &Path) -> Result<(Vec<FileEntry>, u64, usize), String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut skipped = 0usize;
    let own_data_dir = crate::config::data_dir();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&current).await {
            Ok(rd) => rd,
            Err(e) if current == dir => return Err(format!("读取目录失败: {e}")),
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        // 这一层先收在局部，成功了再并进总表：中途列举失败 = 这一层整个不可信，已经收到的
        // 那半份宁可全丢（当作整层跳过）—— 交出一份"少了一半"的清单正是上面要防的事。
        let mut local: Vec<FileEntry> = Vec::new();
        let mut local_total = 0u64;
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut intact = true;
        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => {
                    intact = false;
                    break;
                }
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type().await else {
                skipped += 1;
                continue;
            };
            if crate::unsendable::is_link(&ft)
                || crate::unsendable::is_own_data_path(&path, &own_data_dir)
            {
                skipped += 1;
                continue;
            }
            if crate::unsendable::is_system_name(
                &name,
                crate::unsendable::sits_in_volume_root(&path),
            ) {
                continue;
            }
            if ft.is_dir() {
                subdirs.push(path);
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                skipped += 1;
                continue;
            };
            let rel = path
                .strip_prefix(dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            local_total += meta.len();
            local.push(FileEntry {
                path: rel,
                size: meta.len(),
                mtime: stream::mtime_secs(&meta),
            });
        }
        if !intact {
            skipped += 1;
            continue;
        }
        files.extend(local);
        total += local_total;
        stack.extend(subdirs);
    }
    Ok((files, total, skipped))
}

// ---------------------------------------------------------------------------
// Sender side
// ---------------------------------------------------------------------------

/// 发送方：把本机一批文件/目录推给**一台或多台**设备（F-50）。
///
/// 一次操作 = 一批，只多两件事，其余与单台推送完全一致：
/// - **清单只建一次**（目录只走一遍，各台拿到的清单与同名去重编号一模一样）；
/// - 每台各有一条独立的邀请/job —— 接收侧因此毫无差别（已配对不问同意、未绑定弹卡等点头、
///   一直等（对方点头前不设时钟，见 `watch` 与 `push_job::CAP_AWAIT`）、双向取消、断点续传全部照旧），
///   批次只多一张聚合卡片 + **失败隔离**
///   （一台送不到不影响其余几台；但一台都送不到时整批就地失败，等价于单台那条 F-53）。
///
/// 通道按对端状态逐台选（`docs/p1-implementation.md` §7）：
/// - 长连接 → WS 事件 `push_invite`；其余在线设备（已配对未连、未绑定）→ HTTP `POST /push/invite`。
///
/// 返回整批快照；界面同时还会收到 `push://batch` —— 同一份数据，界面按 `batchId` 合并。
///
/// `at` = 这批文件在**会话时间线**上的位置（发送方的墙上时间）。只有"输入框里文字与图片
/// 交错着一次发出去"那条路会点名给它（见 `chat::send_text` 的同名参数）：接收方那条文件消息
/// 的 `at` 就是它 —— 不带过去的话，对端只能拿"邀请到达的那一刻"当位置，夹在前后两段文字
/// 中间的那张图就会掉到后面去。不点名 = 当场取现在。
///
/// `names` = 与 `paths` **逐位对齐**的覆盖名（F-119，空串 = 这一项不改名），可以整个省掉。
/// 它只改"对端看到的那个名字"，本机读字节仍走 `paths` 那一条 —— 见
/// [`manifest_for_named_paths`] 那段。唯一的调用方是安卓那颗「＋」的「应用」那一项：源是
/// 设备上某个 `/data/app/…/base.apk`，而它落盘后该叫
/// `应用名_versionName(versionCode)_包名.apk`。
pub async fn push_paths(
    state: Arc<AppState>,
    paths: Vec<String>,
    peer_ids: Vec<String>,
    names: Option<Vec<String>>,
    at: Option<u64>,
) -> Result<push_job::BatchView, String> {
    // 逐位对齐是**调用方的契约**：长度对不上说明界面把两条表拼错了，与其按错位的名字发出去
    // （对端落盘的文件名全是别人的），不如当场拒绝。
    let names = names.unwrap_or_default();
    if !names.is_empty() && names.len() != paths.len() {
        return Err(format!(
            "改名表与文件数对不上（内部错误）：{} 个名字 vs {} 个文件",
            names.len(),
            paths.len()
        ));
    }
    let items: Vec<(PathBuf, Option<String>)> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let rename = names.get(i).filter(|n| !n.is_empty()).cloned();
            (PathBuf::from(p), rename)
        })
        .collect();
    let at = at.unwrap_or_else(crate::chat::now_ms);
    // 同一台设备勾两次只发一条（保持勾选顺序）。
    let mut ids: Vec<String> = Vec::new();
    for id in peer_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("没有选择目标设备".into());
    }

    let roots: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
    let source = manifest_for_named_paths(&items).await?;
    if source.files.is_empty() {
        return Err("没有可推送的文件".into());
    }

    let (device_id, self_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    let server_port = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    let batch_id = uuid::Uuid::new_v4().to_string();

    // 先给每台定一行：不在网上的这台**就地失败、不排队**（F-53），能派的进 `pending` 去发邀请。
    let mut targets: Vec<push_job::BatchTarget> = Vec::new();
    let mut pending: Vec<(crate::config::PeerDevice, String)> = Vec::new();
    let mut local_pending: Vec<(crate::localsend::LocalSendPeer, String)> = Vec::new();
    let mut first_reason: Option<String> = None;
    for peer_id in &ids {
        if let Some(peer) = state.get_peer(peer_id) {
            if !peer.online || peer.ip.is_empty() || peer.port == 0 {
                let reason = format!("「{}」不在网上，推送未发送", peer.name);
                first_reason.get_or_insert(reason.clone());
                targets.push(push_job::BatchTarget {
                    peer_id: peer.id.clone(),
                    peer_name: peer.name.clone(),
                    invite_id: String::new(),
                    transport: crate::config::PeerTransport::OrbDrop,
                    paired: peer.paired,
                    state: push_job::TargetState::Failed,
                    received: 0,
                    error: Some(reason),
                });
                continue;
            }
            let invite_id = uuid::Uuid::new_v4().to_string();
            let job = PushJob::new(
                invite_id.clone(),
                batch_id.clone(),
                peer.id.clone(),
                peer.name.clone(),
                source.name.clone(),
                source.is_dir,
                source.sources.clone(),
                peer.paired,
            );
            // **这条从这一刻起就该被接手**：先落盘再进内存（`accepted = false` —— 等对面真的开始拉，
            // 由 `update_status` 升级成 `true`，见 `push_out::OutJob::accepted`）。
            // 一次发送只在"建"与"对面开始拉"这两个时刻动盘，进度不写。
            state.push.out.save_job(push_job::out_job_of(&job, false));
            state.push.insert_job(job);
            targets.push(push_job::BatchTarget {
                peer_id: peer.id.clone(),
                peer_name: peer.name.clone(),
                invite_id: invite_id.clone(),
                transport: crate::config::PeerTransport::OrbDrop,
                paired: peer.paired,
                state: push_job::TargetState::Invited,
                received: 0,
                error: None,
            });
            pending.push((peer, invite_id));
        } else if let Some(peer) = state.localsend.get(peer_id) {
            if !peer.alive() || peer.ip.is_empty() || peer.port == 0 {
                let reason = format!("「{}」不在 LocalSend 网上，推送未发送", peer.alias);
                first_reason.get_or_insert(reason.clone());
                targets.push(push_job::BatchTarget {
                    peer_id: peer.id.clone(),
                    peer_name: peer.alias.clone(),
                    invite_id: String::new(),
                    transport: crate::config::PeerTransport::LocalSend,
                    paired: false,
                    state: push_job::TargetState::Failed,
                    received: 0,
                    error: Some(reason),
                });
                continue;
            }
            let invite_id = uuid::Uuid::new_v4().to_string();
            targets.push(push_job::BatchTarget {
                peer_id: peer.id.clone(),
                peer_name: peer.alias.clone(),
                invite_id: invite_id.clone(),
                transport: crate::config::PeerTransport::LocalSend,
                paired: false,
                state: push_job::TargetState::Invited,
                received: 0,
                error: None,
            });
            local_pending.push((peer, invite_id));
        } else {
            let reason = format!("设备不存在: {peer_id}");
            first_reason.get_or_insert(reason.clone());
            targets.push(push_job::BatchTarget {
                peer_id: peer_id.clone(),
                peer_name: peer_id.clone(),
                invite_id: String::new(),
                transport: crate::config::PeerTransport::OrbDrop,
                paired: false,
                state: push_job::TargetState::Failed,
                received: 0,
                error: Some(reason),
            });
        }
    }

    if pending.is_empty() && local_pending.is_empty() {
        // 一台都派不出去 = 单台推送那条"就地失败"（F-53）：界面弹一条提示，不建卡片。
        // 单台时沿用原来那句原话，多台时给一句能概括的。
        return Err(if ids.len() == 1 {
            first_reason.unwrap_or_else(|| "推送未发送".into())
        } else {
            format!(
                "所选 {} 台设备都收不了（不在网上或设备不存在），推送未发送",
                ids.len()
            )
        });
    }
    let has_orbdrop_pending = !pending.is_empty();

    let batch = push_job::PushBatch {
        id: batch_id.clone(),
        name: source.name.clone(),
        count: source.count,
        skipped: source.skipped,
        per_target_total: source.total,
        targets,
        last_emit: Instant::now(),
    };
    // 卡片也落一份（含**用户这次点的那几项** `roots`：重启后要按它把源监视挂回去）。
    // LocalSend-only 的批次没有可恢复的 job（`out_batch_of` 会过滤掉全部目标），不写空批次。
    let out_batch = push_job::out_batch_of(&batch, &roots);
    if !out_batch.targets.is_empty() {
        state.push.out.save_batch(out_batch);
    }
    let view = state.push.insert_batch(batch);
    if let Ok(value) = serde_json::to_value(&view) {
        ws::emit_ui(&state, ws::events::ui::PUSH_BATCH, &value);
    }

    for (peer, invite_id) in pending {
        let event = WsEvent::PushInvite {
            invite_id: invite_id.clone(),
            sender: PeerInfo {
                id: device_id.clone(),
                name: self_name.clone(),
            },
            name: source.name.clone(),
            size: source.total,
            is_dir: source.is_dir,
            count: source.count,
            files: source.files.clone(),
            caps: vec![
                push_job::CAP_STREAM.to_string(),
                push_job::CAP_AWAIT.to_string(),
            ],
            at,
        };
        spawn_invite_delivery(state.clone(), peer, invite_id, event, server_port);
    }

    // LocalSend 目标：没有 `PushJob`（协议不支持续传），只建批次行；由客户端推字节并推进
    // 同一张批次卡。失败只落自己那一行。
    //
    // **走 per-device lane**：接收端只有一个上传会话槽，直接 spawn 会和同一台设备的
    // 文本/图片、以及另一次文件推送撞 409。取消令牌在**排队前**就登记，排队期间点取消
    // 也能收掉（`cancel_push` 只认这张表）。
    for (peer, invite_id) in local_pending {
        let lane_id = peer.id.clone();
        let job_state = state.clone();
        let source = source.clone();
        let batch_id = batch_id.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        state
            .localsend
            .register_outbound(invite_id.clone(), cancel.clone());
        state.localsend.enqueue_outbound(
            &lane_id,
            Box::pin(async move {
                let job_invite = invite_id;
                if cancel.is_cancelled() {
                    job_state.localsend.unregister_outbound(&job_invite);
                    push_job::batch_set(
                        &job_state,
                        &job_invite,
                        push_job::TargetState::Cancelled,
                        None,
                        Some("已取消".to_string()),
                        true,
                    );
                    return;
                }
                if let Err(e) = crate::localsend::client::send_files(
                    job_state.clone(),
                    peer,
                    batch_id,
                    job_invite.clone(),
                    source,
                    cancel,
                )
                .await
                {
                    push_job::batch_set(
                        &job_state,
                        &job_invite,
                        push_job::TargetState::Failed,
                        None,
                        Some(e),
                        true,
                    );
                }
            }),
        );
    }

    // 这批里有**OrbDrop**台在等对方点头（`Invited`）→ 盯住这次推的源：源一变，这批当场作废。
    // LocalSend 目标不走源监视（它没有可续的 job，客户端自己会失败）。
    if has_orbdrop_pending && state.push.batch_has_invited(&batch_id) {
        watch::watch_sources(&state, &batch_id, &roots);
    }

    Ok(view)
}

/// 把一条邀请送出去：长连接走 WS，其余在线设备走 HTTP `POST /push/invite`。
///
/// 一台一条任务：任何一台的失败都只落在它自己那一行上（`batch_set(.., Failed, ..)`），
/// 牵不动整批 —— 推 5 台、其中 1 台掉线，另外 4 台照走。
fn spawn_invite_delivery(
    state: Arc<AppState>,
    peer: crate::config::PeerDevice,
    invite_id: String,
    event: WsEvent,
    server_port: u16,
) {
    let peer_id = peer.id.clone();
    let peer_name = peer.name.clone();
    let paired = peer.paired;
    crate::task::spawn("push invite", async move {
        if state.ws.has_connection(&peer_id) {
            if state.ws.send(&peer_id, event) {
                // 长连接那条路本来就是异步的（决定走 `push_response`）。**没有时钟**：
                // 对端一直不点头就一直等着（它带 `await1` 时两边都没有时钟；老对端自己
                // 那边会 60 秒作废，或者由它的回答/对账把话带回来）。
                debug!(%invite_id, %peer_id, "invite sent over the long connection; waiting for the decision");
            } else {
                fail_invite(&state, &invite_id, "对方离线");
            }
            return;
        }
        match deliver_invite_http(&peer, server_port, &event, paired).await {
            Ok(d) if d.pending => {
                // `await1` 的对端：**投递即算送达**。它那边一条卡片挂着等用户点，
                // 那道决定稍后从回执通道回来（`push_response`）。本机这张卡就一直写
                // 「等待对方同意」—— 这正是要的：不点就一直等。
                info!(%invite_id, %peer_id, "invite delivered; the peer is waiting for its user (await1)");
            }
            Ok(d) if d.accepted => {
                info!(%invite_id, %peer_id, "invite auto-accepted by a paired peer");
                state.push.update_status(&invite_id, PushStatus::Accepted);
                ws::emit_ui(
                    &state,
                    ws::events::ui::PUSH_RESPONSE,
                    &serde_json::json!({
                        "inviteId": invite_id,
                        "accepted": true,
                    }),
                );
            }
            Ok(d) => {
                let why = refusal_reason(&peer_name, &d);
                info!(%invite_id, %peer_id, legacy = d.reason.is_none(), %why, "invite refused synchronously");
                state.push.remove_job(&invite_id);
                push_job::batch_set(
                    &state,
                    &invite_id,
                    push_job::TargetState::Rejected,
                    None,
                    Some(why.clone()),
                    true,
                );
                ws::emit_ui(
                    &state,
                    ws::events::ui::PUSH_RESPONSE,
                    &serde_json::json!({
                        "inviteId": invite_id,
                        "accepted": false,
                        "reason": why,
                    }),
                );
                ws::emit_ui(
                    &state,
                    ws::events::ui::PUSH_DONE,
                    &serde_json::json!({
                        "inviteId": invite_id,
                        "status": "failed",
                        "reason": why,
                    }),
                );
            }
            Err(reason) => fail_invite(&state, &invite_id, &reason),
        }
    });
}

/// 邀请没能送到（对方不在网上/请求失败）：清掉 job 并把这一台标成失败。
fn fail_invite(state: &AppState, invite_id: &str, reason: &str) {
    state.push.remove_job(invite_id);
    push_job::batch_set(
        state,
        invite_id,
        push_job::TargetState::Failed,
        None,
        Some(reason.to_string()),
        true,
    );
    ws::emit_ui(
        state,
        ws::events::ui::PUSH_DONE,
        &serde_json::json!({
            "inviteId": invite_id,
            "status": "failed",
            "reason": reason,
        }),
    );
}

/// 一条邀请在远端是什么结局（`POST /push/invite` 的响应体）。
///
/// 三档要分得清：
/// - `pending` —— 送到了、对方记下了，正在等它的用户点（`await1` 对端）。**发送方继续等**，
///   不是拒绝；
/// - `accepted` —— 对方**免同意**直接收（已配对设备）；
/// - 其余 —— 对方明确拒绝 / 收不下，`reason` 是它给的原话。
#[derive(serde::Deserialize)]
struct InviteDecision {
    accepted: bool,
    /// 邀请已送达、等对方用户决定（见 `push_job::CAP_AWAIT`）。老对端不带 → false。
    #[serde(default)]
    pending: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// 那条**同步**的 `accepted:false` 该怎么说。
///
/// **对端没回 `pending` 就说明它不认识 `await1`**（见 `push_job::CAP_AWAIT`）：带这个位
/// 的对端永远不会在这条同步响应上拒绝（拒绝走回执通道），所以只要在这儿收到 `accepted:false`，
/// 对面就是老版本。而老版本那句 `accepted:false` **分不出"用户点了拒绝"与"60 秒到点自动作废"**
/// （它不带 `reason`）—— 所以别替它编一个"对方拒绝"，如实说清"对面是老版本、它按老规矩办"，
/// 让人知道**升级对端**才是那条路（P1-13 的同一类毛病：说错原因比不说不清更坏）。
fn refusal_reason(peer_name: &str, decision: &InviteDecision) -> String {
    decision.reason.clone().unwrap_or_else(|| {
        format!("「{peer_name}」是老版本：它不支持一直等，邀请 60 秒没人处理就会自动作废")
    })
}

/// 把邀请 POST 到对端的 `/push/invite`。返回对端给的那道决定。
///
/// 请求体就是 WS 那条路的 `push_invite` 事件（同一个序列化结构，多带一个 `port`）——
/// 两条通道说的是同一套协议，接收方只有一份解析代码。`port` 是本机**服务端口**：
/// 这个 HTTP 请求的源端口是一次性的，对端要回执（进度/完成）得发回真正的服务端口。
///
/// **响应不再是"最终决定"**：带 `await1` 的对端（新版本）只回一句"送到了、等着呢"
/// （`pending`），它那边的用户点完才把决定通过 `/push/report` 发回来；老对端才会一直
/// 挂着这条请求，把用户点的结果当同步响应当场回（那条路上 `HTTP_INVITE_TIMEOUT` 是
/// 网络时限，不是"等用户"的时限）。
async fn deliver_invite_http(
    peer: &crate::config::PeerDevice,
    server_port: u16,
    event: &WsEvent,
    paired: bool,
) -> Result<InviteDecision, String> {
    let mut body = serde_json::to_value(event).map_err(|e| format!("邀请序列化失败: {e}"))?;
    body["port"] = serde_json::json!(server_port);

    let url = format!(
        "{}/push/invite",
        crate::net::peer_base(peer.dial().0, peer.dial().1)
    );
    let mut req = crate::net::dial_client(
        peer.dial().0,
        &crate::credential::dial_pin(&peer.id, paired),
    )
    .post(&url)
    .timeout(HTTP_INVITE_TIMEOUT)
    .json(&body);
    if paired {
        if let Ok(Some(token)) = credential::get_peer_token(&peer.id) {
            req = req.bearer_auth(token);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.name))?;
    if !resp.status().is_success() {
        return Err(format!(
            "「{}」没能接收这次推送（HTTP {}）",
            peer.name,
            resp.status().as_u16()
        ));
    }
    let decision: InviteDecision =
        crate::net::read_json_capped(resp, crate::net::CONTROL_BODY_MAX)
            .await
            .map_err(|e| format!("「{}」的回应无法解析：{e}", peer.name))?;
    Ok(decision)
}

/// 发送方：取消一条推送（F-54）。
///
/// 对端必须知道，两个阶段各有各的理由：
/// - 邀请还挂着（对方还没点）→ 那条邀请卡必须撤掉，否则它一直挂在对面通知列表里，
///   点「接收」才发现早就没了；
/// - 已经在传 → 下载端点会当场断流、下一次请求拿到 410，收手更快。
pub fn cancel_push(state: &AppState, invite_id: &str) -> Result<(), String> {
    // LocalSend 目标没有 `PushJob`：它的取消令牌在 LocalSendState 里。
    if state.localsend.outbound_cancel(invite_id).is_some() {
        crate::localsend::client::cancel(state, invite_id);
        push_job::batch_set(
            state,
            invite_id,
            push_job::TargetState::Cancelled,
            None,
            Some("已取消".to_string()),
            true,
        );
        return Ok(());
    }
    cancel_push_with(state, invite_id, "已取消", "发送方已取消")
}

/// 同上，但两句原因各写各的：`card` 是本机卡片那一行，`wire` 是回给对端那句话
/// （源变化那条路要用它说清"为什么"——见 `transfer::watch`）。
fn cancel_push_with(
    state: &AppState,
    invite_id: &str,
    card: &str,
    wire: &str,
) -> Result<(), String> {
    let job = state
        .push
        .job(invite_id)
        .ok_or_else(|| "推送不存在".to_string())?;
    state.push.remove_job(invite_id);
    push_job::batch_set(
        state,
        invite_id,
        push_job::TargetState::Cancelled,
        None,
        Some(card.to_string()),
        true,
    );
    notify_peer(
        state,
        &job.peer_id,
        WsEvent::PushDone {
            invite_id: invite_id.to_string(),
            status: "cancelled".into(),
            reason: Some(wire.to_string()),
        },
    );
    ws::emit_ui(
        state,
        ws::events::ui::PUSH_DONE,
        &serde_json::json!({
            "inviteId": invite_id,
            "status": "cancelled",
        }),
    );
    Ok(())
}

/// 发送方：取消一整批推送 —— 每台各撤一条（两个阶段各自的理由见 `cancel_push`）。
///
/// 单台的失败（对端已经不在网上、这条早就结束了）**不该挡住其余几台的取消**，
/// 所以逐台各自尽力、结果不带出来。
pub fn cancel_push_batch(state: &AppState, batch_id: &str) -> Result<(), String> {
    cancel_push_batch_with(state, batch_id, "已取消", "发送方已取消")
}

/// 同上，换成自己的那两句话（源文件/目录变了时用它，见 `transfer::watch`）。
pub fn cancel_push_batch_with(
    state: &AppState,
    batch_id: &str,
    card: &str,
    wire: &str,
) -> Result<(), String> {
    let targets = state
        .push
        .batch_targets(batch_id)
        .ok_or_else(|| "推送不存在".to_string())?;
    for target in targets {
        if target.transport == crate::config::PeerTransport::LocalSend {
            crate::localsend::client::cancel(state, &target.invite_id);
            push_job::batch_set(
                state,
                &target.invite_id,
                push_job::TargetState::Cancelled,
                None,
                Some(card.to_string()),
                true,
            );
        } else if target.state.is_waiting() {
            let _ = cancel_push_with(state, &target.invite_id, card, wire);
        }
    }
    Ok(())
}

/// 发送方给对端捎一条推送状态（目前只有「取消」）：长连接走 WS，否则 POST 它的 `/push/report`。
///
/// 地址取自地址簿 —— 发送方手里没有对端的回调地址，但推送本来就要求对端在线（有地址）。
pub fn notify_peer(state: &AppState, peer_id: &str, event: WsEvent) {
    let callback = state
        .get_peer(peer_id)
        .and_then(|p| (!p.ip.is_empty() && p.port != 0).then(|| (p.ip.clone(), p.port)));
    push_job::report_to_sender(state, peer_id, &callback, event);
}

// ---------------------------------------------------------------------------
// Receiver side
// ---------------------------------------------------------------------------

/// 接收方：邀请到点没被处理就自动拒绝（**只服务老对端**）。
///
/// 老对端的 HTTP 邀请挂着等同步响应，本机这边没人管的话它会一直挂着 —— 两边状态必须同时
/// 作废。带 `await1` 的对端（见 `push_job::CAP_AWAIT`）在等一条**报告**，两边都没有时钟：
/// 这条函数对它直接返回，卡片留到用户点、或发送方撤销为止（这就是"一直等"）。
pub fn schedule_invite_timeout(state: Arc<AppState>, invite_id: String) {
    if state.push.invite_supports_await(&invite_id) {
        return;
    }
    crate::task::spawn("push invite decision timeout", async move {
        tokio::time::sleep(INVITE_TTL).await;
        let Some(invite) = state.push.take_invite(&invite_id) else {
            return; // 已经处理过了
        };
        info!(%invite_id, "invite expired, auto-rejecting");
        reject_shared(&state, invite, "超时未处理");
    });
}

/// 接收方：同意一条邀请 → 开始拉取（F-52）。
///
/// `root` = 这次接收的落盘根目录：`None` 用设置里的下载目录。只有界面在「每次选择位置」
/// （`DownloadMode::AskEachTime`）下替用户问过目录时才带它 —— 自动接收（已配对 + 开着
/// 「已配对设备自动接收」）走的是 `None`。传进来就**先验一次**（必须绝对路径、能建出来），
/// 不合格当场返回错误、这条邀请**原样留着**（卡片还在，用户能换个目录再点一次），也不放行发送方。
pub fn accept_invite(
    state: Arc<AppState>,
    invite_id: &str,
    root: Option<std::path::PathBuf>,
) -> Result<(), String> {
    // 落盘那一层由设置决定（S-12）：在拿 inbound 锁**之前**读，锁序单一，不跟自己打结。
    let device_subdir = state.config.lock().device_subdir;
    let (invite, sender_id) = {
        let mut inbound = state.push.inbound.lock();
        let inv = inbound
            .get_mut(invite_id)
            .ok_or_else(|| "推送不存在或已超时".to_string())?;
        // 下载要连的地址：邀请带来的地址，或现从地址簿里补一个 —— 补不上就不能开始
        // （缺了它只会空转到天荒地老），也就还没到"放行"的时候。
        let callback = inv
            .callback
            .clone()
            .or_else(|| {
                state
                    .get_peer(&inv.sender.id)
                    .and_then(|p| (!p.ip.is_empty() && p.port != 0).then(|| (p.ip.clone(), p.port)))
            })
            .ok_or_else(|| "发送方地址未知，无法接收".to_string())?;
        if let Some(base) = root.as_deref() {
            // 目录选择框给的一定是绝对路径；不是就不接（相对路径会挂在进程当前目录上，
            // 落哪儿全凭启动方式，谁也不想要这种"接收方"）。
            if !base.is_absolute() {
                return Err("落盘目录必须是绝对路径".into());
            }
            // 就地验一次可写（不存在的建出来、拔掉的盘/只读目录当场报错）：失败时**什么都别动**，
            // 用户换一个目录重按「接收」即可 —— 比"收了一半才发现写不进去"强。
            let dir = resume::landing_dir(base, &inv.sender.name, device_subdir);
            std::fs::create_dir_all(&dir).map_err(|e| format!("落盘目录不可用：{e}"))?;
        }
        if let Some(tx) = inv.responder.take() {
            // 放行发送方；但**保留**这条邀请：下载循环拿它当"没被取消"的标志。
            let _ = tx.send(true);
        }
        (
            InboundInvite {
                invite_id: inv.invite_id.clone(),
                sender: inv.sender.clone(),
                name: inv.name.clone(),
                size: inv.size,
                is_dir: inv.is_dir,
                count: inv.count,
                files: inv.files.clone(),
                caps: inv.caps.clone(),
                root: root.clone(),
                callback: Some(callback),
                responder: None,
            },
            inv.sender.id.clone(),
        )
    };

    // **从这一刻起这条接收该被接手**：落一份待续传任务 —— 进程之后被杀/关掉，下次启动
    // 自己接着收（见 `resume_pending`）。写在放行之后、起下载之前：没点头的邀请不该在重启
    // 之后自己开始收，那等于替用户点了一次同意。
    state.push.resume.put_pending(invite.to_pending());
    push_job::spawn_download(state, invite, sender_id);
    Ok(())
}

/// 接收方：拒绝一条邀请 —— 界面上的按钮走这里。
pub fn reject_invite(state: &AppState, invite_id: &str) {
    reject_invite_with(state, invite_id, "对方拒绝");
}

/// 同上，但把原因带到本机那条通知上。
///
/// 自动放行（已绑定设备）失败时用它：那种场合**没有人"拒绝"**，是本机收不下
/// （地址未知之类）—— 原因照实说，别让用户以为对面点了拒绝。
pub fn reject_invite_with(state: &AppState, invite_id: &str, reason: &str) {
    // 明确不要这条：待续传任务一并作废（预检那条路不会记任务，这里是兜底 —— 万一
    // 放行之后又走到拒绝，重启后不该再自己开始收）。
    state.push.resume.drop_pending(invite_id);
    let Some(mut invite) = state.push.take_invite(invite_id) else {
        return;
    };
    if let Some(tx) = invite.responder.take() {
        let _ = tx.send(false);
    }
    reject_shared(state, invite, reason);
}

/// 拒绝的共同出口：回执 + 本机通知列表撤掉这张卡。
fn reject_shared(state: &AppState, invite: InboundInvite, reason: &str) {
    let response = WsEvent::PushResponse {
        invite_id: invite.invite_id.clone(),
        accepted: false,
        reason: Some(reason.to_string()),
    };
    if invite.supports_await() {
        // 等报告那一版（`await1`）：这条决定只能从回执通道走 —— 长连接在就先走它，
        // 否则 POST 对方的 `/push/report`。**未绑定设备没有长连接**，HTTP 这一跳是它唯一的路。
        push_job::report_to_sender(state, &invite.sender.id, &invite.callback, response);
    } else if state.ws.has_connection(&invite.sender.id) {
        // 老对端：长连接上照旧回一条（HTTP 那条路的决定是它自己那条同步响应）。
        ws::send_to_peer(state, &invite.sender.id, response);
    }
    ws::emit_ui(
        state,
        ws::events::ui::PUSH_RESPONSE,
        &serde_json::json!({
            "inviteId": invite.invite_id,
            "accepted": false,
            "reason": reason,
        }),
    );
    // 拒绝也是个结局：发送方那边可能还是靠对账（`PushAsk`）补上的，这里把真话说出去。
    state.push.record_outcome(
        &invite.invite_id,
        &invite.sender.id,
        "rejected",
        0,
        Some(reason.to_string()),
    );
}

/// 老协议那条路写下的**半截文件**的清理入口：算好落盘根，剩下的交给 `push_job::discard_legacy`。
///
/// 落盘根用 `resume::landing_dir(<根>, <对端名>, <设置里的那层>)` —— 与下载循环是**同一份输入**
/// （那次邀请的根 / 对端自报的名字 / 设置），差一样删的就不是它写的那个位置。
///
/// `files` = (清单里的相对路径, 总大小)：只有"本机写过、还没写完"的那几个会被删（判据在
/// `discard_legacy` 里，别在这里改口径）。
fn discard_legacy_receive(
    state: &AppState,
    invite_id: &str,
    sender_name: &str,
    root: Option<&std::path::Path>,
    files: &[(&str, u64)],
) {
    let (download_dir, device_subdir) = {
        let config = state.config.lock();
        (config.download_dir.clone(), config.device_subdir)
    };
    let base = resume::landing_dir(
        root.unwrap_or(download_dir.as_path()),
        sender_name,
        device_subdir,
    );
    push_job::discard_legacy(state, invite_id, &base, files, None);
}

/// 接收方：取消正在进行的接收（**新协议**那份按文件身份存的块位图留着，用户手动重推一次仍省得下）。
///
/// **老协议**那条路不一样：它那份记录按**邀请 id** 存、半截文件又写在最终路径上，取消之后这条
/// 邀请不会再被接手（下面第一行的 `drop_pending`）、重推一次又是新 id —— 留着就是一对
/// "永远没人清"的记录 + 假文件，所以一并收掉（见 `push_job::discard_legacy`）。
///
/// **但不再自动接着拉**（`resume_pending` 的名单里把它去掉）：自动续传的前提是"用户没喊停"，
/// 而这里正是用户喊停的那一下 —— 关掉应用是"可能想停"，点这颗按钮是"就是要停"。
pub fn cancel_receive(state: &AppState, invite_id: &str) {
    state.push.resume.drop_pending(invite_id);
    // 暂停中的那条：先摘表、再放开闸门 —— worker 正等在 `wait_for` 上，不放它就永远醒不过来
    // （表摘了、任务还在，比留着更难查）。醒来后它会看到邀请已被取走，照常走清理。
    if let Some(control) = state.receive_downloads.lock().remove(invite_id) {
        control.set_paused(false);
    }
    let Some(invite) = state.push.take_invite(invite_id) else {
        return;
    };
    // 老协议那条路的收尾清理（邀请此刻已经在手里，落盘根与清单都从它推）。
    let manifest = invite.manifest();
    let sizes: Vec<(&str, u64)> = manifest.iter().map(|f| (f.path.as_str(), f.size)).collect();
    discard_legacy_receive(
        state,
        invite_id,
        &invite.sender.name,
        invite.root.as_deref(),
        &sizes,
    );
    // 下载循环发现邀请没了就会收手；这里替它把话带到发送方 ——
    // 否则对面永远停在"正在传输"，那条 job 没有超时兜底。
    push_job::report_to_sender(
        state,
        &invite.sender.id,
        &invite.callback,
        WsEvent::PushDone {
            invite_id: invite.invite_id.clone(),
            status: "failed".into(),
            reason: Some("接收方已取消".into()),
        },
    );
    ws::emit_ui(
        state,
        ws::events::ui::PUSH_DONE,
        &serde_json::json!({
            "inviteId": invite.invite_id,
            "status": "cancelled",
        }),
    );
    // 这条 `PushDone` 走的是"尽力而为"的回执路（链路断了就没了），对账时也要答得出来。
    state.push.record_outcome(
        &invite.invite_id,
        &invite.sender.id,
        "cancelled",
        0,
        Some("接收方已取消".into()),
    );
    // 取消 = 那条还没传完的推送要能接着传，所以那些还没落盘的进度必须现在就落地
    // （见 `ResumeStore::flush`）：新协议的块位图靠它，老的续传记录则刚被上面那条清理收掉了。
    state.push.resume.flush();
    warn!(%invite_id, "receive cancelled; 老协议的半截文件与记录已收掉，块位图留着");
}

// ---------------------------------------------------------------------------
// 自动续传：进程重启之后接着收
// ---------------------------------------------------------------------------

/// 每一拍（启动时 + 每 5s 一拍）看一遍还没传完的接收任务，按退避台阶接着收。
///
/// 判据与纪律：
/// - **这一轮还在跑**（邀请还在 `inbound` 里）→ 跳过。它同时是 TTL 的豁免：真的在传的那条
///   不该被 7 天窗口砍掉（"有进展"由 `touch_pending` 在文件收尾时刷新）。
/// - **过期**（7 天没进展）→ 终态收尾并说清原因；文件级位图留着（用户手动重推仍接着传）。
/// - **没到退避台阶** → 跳过（够不着对端时就是靠它"先放着不管"，见 `RETRY_BACKOFF`）。
/// - 其余 → 重建邀请（`PendingInbound::to_invite`）、挂回 `inbound`、走同一条
///   `spawn_download`：新数据面按**文件身份**接手块位图、老数据面按**同一个邀请 id**接手
///   offset —— 两条路都不必重传已经落盘的那些字节。位图已经满了的那些文件连网都不用上
///   （`run` 的收尾兜底直接把 `.part` 改名落位）。
///
/// 幂等、随时可叫：那一拍本来就只是"看一遍名单"（退避在 `attempt_due` 里把关）。
pub fn resume_pending(state: &Arc<AppState>) {
    let now = resume::now_secs();
    // 任务删了但退避记账还在（内存里的一点点残留，顺手扫掉）。
    state.push.resume.prune_attempts();
    for rec in state.push.resume.pending_entries() {
        if state.push.inbound.lock().contains_key(&rec.invite_id) {
            continue;
        }
        if rec.expired(now) {
            // 这条待续传任务到此为止。老协议那条路的**半截文件与记录一并收掉**：邀请早就不在
            // `inbound` 里了（这条任务一直在退避重试、没跑起来），所以落盘根与清单得从这条任务
            // 自己推 —— 输入与下载循环用的是同一套（那次邀请的根 + 对端名 + 设置），
            // 见 `discard_legacy_receive`。不这么做，这对记录 + 最终路径上的半截文件就没人清了。
            let files: Vec<(&str, u64)> = rec
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.size))
                .collect();
            discard_legacy_receive(
                state,
                &rec.invite_id,
                &rec.sender_name,
                rec.root.as_deref().map(std::path::Path::new),
                &files,
            );
            push_job::fail_download(
                state,
                &rec.sender_id,
                &rec.callback,
                &rec.invite_id,
                format!(
                    "等了 {} 天也没传完，已放弃（发送方一直没出现）",
                    resume::PENDING_TTL_SECS / 86400
                ),
            );
            continue;
        }
        if !state.push.resume.attempt_due(&rec.invite_id) {
            continue;
        }
        state.push.resume.note_attempt(&rec.invite_id);
        // 挂回 `inbound`：它同时是"这一轮在跑"的标志（`cancel_receive` 也靠它叫停）。
        state.push.insert_invite(rec.to_invite());
        debug!(invite = %rec.invite_id, sender = %rec.sender_id, "接着收这条没传完的推送");
        push_job::spawn_download(state.clone(), rec.to_invite(), rec.sender_id.clone());
    }
}

// ---------------------------------------------------------------------------
// 对账：回执丢了怎么办（`PushAsk` / `PushState`）
// ---------------------------------------------------------------------------

/// 发送方：把本机发出去的、**还没落终态**的邀请问一遍对端。
///
/// 调用点与文本消息的"排队补发"（`chat::flush_queued`）是同一批，理由也一样：
/// **链路在的时候不主动对账，回执丢了就永远对不上账**。回执（进度/完成/拒绝）是"一次、
/// 尽力而为"的 —— 长连接断在"检查有没有会话"与"真的发出去"之间、或那一跳 HTTP 补发失败，
/// 发送方那一台就会一直停在「等待」（带 `await1` 的对端没有时钟，见 `push_job::CAP_AWAIT`），
/// 而对面其实早就拒了、或者早就收完了。
///
/// 幂等 + 节流（`PushStore::ask_due`），所以每 5 秒一拍地叫它不会刷屏；没有待对账的邀请
/// 就什么都不做（连闸门都不动）。只走长连接：对账的前提就是"链路在"。
pub fn reconcile(state: &Arc<AppState>, peer_id: &str) {
    if !state.ws.has_connection(peer_id) {
        return;
    }
    let invite_ids = state.push.waiting_invites_of(peer_id);
    if invite_ids.is_empty() || !state.push.ask_due(peer_id) {
        return;
    }
    let ask_id = uuid::Uuid::new_v4().to_string();
    debug!(%peer_id, count = invite_ids.len(), %ask_id, "推送对账：问对端这些邀请的结果");
    if !state
        .ws
        .send(peer_id, WsEvent::PushAsk { ask_id, invite_ids })
    {
        debug!(%peer_id, "推送对账：问不出去（会话刚断），等下一拍");
    }
}

/// 接收方：如实回答一条对账（`PushAsk`）—— 本机还拿着的答 `waiting`，
/// 处理过的按结局答，没见过的答 `unknown`（= 那条邀请根本没送到）。
pub fn answer_ask(state: &AppState, peer_id: &str, ask_id: &str, invite_ids: &[String]) -> WsEvent {
    let states = invite_ids
        .iter()
        .map(|id| state.push.answer_invite_state(id, peer_id))
        .collect::<Vec<_>>();
    debug!(%peer_id, %ask_id, count = invite_ids.len(), "推送对账：答对端");
    WsEvent::PushState {
        ask_id: ask_id.to_string(),
        states,
    }
}

/// 发送方：把对账的答案落到批次上 —— 这就是"回执丢了"的补漏。
///
/// 只认**属于这台设备**的邀请（答案里塞了别人的 id 就记一句、不落账）。落到批次上的写法
/// 与真实回执完全同路（`batch_set`），所以"本机推测的失败"能被对端的成功纠正，
/// 而"用户取消 / 对端拒绝"这些明确意图改不动（见 `push_job::late_success`）。
pub fn apply_push_states(
    state: &AppState,
    peer_id: &str,
    ask_id: &str,
    states: &[push_job::PushInviteStateWire],
) {
    use push_job::{batch_set, PushStatus, TargetState};
    for s in states {
        let Some(target) = state.push.target_of_invite(&s.invite_id) else {
            // 批次已经收起（整批走完就丢了）或者这条不属于本机发出去的推送。
            continue;
        };
        if target.peer_id != peer_id {
            warn!(%peer_id, invite_id = %s.invite_id, "对账答案里混进了别的设备的邀请，忽略");
            continue;
        }
        match s.state.as_str() {
            "done" => {
                state.push.remove_job(&s.invite_id);
                batch_set(state, &s.invite_id, TargetState::Done, None, None, true);
            }
            "failed" => {
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Failed,
                    None,
                    Some(s.reason.clone().unwrap_or_else(|| "对端接收失败".into())),
                    true,
                );
            }
            "rejected" => {
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Rejected,
                    None,
                    // 用对端给的原因：同样是 `rejected`，"对方拒绝"和"超时未处理"是两件事，
                    // 卡片上那句话该说准（`reject_shared` 两种都走这一档）。
                    Some(
                        s.reason
                            .clone()
                            .unwrap_or_else(|| "对方拒绝了这次推送".into()),
                    ),
                    true,
                );
            }
            "cancelled" => {
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Failed,
                    None,
                    Some("对方取消了接收".into()),
                    true,
                );
            }
            "waiting" => {
                // 对端还拿着这条邀请：未绑定的是"等它点同意"（界面别改口），已配对的它自己
                // 就开收了 —— 那就是"传输中"，卡片该从「等待开始」挪到进度那一档。
                state.push.update_status(&s.invite_id, PushStatus::Accepted);
                if target.paired {
                    batch_set(
                        state,
                        &s.invite_id,
                        TargetState::Sending,
                        Some(s.received),
                        None,
                        false,
                    );
                }
            }
            // 对端没有这条邀请（本机没见过 / 记录已过期）：它根本没送到 —— 当场收手，
            // 不必一直等着（带 `await1` 的对端没有时钟，凭证就在这条回答里）。
            // 发送方随后重试时对端会正常收到。
            other => {
                debug!(invite_id = %s.invite_id, state = %other, "对账：对端没有这条邀请");
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Failed,
                    None,
                    Some("对端没有这条邀请（没送达）".into()),
                    true,
                );
            }
        }
    }
    debug!(%peer_id, %ask_id, "推送对账答案已落账");
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("orbdrop_manifest_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 链接不进去、也不数它里面有多少东西：junction 回环会让走查把同一棵树翻几百遍
    /// （`%LOCALAPPDATA%\Application Data` 就是指向自己父目录的那种），
    /// 而"跳过了 N 项"里的 N 数的是**项**（这个链接本身），不是它背后的文件数。
    #[tokio::test]
    async fn links_are_skipped_not_followed() {
        let dir = temp_dir("link");
        std::fs::create_dir_all(dir.join("真").join("深")).unwrap();
        std::fs::write(dir.join("真").join("深").join("a.txt"), b"hi").unwrap();
        std::fs::write(dir.join("散.txt"), b"xy").unwrap();
        let link = dir.join("回环");
        // 指向自己的父目录 = 真正的回环：跟随它，走查就永远走不完。
        if !crate::unsendable::make_link_for_test(&link, &dir) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let (files, total, skipped) = collect_dir_manifest(&dir).await.unwrap();
        let mut names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["散.txt", "真/深/a.txt"],
            "只该有真实文件，没有回环翻出来的副本"
        );
        assert_eq!(total, 4);
        assert_eq!(skipped, 1, "跳过的是那个链接本身");

        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 顶层源是用户**明确点的**：是链接就当场报错，别静默跳成一个空清单
    /// （那样用户只会看到"没有可推送的文件"，根本不知道自己选错了什么）。
    #[tokio::test]
    async fn top_level_link_source_is_refused() {
        let dir = temp_dir("toplink");
        std::fs::create_dir_all(dir.join("真")).unwrap();
        let link = dir.join("链接");
        if !crate::unsendable::make_link_for_test(&link, &dir.join("真")) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let Err(err) = manifest_for_paths(&[link.clone()]).await else {
            panic!("顶层是链接的源必须当场拒绝");
        };
        assert!(err.contains("链接"), "该说清是链接的问题，实际：{err}");

        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 卷根上的系统文件（`pagefile.sys`…）不能被当成推送源 —— 它读不了，也不该送。
    #[cfg(windows)]
    #[tokio::test]
    async fn root_level_system_file_is_refused() {
        let pagefile = PathBuf::from("C:\\pagefile.sys");
        if tokio::fs::symlink_metadata(&pagefile).await.is_err() {
            return; // 这台机器关掉了分页文件：没有可测的
        }
        let Err(err) = manifest_for_paths(&[pagefile]).await else {
            panic!("卷根上的系统文件必须当场拒绝");
        };
        assert!(err.contains("系统"), "该说清是系统自己的东西，实际：{err}");
    }

    /// 读不到的项**跳过 + 计数**，不能把整批拖垮：用户的目录树里有一个打不开的
    /// 子目录是常事（权限、刚好被删），为此拒绝整批等于什么都传不了。
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_subdir_is_skipped_and_counted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("denied");
        std::fs::create_dir_all(dir.join("看得见")).unwrap();
        std::fs::write(dir.join("看得见").join("a.txt"), b"x").unwrap();
        let shut = dir.join("打不开");
        std::fs::create_dir_all(&shut).unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();

        let (files, _, skipped) = collect_dir_manifest(&dir).await.unwrap();
        assert_eq!(files.len(), 1, "看得见的那份照常进清单");
        assert_eq!(skipped, 1, "打不开的那个子目录算一项");

        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 但**用户点的那个源**读不到仍然整条失败：界面得说出"这个目录读不了"，
    /// 而不是推一个空清单出去。
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_root_still_fails_the_whole_source() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("denied_root");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let Err(err) = manifest_for_paths(&[dir.clone()]).await else {
            panic!("源目录读不到必须整条失败");
        };
        assert!(err.contains("读取目录失败"), "实际：{err}");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 清单的形状：目录里混着链接、读不到的项时，**计数照实回到 `PushSource`** ——
    /// 卡片上"已跳过 N 项"读的就是它，数字错了用户就不知道清单少了什么。
    #[tokio::test]
    async fn skipped_survives_into_the_source() {
        let dir = temp_dir("count");
        std::fs::create_dir_all(dir.join("子")).unwrap();
        std::fs::write(dir.join("子").join("a.txt"), b"x").unwrap();
        let link = dir.join("链接");
        if !crate::unsendable::make_link_for_test(&link, &dir.join("子")) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let source = manifest_for_paths(&[dir.clone()]).await.unwrap();
        assert_eq!(source.skipped, 1);
        assert_eq!(source.files.len(), 1);
        assert!(source.is_dir, "有清单的推送才是目录语义");

        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── 覆盖名（F-119）：对端看到的名字与本机读字节的路径 ────────────────────

    /// 覆盖名只改**清单里的那一个词**：`files` 写它、`sources` 仍指着源路径。
    ///
    /// 这条钉的是"发送应用"那条路的地基：源是设备上某个 `/data/app/…/base.apk`（只读、
    /// 不会改名、也不会拷一份出来），对端该看到的是应用自己的名字。
    #[tokio::test]
    async fn an_override_only_changes_the_name_the_peer_sees() {
        let dir = temp_dir("rename");
        let apk = dir.join("base.apk");
        std::fs::write(&apk, b"fake-apk").unwrap();
        let shown = "微信_8.0.50(2600)_com.tencent.mm.apk";

        let items = vec![(apk.clone(), Some(shown.to_string()))];
        let source = manifest_for_named_paths(&items).await.unwrap();

        // 单文件推送：清单名就是落盘名（对端只认名字与大小），所以 `name` 也得是覆盖名 ——
        // 少了这一处，对端收到的会是一条叫 `base.apk` 的文件。
        assert_eq!(source.name, shown);
        assert_eq!(source.files.len(), 1);
        assert_eq!(source.files[0].path, shown);
        assert_eq!(source.files[0].size, 8);
        assert!(!source.is_dir, "单文件推送没有清单可言");

        // 本机那份翻译表仍指着真的那份文件（下载端点每次请求都按它开文件）。
        let real = source
            .sources
            .get(shown)
            .expect("翻译表要按**清单里的名字**建键");
        assert_eq!(std::fs::read(real).unwrap(), b"fake-apk");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 脏覆盖名进不来：净化在对端落盘那关是**必需**的（`:` 会让整次接收判失败），
    /// 净化后为空 = 没有覆盖名，退回源自己的名字。
    #[tokio::test]
    async fn dirty_override_names_are_cleaned_or_dropped() {
        let dir = temp_dir("rename_dirty");
        let a = dir.join("base.apk");
        std::fs::write(&a, b"a").unwrap();

        let items = vec![(a.clone(), Some("X: 编辑器.apk".to_string()))];
        let source = manifest_for_named_paths(&items).await.unwrap();
        assert_eq!(source.files[0].path, "X_ 编辑器.apk");

        let items = vec![(a.clone(), Some("...".to_string()))];
        let source = manifest_for_named_paths(&items).await.unwrap();
        assert_eq!(source.files[0].path, "base.apk", "洗成空串就该退回源名");
        assert_eq!(source.name, "base.apk");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 两个源带同一个覆盖名时**照样去重**（编号插在扩展名前面）：清单里的路径必须唯一，
    /// 否则对端两条清单项落到同一个路径上，谁覆盖谁看运气。
    #[tokio::test]
    async fn duplicate_override_names_are_deduped() {
        let dir = temp_dir("rename_dup");
        let a = dir.join("one.apk");
        let b = dir.join("two.apk");
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"2").unwrap();

        let items = vec![
            (a.clone(), Some("同名.apk".to_string())),
            (b.clone(), Some("同名.apk".to_string())),
        ];
        let source = manifest_for_named_paths(&items).await.unwrap();
        assert_eq!(source.files[0].path, "同名.apk");
        assert_eq!(source.files[1].path, "同名 (2).apk");
        assert_eq!(source.sources.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── 混版本：对面回的那句"拒绝"该怎么说 ────────────────────────────────────

    /// `accepted:false` 而且**没有 `pending`** = 对端不认识 `await1`（老版本）。
    ///
    /// 老对端那句拒绝分不出"用户点了拒绝"与"60 秒到点自动作废"（它不带 `reason`）——
    /// 所以卡片上不能写死"对方拒绝"，得如实说"对面是老版本、按老规矩办"，让人知道
    /// 该往哪边走（升级对端）。对端**给了**原因就原话用它。
    #[test]
    fn a_synchronous_refusal_names_an_old_peer() {
        let legacy = InviteDecision {
            accepted: false,
            pending: false,
            reason: None,
        };
        let why = refusal_reason("PC-UB", &legacy);
        assert!(
            why.contains("老版本"),
            "老对端要说清是老版本，别写死「对方拒绝」：{why}"
        );
        assert!(why.contains("PC-UB"), "卡片上要认得出是哪一台：{why}");

        let with_reason = InviteDecision {
            accepted: false,
            pending: false,
            reason: Some("落盘目录不可用".into()),
        };
        assert_eq!(
            refusal_reason("PC-UB", &with_reason),
            "落盘目录不可用",
            "对端给了原因就原话用它"
        );
    }

    // ── 对账：回执丢了怎么办（`PushAsk` / `PushState`）────────────────────────

    /// 一台临时状态：对账的落账要真的批次与真的 store。
    fn reconcile_state() -> Arc<AppState> {
        let dir =
            std::env::temp_dir().join(format!("orbdrop_push_reconcile_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(AppState::new(
            crate::config::Config::default(),
            dir.join("config.toml"),
        ))
    }

    fn batch_with(targets: Vec<push_job::BatchTarget>) -> push_job::PushBatch {
        push_job::PushBatch {
            id: "b1".into(),
            name: "a.txt".into(),
            count: 1,
            skipped: 0,
            per_target_total: 10,
            targets,
            last_emit: Instant::now(),
        }
    }

    fn target_of(peer: &str, invite: &str, state: push_job::TargetState) -> push_job::BatchTarget {
        push_job::BatchTarget {
            peer_id: peer.into(),
            peer_name: peer.into(),
            invite_id: invite.into(),
            transport: crate::config::PeerTransport::OrbDrop,
            paired: true,
            state,
            received: 0,
            error: None,
        }
    }

    /// 一整个来回：发送方问 → 接收方按**自己的记录**答 → 发送方落账 —— 这就是"回执丢了"的补漏。
    /// 两端在这一条里是同一个进程里的两个 `AppState`（生产里是两个进程，协议一样）。
    #[tokio::test]
    async fn reconcile_round_trip_lands_the_receivers_answer() {
        // 发送方：一条还没落终态的邀请（回执丢了，界面还停在「等待」）
        let sender = reconcile_state();
        sender.push.insert_batch(batch_with(vec![target_of(
            "p1",
            "i1",
            push_job::TargetState::Invited,
        )]));
        let ids = sender.push.waiting_invites_of("p1");
        assert_eq!(ids, vec!["i1".to_string()], "要问的就是它");

        // 接收方：这条邀请它早就处理完了（结局记录在案）
        let receiver = reconcile_state();
        receiver.push.record_outcome("i1", "p1", "done", 1024, None);

        // 接收方答（对端 id 逐条校验）
        let answer = answer_ask(&receiver, "p1", "ask-1", &ids);
        let WsEvent::PushState { states, ask_id } = answer else {
            panic!("对账的答案是 PushState");
        };
        assert_eq!(ask_id, "ask-1");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].state, "done");
        assert_eq!(states[0].received, 1024);

        // 发送方落账：那条「等待」变成「已成功」，会话气泡跟着改口（`push://batch` 全量快照）
        apply_push_states(&sender, "p1", "ask-1", &states);
        let target = sender.push.target_of_invite("i1").unwrap();
        assert_eq!(target.state, push_job::TargetState::Done);
        assert!(
            sender.push.batch_snapshot("b1").is_some(),
            "卡片还在（宽限期）"
        );
    }

    /// 本机判的"失败"（超时/送不出去）是**推测**，对账拿到 `done` 就该改口 ——
    /// 对面收完了、本机写着失败，那是假账。
    #[tokio::test]
    async fn reconcile_corrects_a_speculative_failure() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of(
            "p1",
            "i1",
            push_job::TargetState::Invited,
        )]));
        // 本机先把这一台**推测**成失败（邀请送不出去 / 对账答 `unknown` 都落这一档）
        push_job::batch_set(
            &state,
            "i1",
            push_job::TargetState::Failed,
            None,
            Some("对方未响应".into()),
            true,
        );
        assert_eq!(
            state.push.target_of_invite("i1").unwrap().state,
            push_job::TargetState::Failed
        );

        // 对端其实收完了：对账答案（正常情况下这条是回执，丢了才需要问）
        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "done".into(),
                received: 10,
                reason: None,
            }],
        );

        let target = state.push.target_of_invite("i1").unwrap();
        assert_eq!(
            target.state,
            push_job::TargetState::Done,
            "推测的失败被对端的成功改写"
        );
        assert!(target.error.is_none(), "改口时要把上一次的原因抹掉");
    }

    /// 但"当事人明确表达的"不能被改写：对面拒绝过的那台，不会因为一条 `done` 就变成成功
    /// （终态只落一次的纪律，见 `push_job::late_success`）。
    #[tokio::test]
    async fn reconcile_never_overwrites_an_explicit_rejection() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of(
            "p1",
            "i1",
            push_job::TargetState::Invited,
        )]));
        push_job::batch_set(
            &state,
            "i1",
            push_job::TargetState::Rejected,
            None,
            Some("对方拒绝".into()),
            true,
        );

        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "done".into(),
                received: 10,
                reason: None,
            }],
        );

        assert_eq!(
            state.push.target_of_invite("i1").unwrap().state,
            push_job::TargetState::Rejected
        );
    }

    /// 对端说"没见过这条邀请" = 那条邀请根本没送到：当场收手，不用一直等着。
    #[tokio::test]
    async fn reconcile_fails_an_invite_the_peer_never_saw() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of(
            "p1",
            "i1",
            push_job::TargetState::Invited,
        )]));
        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "unknown".into(),
                received: 0,
                reason: None,
            }],
        );
        let target = state.push.target_of_invite("i1").unwrap();
        assert_eq!(target.state, push_job::TargetState::Failed);
        assert!(target.error.unwrap().contains("没送达"));
    }

    /// 答案里混进别的设备的邀请：不落账（对端的答案不能动到第三台那行）。
    #[tokio::test]
    async fn reconcile_ignores_invites_of_other_peers() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of(
            "p2",
            "i1",
            push_job::TargetState::Invited,
        )]));
        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "done".into(),
                received: 10,
                reason: None,
            }],
        );
        assert_eq!(
            state.push.target_of_invite("i1").unwrap().state,
            push_job::TargetState::Invited,
            "不是这台设备的答案，一个字都不该动"
        );
    }

    /// 只问"还没落终态"的那几条，而且同一台设备有节流（每 5 秒一拍地叫不会刷屏）。
    #[tokio::test]
    async fn reconcile_asks_only_about_waiting_invites_and_is_throttled() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![
            target_of("p1", "waiting", push_job::TargetState::Invited),
            target_of("p1", "done", push_job::TargetState::Done),
            target_of("p2", "other-peer", push_job::TargetState::Invited),
        ]));
        let (tx, mut rx) = mpsc::channel(crate::ws::EVENT_QUEUE_CAPACITY);
        state.ws.register("p1", tx);

        reconcile(&state, "p1");
        let event = rx.try_recv().expect("问出去了一条 PushAsk");
        match event {
            WsEvent::PushAsk { invite_ids, .. } => {
                assert_eq!(
                    invite_ids,
                    vec!["waiting".to_string()],
                    "只问还没落终态的那条"
                );
            }
            other => panic!("期望 PushAsk，得到 {}", other.type_name()),
        }
        // 节流：立刻再叫一次不该再问（回执丢了靠下一拍，20 秒后才又问）
        reconcile(&state, "p1");
        assert!(rx.try_recv().is_err(), "20 秒内不重复问");
    }

    /// 没有长连接就不问：对账的前提是"链路在"（问出去也得有人答）。
    #[tokio::test]
    async fn reconcile_does_nothing_without_a_link() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of(
            "p1",
            "i1",
            push_job::TargetState::Invited,
        )]));
        reconcile(&state, "p1"); // 没注册会话 → 早退，不发也不占闸门
        assert!(state.push.ask_due("p1"), "没问过就不该占掉闸门");
    }

    /// 回执的最后一道兜底：**"塞进 WS 队列"失败也要退回 HTTP**（以前只看"有没有会话"，
    /// 于是"会话还在但已经写不进去"这一档会静默丢回执）。这里用"注册过、但接收端已经丢掉"
    /// 的会话把这个分支逼出来。
    ///
    /// 对端地址是 `https://`（`net::peer_base`），所以监听端看到的是 TLS 握手的第一帧 ——
    /// 这已经足够证明"回执确实往那个地址发了一次"（要连出握手，先得拨号成功）。
    #[tokio::test]
    async fn report_falls_back_to_http_when_the_ws_queue_is_dead() {
        let state = reconcile_state();
        let (tx, rx) = mpsc::channel(crate::ws::EVENT_QUEUE_CAPACITY);
        state.ws.register("p1", tx);
        drop(rx); // 会话还在册，但队列已经没人接

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                .await
                .unwrap();
            buf[..n].to_vec()
        });

        push_job::report_to_sender(
            &state,
            "p1",
            &Some(("127.0.0.1".into(), port)),
            WsEvent::PushDone {
                invite_id: "i1".into(),
                status: "completed".into(),
                reason: None,
            },
        );

        let head = tokio::time::timeout(Duration::from_secs(5), seen)
            .await
            .expect("5 秒内该向回调地址发出回执")
            .unwrap();
        assert_eq!(
            head.first(),
            Some(&0x16),
            "TLS 握手：确实拨到了那个地址，head={head:?}"
        );

        // 有活儿干的会话不该走这条路：回执进的是 WS 队列。
        let (tx2, mut rx2) = mpsc::channel(crate::ws::EVENT_QUEUE_CAPACITY);
        state.ws.register("p2", tx2);
        push_job::report_to_sender(
            &state,
            "p2",
            &Some(("127.0.0.1".into(), 1)), // 故意指向一个没人听的端口：走了 HTTP 就会连不上
            WsEvent::PushDone {
                invite_id: "i2".into(),
                status: "completed".into(),
                reason: None,
            },
        );
        assert!(
            matches!(rx2.try_recv(), Ok(WsEvent::PushDone { .. })),
            "有会话就走 WS"
        );
    }
}
