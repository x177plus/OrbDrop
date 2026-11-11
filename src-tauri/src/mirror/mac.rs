//! macOS 的对端项拖出：**先整份暂存，再按真实路径交给 `NSDraggingSession`**。
//!
//! 为什么不是 Windows 那种「占位符 + 按需水合」：
//!
//! - macOS 没有 CFAPI。File Provider（`NSFileProviderReplicatedExtension`）是公开的按需文件
//!   方案，但它要求一个独立 app extension target、App Group、单独签名与用户授权，单文件
//!   Tauri 应用装不下这一整套；
//! - macFUSE 要用户额外安装内核扩展/系统扩展，不能作为默认路径；
//! - `NSFilePromiseProvider` 可以让 Finder 在落下时才来要字节，但只对认 file promise 的目标
//!   有效（Finder / 邮件支持，很多第三方应用不支持），目录承诺的行为也不统一。本应用的一条
//!   硬约定是「交给目标的是真实路径」—— 只认路径的应用（微信这类）必须也能收到。
//!
//! 所以这里选**最笨但到处都能用**的那条：起拖前把这一项下载到镜像目录，返回普通路径。
//! 代价与取舍：
//!
//! - 大文件/大目录会在「请按住不放」那一刻等下载完成；这是 macOS 与 Windows 的已知差异，
//!   界面文案与 `docs/macos.md` 都写明了；
//! - 磁盘上会留一份真实副本（Windows 是占位符）；回收与 Windows 共用 [`super::sweep`]，
//!   按**本地放入时刻**算 TTL（24 小时），不碰正在被目标复制的那一份（暂存名带唯一后缀，
//!   同一项重拖也走新路径，绝不会在目标读取时覆盖/删除它）；
//! - 同一项在 [`REUSE_WINDOW`] 内重拖直接复用上次的路径，避免「手松早了」重拖又下一遍。
//!
//! 路径安全与 Windows 同源：设备 id / 共享项 id 走 [`identity::share_dir`] 的白名单，
//! 远端名字走 [`crate::peer_fs::plain_component`]，最终落点必须在镜像根之下。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use uuid::Uuid;

use super::{identity, note_placed, MirroredItem};
use crate::peer_fs::{self, PeerFile};
use crate::state::AppState;

/// 同一项的重拖窗口：窗口内命中缓存，直接返回上次暂存的那份。
const REUSE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// 一次递归最多搬多少项 / 多深。防的是对端回一棵无限/异常巨大的树把本机拖死；
/// 正常共享目录离这两条线很远。
const MAX_ENTRIES: usize = 200_000;
const MAX_DEPTH: usize = 64;

/// 最近暂存的 `(坐标 → 本地路径 + 对端元数据)`。只在本进程内，进程退出即失效（文件本身按 TTL 回收）。
#[derive(Clone)]
struct Recent {
    path: PathBuf,
    at: Instant,
    /// 首次暂存时问到的对端元数据；写回拿它做冲突判据。
    meta: Option<peer_fs::EntryMeta>,
    is_dir: bool,
}

