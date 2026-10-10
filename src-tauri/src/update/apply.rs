//! 用新 exe 覆盖当前 exe（单文件桌面包的自更新落地）。
//!
//! # 为什么需要一个"helper 进程"
//!
//! Windows 上**运行中的 exe 不能被写**：进程映像还映射着那个文件，任何覆盖它的尝试都是
//! 共享冲突（不是权限问题，关掉杀软也一样）。所以"自己覆盖自己"这件事必须由一个**别的
//! 进程**来做，而且那个进程不能在同一个文件上跑。
//!
//! 我们不额外发一个更新器（那就不是单文件了），而是让**刚下载好的新 exe 自己**当 helper：
//!
//! 1. 旧进程把新 exe 下到 `%TEMP%\orbdrop-update-<版本>\orbdrop.exe`；
//! 2. 旧进程带上 `--orbdrop-apply-update --target <当前 exe> --pid <自己>` 把**新 exe**
//!    拉起来（它就是 helper，运行在临时目录里，跟目标文件不是同一个文件）；
//! 3. 旧进程退出 → 目标 exe 的文件锁随之释放；
//! 4. helper 等旧进程真的没了，把旧 exe 改名成 `.old`、把**自己**拷到目标路径、再把目标
//!    拉起来，最后删掉 `.old` 与临时目录。
//!
//! 这样落地后**依旧只有一个文件**（`.old` 与临时目录都是过渡态，见 [`cleanup_stale`]）。
//!
//! # 失败也要能退回去
//!
//! 每一步失败都尽量把旧 exe 恢复成原样并把它拉起来 —— 用户宁可停留在旧版本，也不能落到
//! "一个 exe 都没有"的地步。所有失败都写进 `<数据目录>/logs/update.log`：helper 阶段没有
//! 控制台（release 是 `windows_subsystem = "windows"`），日志是唯一的出口。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// helper 的入口标记。带它启动 = "我是来覆盖并重启的，别初始化应用"。
const APPLY_FLAG: &str = "--orbdrop-apply-update";
/// 等旧进程退出的上限：正常退出是毫秒级，等这么久还没死就是卡住了（此时硬试拷贝）。
const WAIT_EXIT: Duration = Duration::from_secs(30);
/// 拷贝重试：旧进程刚退出那一瞬间，杀软/索引器可能还捏着文件，重试能救回这一类。
const COPY_RETRIES: usize = 24;
const COPY_RETRY_GAP: Duration = Duration::from_millis(250);
/// 临时目录的保质期：超过它的 `%TEMP%\orbdrop-update-*` 一律当垃圾清掉。
const TEMP_TTL: Duration = Duration::from_secs(3600);

/// 覆盖用的备份后缀。取"追加"而不是 `with_extension`：`orbdrop.exe` → `orbdrop.exe.old`
/// 一眼看得出是谁的备份，也不会把 exe 的扩展名换掉。
const BACKUP_SUFFIX: &str = ".old";

// ---------------------------------------------------------------------------
// 父进程侧
// ---------------------------------------------------------------------------

/// 把 helper（= 刚下好的新 exe）拉起来。返回后父进程应当立刻退出。
pub fn spawn_helper(new_exe: &Path, target: &Path) -> Result<(), String> {
    if !new_exe.is_file() {
        return Err(format!("下载的更新包不在：{}", new_exe.display()));
    }
    let mut cmd = std::process::Command::new(new_exe);
    cmd.arg(APPLY_FLAG)
        .arg("--target")
        .arg(target)
        .arg("--pid")
        .arg(std::process::id().to_string())
        .arg("--self")
        .arg(new_exe);
    // 工作目录钉在临时目录：helper 一会儿要清理它，别让它把当前目录当成自己的家。
    if let Some(dir) = new_exe.parent() {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map_err(|e| {
        format!(
            "拉不起更新程序：{e}（更新包已下好，在 {}，可以手动替换当前 exe）",
            new_exe.display()
        )
    })?;
    Ok(())
}

/// 清掉上一次更新留下的过渡文件：`<exe>.old` 与过期的临时目录。
///
/// helper 自己删不掉正在运行的那个文件与目录（都还被映射着），所以这两件残渣由**下一次
/// 启动**来收 —— 收在启动最早期，别让用户看见。
pub fn cleanup_stale() {
    if let Ok(exe) = std::env::current_exe() {
        let backup = with_suffix(&exe, BACKUP_SUFFIX);
        if backup.exists() {
            match std::fs::remove_file(&backup) {
                Ok(()) => log_line(&format!("清掉上次更新的备份 {}", backup.display())),
                Err(e) => log_line(&format!("删不掉 {}：{e}", backup.display())),
            }
        }
    }
    let temp = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&temp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("orbdrop-update-") {
            continue;
        }
        let path = entry.path();
        // 只清"老人"：时间戳还在变动说明正在用（比如用户刚点了更新但还没重启过来）。
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > TEMP_TTL);
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// helper 侧
// ---------------------------------------------------------------------------

/// 命令行里带 [`APPLY_FLAG`] 时执行覆盖流程。**返回 `true` 表示"我是 helper 且已经干完了"**，
/// 调用方（`main.rs`）必须立刻返回，绝不能再去初始化 Tauri —— 那会拉起第二个托盘图标、
/// 还会和单实例锁打架。
pub fn run_helper_from_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == APPLY_FLAG) {
        return false;
    }
    let opt = |key: &str| -> Option<String> {
        let i = args.iter().position(|a| a == key)?;
        args.get(i + 1).cloned()
    };
    let target = opt("--target").map(PathBuf::from);
    let pid = opt("--pid").and_then(|p| p.parse::<u32>().ok());
    let me = opt("--self").map(PathBuf::from);
    match (target, pid, me) {
        (Some(target), Some(pid), Some(me)) => {
            std::process::exit(helper_main(&target, pid, &me));
        }
        _ => {
            log_line("helper 参数不全，什么都没做");
            std::process::exit(2);
        }
    }
}