static RECENT: LazyLock<Mutex<HashMap<String, Recent>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 把一项从对端落到镜像目录，返回可以交给 Finder 的真实路径（连同对端元数据）。
pub async fn place_item(
    state: &Arc<AppState>,
    item: &MirroredItem,
) -> Result<super::PlacedItem, String> {
    let dir = identity::share_dir(&super::root(), &item.peer_id, &item.share_id)?;
    let (base, token) = peer_fs::coords(state, &item.peer_id)?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("建镜像目录失败（{}）: {e}", dir.display()))?;

    let key = format!("{}\0{}\0{}", item.peer_id, item.share_id, item.path);
    // 先问对端元数据再决定能否复用缓存：缓存只保证"同一个远端版本"的暂存副本，
    // 对端改了就必须换新槽重新下载（否则重新打开拿到的还是旧内容）。问不到时若缓存还在，
    // 退回缓存（离线也能看最近那份）；在线后自动任务会发现对端版本变化并提示。
    let client = peer_fs::client_for(state, &item.peer_id)?;
    let meta = match peer_fs::probe_entry(&client, &base, &token, &item.share_id, &item.path).await
    {
        Ok(meta) => meta,
        Err(e) => {
            if let Some(cached) = cached(&key) {
                warn!(error = %e, path = %cached.path.display(), "问不到对端元数据，复用最近的暂存副本");
                return Ok(cached);
            }
            return Err(e);
        }
    };
    if let Some(cached) = cached(&key) {
        let remote_size = meta
            .as_ref()
            .and_then(|m| (!m.is_dir).then_some(m.size));
        let remote_mtime = meta.as_ref().and_then(|m| m.mtime);
        let remote_rev = meta.as_ref().and_then(|m| m.rev.as_deref());
        if (cached.size, cached.mtime) == (remote_size, remote_mtime)
            && remote_rev.map_or(true, |rev| cached.rev.as_deref() == Some(rev))
        {
            info!(path = %cached.path.display(), "复用刚暂存过的对端项（远端版本未变）");
            return Ok(cached);
        }
        forget_cached(&key);
        info!("对端已更新，丢弃旧暂存并重新下载");
    }

    let is_dir = meta
        .as_ref()
        .map(|m| m.is_dir)
        .unwrap_or(item.is_dir);
    let expected = meta
        .as_ref()
        .and_then(|m| (!m.is_dir).then_some(m.size));

    let desired = identity::child_path(&dir, &item.name)?;
    let name = desired
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| "镜像落点没有名字".to_string())?;
    // 每次拖拽一个**独立的槽目录**，文件名仍是原名：目标还捏着上一次那份在复制时，
    // 覆盖/删除它会报「源文件不可用」；把槽目录做成唯一就能既不复用旧文件、也不给
    // 用户拖出去的文件名加 ` (2)`。回收按槽目录记账（`note_placed` 在槽上）。
    let slot = dir.join(Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&slot)
        .await
        .map_err(|e| format!("建暂存目录失败（{}）: {e}", slot.display()))?;
    let local = slot.join(&name);
    let temp = slot.join(format!(".{name}.orbdrop-part"));

    let result = if is_dir {
        download_tree(&client, &base, &token, &item.share_id, &item.path, &temp).await
    } else {
        download_file(
            &client,
            &base,
            &token,
            &item.share_id,
            &item.path,
            &temp,
            expected,
        )
        .await
    };
    if let Err(e) = result {
        cleanup(&slot).await;
        return Err(e);
    }
    if let Err(e) = tokio::fs::rename(&temp, &local).await {
        cleanup(&slot).await;
        return Err(format!("暂存落位失败（{}）: {e}", local.display()));
    }

    // 记账点在**槽目录**上：`mirror::sweep` 扫的是 `<peer>/<share>/<top>` 这一层，
    // 而且整槽一起回收，不会只删掉文件留下空目录。
    note_placed(&slot, b"macos-staging");
    let mtime = meta.as_ref().and_then(|m| m.mtime);
    let mtime_ms = meta.as_ref().and_then(|m| m.mtime_ms);
    let rev = meta.as_ref().and_then(|m| m.rev.clone());
    remember(key, local.clone(), meta, is_dir);
    info!(
        path = %local.display(),
        is_dir, "对端项已完整暂存，按真实路径拖出"
    );
    Ok(super::PlacedItem {
        path: local,
        size: expected,
        mtime,
        mtime_ms,
        rev,
        is_dir,
    })
}

/// 「打开」用的稳定工作副本：同一个远端路径永远落在
/// `<镜像根>/<peer>/<share>/work/<相对路径>`，远端版本变化时原地替换，不换槽。
///
/// 与 [`place_item`] 的分工：拖出要“每次一个新槽”，因为 Finder/目标程序可能还捏着上一次
/// 那份在复制；打开要的是“同一个文件”，否则编辑器、最近文件列表、写回登记全会跟着漂。
pub async fn open_item(
    state: &Arc<AppState>,
    item: &MirroredItem,
) -> Result<super::PlacedItem, String> {
    let dir = identity::share_dir(&super::root(), &item.peer_id, &item.share_id)?;
    let work_root = dir.join("work");
    tokio::fs::create_dir_all(&work_root)
        .await
        .map_err(|e| format!("建工作副本目录失败（{}）: {e}", work_root.display()))?;
    let local = super::writeback::stable_local_path(
        &item.peer_id,
        &item.share_id,
        &item.path,
        &item.name,
    )?;
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("建工作副本目录失败（{}）: {e}", parent.display()))?;
    }

    let (base, token) = peer_fs::coords(state, &item.peer_id)?;
    let client = peer_fs::client_for(state, &item.peer_id)?;
    let meta = match peer_fs::probe_entry(&client, &base, &token, &item.share_id, &item.path).await {
        Ok(meta) => meta,
        Err(e) => {
            // 对端暂时问不到：本地已有副本就先用着（自动任务会在链路回来后重新探测）；
            // 没有就只能如实失败。
            if local.exists() {
                warn!(error = %e, path = %local.display(), "问不到对端元数据，复用本地工作副本");
                let (size, mtime_ms) = super::writeback::local_snapshot(&local);
                return Ok(super::PlacedItem {
                    path: local,
                    size,
                    mtime: mtime_ms.map(|ms| ms / 1000),
                    mtime_ms,
                    rev: None,
                    is_dir: false,
                });
            }
            return Err(e);
        }
    };
    if meta.as_ref().is_some_and(|m| m.is_dir) {
        return Err("当前平台只能直接打开文件；目录请先下载".to_string());
    }
    let expected = meta.as_ref().map(|m| m.size);
    let rev = meta.as_ref().and_then(|m| m.rev.clone());
    let mtime = meta.as_ref().and_then(|m| m.mtime);
    let mtime_ms = meta.as_ref().and_then(|m| m.mtime_ms);

    // 同一个远端版本已经落过盘：直接复用，不重新下载。自动刷新只在 rev 变化时才调
    // 这里，所以“版本相同”只会出现在用户重复打开同一条目时。
    if local.exists() {
        if let Some(opened) =
            super::writeback::find_opened(&item.peer_id, &item.share_id, &item.path)
        {
            if !super::writeback::local_changed(&opened)
                && match (rev.as_deref(), opened.rev.as_deref()) {
                    (Some(remote), Some(base)) => remote == base,
                    // 任一侧不知道版本：宁可重新下载，也不把旧内容当成最新。
                    _ => false,
                }
            {
                let (size, local_mtime_ms) = super::writeback::local_snapshot(&local);
                return Ok(super::PlacedItem {
                    path: local,
                    size: size.or(expected),
                    mtime: local_mtime_ms.map(|ms| ms / 1000).or(mtime),
                    mtime_ms: local_mtime_ms.or(mtime_ms),
                    rev,
                    is_dir: false,
                });
            }
        }
    }

    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| item.name.clone());
    let temp = local
        .parent()
        .unwrap_or(&work_root)
        .join(format!(".{name}.orbdrop-part"));
    let existed_before = local.exists();
    let before = super::writeback::local_snapshot(&local);
    download_file(
        &client,
        &base,
        &token,
        &item.share_id,
        &item.path,
        &temp,
        expected,
    )
    .await?;
    // 下载期间用户又保存了：**不能**把新内容 rename 到目标路径上。先把临时文件丢掉，
    // 让调用方按冲突处理；本地内容原样保留。
    if existed_before {
        let after = super::writeback::local_snapshot(&local);
        if after != before {
            let _ = std::fs::remove_file(&temp);
            return Err("本地工作副本在刷新期间被修改；已保留本地内容，未覆盖".into());
        }
    }
    super::replace_file_in_place(&temp, &local).await?;
    // 记账点放在 work 根：回收扫描按 `<share>/work` 这一层看；真正保护它的是写回登记。
    note_placed(&work_root, b"work-copy");
    info!(
        path = %local.display(),
        refresh = true,
        "对端文件已落成稳定工作副本"
    );
    Ok(super::PlacedItem {
        path: local,
        size: expected,
        mtime,
        mtime_ms,
        rev,
        is_dir: false,
    })
}