/// 返回进程退出码（0 = 新版本已经跑起来了）。
fn helper_main(target: &Path, pid: u32, me: &Path) -> i32 {
    log_line(&format!(
        "helper 开始：等 {pid} 退出后把 {} 覆盖到 {}",
        me.display(),
        target.display()
    ));

    wait_exit(pid, WAIT_EXIT);

    // 1) 旧 exe 改名备份。改名（而不是直接覆盖）换来的是"拷贝失败还能退回去"。
    let backup = with_suffix(target, BACKUP_SUFFIX);
    let _ = std::fs::remove_file(&backup);
    if let Err(e) = std::fs::rename(target, &backup) {
        log_line(&format!("旧 exe 改名失败（{e}），改走直接覆盖"));
    }

    // 2) 把自己拷到目标路径。
    if let Err(e) = copy_retry(me, target) {
        log_line(&format!("覆盖失败：{e}"));
        // 退回去：把备份还原成原路径并把**旧版本**拉起来，别让用户手里一个 exe 都没有。
        if backup.exists() {
            let _ = std::fs::remove_file(target);
            if let Err(e2) = std::fs::rename(&backup, target) {
                log_line(&format!("回滚也失败：{e2}（备份在 {}）", backup.display()));
            }
        }
        if target.exists() {
            let _ = spawn_detached(target);
        }
        cleanup(me);
        return 1;
    }

    // 3) 拉新版本。**先起再删备份**：备份删早了，这一步失败就真没退路了。
    if let Err(e) = spawn_detached(target) {
        log_line(&format!("新版本拉不起来：{e}（文件已就位，手动开一下即可）"));
        cleanup(me);
        return 1;
    }
    let _ = std::fs::remove_file(&backup);
    log_line("新版本已启动");
    cleanup(me);
    0
}

/// 等目标进程退出。
///
/// 拿 `SYNCHRONIZE` 句柄等它 —— 这是唯一"干净"的等法（不轮询、不误判）。进程已经没了时
/// `OpenProcess` 直接失败，也就是"立刻可以覆盖"。
#[cfg(windows)]
fn wait_exit(pid: u32, max: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(handle) => {
                // SAFETY：handle 由 OpenProcess 给出、下面立刻关掉；INFINITE 由 max 兜底
                // （WaitForSingleObject 的毫秒参数给 0xFFFFFFFF 是无限等待，这里给 ms）。
                let _ = WaitForSingleObject(handle, max.as_millis() as u32);
                let _ = CloseHandle(handle);
            }
            Err(_) => log_line(&format!("进程 {pid} 已经不在了，直接覆盖")),
        }
    }
}

/// 非 Windows 上没这条路径（本应用的桌面端只出 Windows），保留一个空实现让代码在别的
/// 平台上照样能编译。真正兜住"文件还没解锁"的是后面的拷贝重试。
#[cfg(not(windows))]
fn wait_exit(_pid: u32, max: Duration) {
    std::thread::sleep(max.min(Duration::from_secs(2)));
}

/// 拷贝并重试：旧进程刚退出时，杀软/搜索索引可能还捏着文件几毫秒到几秒。
fn copy_retry(from: &Path, to: &Path) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..COPY_RETRIES {
        match std::fs::copy(from, to) {
            Ok(_) => {
                // 拷完对一下长度：`copy` 成功但目标被别的进程截断过这种事没道理发生，
                // 可这里是"覆盖用户唯一的可执行文件"，多看一眼值得。
                let a = std::fs::metadata(from).map(|m| m.len()).unwrap_or(0);
                let b = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
                if a == b {
                    return Ok(());
                }
                last = format!("拷贝后大小不一致（{a} ≠ {b}）");
            }
            Err(e) => last = e.to_string(),
        }
        if attempt + 1 < COPY_RETRIES {
            std::thread::sleep(COPY_RETRY_GAP);
        }
    }
    Err(last)
}

/// 起一个新进程并立刻放手（helper 马上要退出，不能等它）。
fn spawn_detached(exe: &Path) -> Result<(), String> {
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 收尾：临时目录（helper 自己就在里面，删不掉的部分留给下次启动）。
fn cleanup(me: &Path) {
    if let Some(dir) = me.parent() {
        if dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("orbdrop-update-"))
        {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => {}
                Err(e) => log_line(&format!("临时目录删不掉（下次启动会清）：{e}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 在文件名后面追加后缀：`orbdrop.exe` → `orbdrop.exe.old`。
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// 追加一行日志。
///
/// 不走 `tracing`：helper 阶段没有初始化过 subscriber（那是 `boot_state` 干的活，helper 特意
/// 不碰），写进去只会石沉大海。直接落文件，路径与应用的日志目录一致。
fn log_line(msg: &str) {
    use std::io::Write as _;
    let dir = crate::config::data_dir().join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("update.log"))
    {
        let _ = writeln!(f, "[{}] {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}