/// 递归下载一个目录。用显式栈而不是递归 async fn：深度由对端决定，递归会直接吃爆调用栈。
async fn download_tree(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    root_path: &str,
    dest: &Path,
) -> Result<(), String> {
    let mut stack: Vec<(String, PathBuf, usize)> =
        vec![(root_path.to_string(), dest.to_path_buf(), 0)];
    let mut count = 0usize;

    while let Some((remote, local, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(format!("目录层级超过 {MAX_DEPTH}，已停止（{remote}）"));
        }
        tokio::fs::create_dir_all(&local)
            .await
            .map_err(|e| format!("建暂存目录失败（{}）: {e}", local.display()))?;
        let entries = peer_fs::fetch_tree(client, base, token, share_id, &remote).await?;
        for entry in entries {
            count += 1;
            if count > MAX_ENTRIES {
                return Err(format!("目录项超过 {MAX_ENTRIES} 个，已停止"));
            }
            // 远端路径用**对端给的原名**（服务端按它解析）；本地落点用清洗过的单段名。
            let remote_child = peer_fs::join_slash(&remote, &entry.name);
            let Some(name) = peer_fs::plain_component(&entry.name) else {
                warn!(name = %entry.name, "跳过对端给的非法文件名");
                continue;
            };
            // 对端两个名字清洗后可能撞成同一个本地名（`a:b` / `a_b`）：撞了就让本地名带序号，
            // 不覆盖已经下载的那一项。
            let child = crate::config::unique_path(&local, &name);
            if entry.is_dir {
                stack.push((remote_child, child, depth + 1));
            } else {
                download_file(
                    client,
                    base,
                    token,
                    share_id,
                    &remote_child,
                    &child,
                    Some(entry.size),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// 下一份文件：按 `READ_CHUNK` 循环读到短读；`expected` 有值时逐块对账，最后必须严丝合缝。
async fn download_file(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    remote: &str,
    dest: &Path,
    expected: Option<u64>,
) -> Result<(), String> {
    let file = PeerFile {
        base: base.to_string(),
        token: token.to_string(),
        share_id: share_id.to_string(),
        path: remote.to_string(),
    };
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("建暂存目录失败（{}）: {e}", parent.display()))?;
    }
    let mut out = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("建暂存文件失败（{}）: {e}", dest.display()))?;
    let mut offset = 0u64;
    loop {
        let chunk = peer_fs::fetch_file_range(client, &file, offset, peer_fs::READ_CHUNK).await?;
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
        if let Some(expected) = expected {
            if offset > expected {
                return Err(format!(
                    "对端「{remote}」回的字节比它自己报的大小多（> {expected}），已中止"
                ));
            }
        }
        out.write_all(&chunk)
            .await
            .map_err(|e| format!("写暂存文件失败（{}）: {e}", dest.display()))?;
        if chunk.len() < peer_fs::READ_CHUNK as usize {
            break;
        }
    }
    out.flush()
        .await
        .map_err(|e| format!("刷新暂存文件失败（{}）: {e}", dest.display()))?;
    drop(out);
    if let Some(expected) = expected {
        if offset != expected {
            return Err(format!(
                "「{remote}」大小对不上（对端报 {expected}，实际收到 {offset}）"
            ));
        }
    }
    Ok(())
}

async fn cleanup(path: &Path) {
    let removed = if path.is_dir() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    };
    if let Err(e) = removed {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "清理失败的暂存项没清掉");
        }
    }
}

fn forget_cached(key: &str) {
    RECENT.lock().remove(key);
}

fn cached(key: &str) -> Option<super::PlacedItem> {
    let mut map = RECENT.lock();
    let hit = map
        .get(key)
        .map(|recent| (recent.clone(), recent.at.elapsed() < REUSE_WINDOW));
    match hit {
        Some((recent, true)) if recent.path.exists() => Some(super::PlacedItem {
            path: recent.path,
            size: recent
                .meta
                .as_ref()
                .and_then(|m| (!m.is_dir).then_some(m.size)),
            mtime: recent.meta.as_ref().and_then(|m| m.mtime),
            mtime_ms: recent.meta.as_ref().and_then(|m| m.mtime_ms),
            rev: recent.meta.as_ref().and_then(|m| m.rev.clone()),
            is_dir: recent.is_dir,
        }),
        Some(_) => {
            map.remove(key);
            None
        }
        None => None,
    }
}

fn remember(key: String, path: PathBuf, meta: Option<peer_fs::EntryMeta>, is_dir: bool) {
    let mut map = RECENT.lock();
    // 顺手清掉过期项；正常一次会话不会堆太多。
    map.retain(|_, recent| recent.at.elapsed() < REUSE_WINDOW);
    map.insert(
        key,
        Recent {
            path,
            at: Instant::now(),
            meta,
            is_dir,
        },
    );
}