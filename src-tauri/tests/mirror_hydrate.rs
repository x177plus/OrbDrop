//! 镜像（CFAPI）端到端：占位符 → 目标读 → 从对端拉 → 字节正确。
//!
//! 这是这条路唯一值得信的验证：真同步根（临时目录里注册/连接）、真占位符、真 HTTP（本机把自己
//! 当对端）。`FETCH_DATA` 与 `FETCH_PLACEHOLDERS` 两条回调都被真的走一遍 —— 平台把读挂起、
//! 回调我们、`CfExecute` 把字节交回去。
//!
//! 覆盖的关键判据：
//! - 占位符**不占磁盘**（逻辑大小是真实大小，"磁盘上的大小"是 0）；
//! - 水合出来的字节**与对端逐字节相同**，含**非 4 KiB 对齐的末尾**（那是平台唯一允许不对齐的段）；
//! - 目录占位符是"未填充"的：枚举它才触发 `FETCH_PLACEHOLDERS`，子树是那时长出来的；
//! - 退出（`mirror::shutdown`）把镜像树与同步根注册都清掉 —— 测试跑完不在本机留残骸。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;

use orbdrop_lib::config::{Config, PeerDevice, ShareItem};
use orbdrop_lib::credential;
use orbdrop_lib::mirror::{self, MirroredItem};
use orbdrop_lib::state::AppState;

/// 故意的**非** 4 KiB 对齐长度：末尾那一段按文档可以不带对齐，正是容易写错的地方。
const BIG_LEN: usize = 3 * 1024 * 1024 + 1234;

fn big_payload() -> Vec<u8> {
    (0..BIG_LEN).map(|i| (i % 251) as u8).collect()
}

const SUB_A: &[u8] = b"alpha from peer";
const SUB_B: &[u8] = b"beta from peer";

/// "读一页就撒手"那一幕用的文件：太小会整份一次拉完（撤销窗口就没有了），太大又白等。
const CANCEL_LEN: usize = 256 * 1024;

fn cancel_payload() -> Vec<u8> {
    (0..CANCEL_LEN).map(|i| (i % 241) as u8).collect()
}

fn remote_share_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_mirror_src_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("big.bin"), big_payload()).unwrap();
    std::fs::write(dir.join("cancel.bin"), cancel_payload()).unwrap();
    std::fs::write(dir.join("small.txt"), SMALL).unwrap();
    std::fs::write(dir.join("sub").join("a.txt"), SUB_A).unwrap();
    std::fs::write(dir.join("sub").join("b.bin"), SUB_B).unwrap();
    dir
}

const SMALL: &[u8] = b"small enough to drag through the switch test";

fn make_state(data_dir: &std::path::Path, port: u16) -> Arc<AppState> {
    let mut config = Config::default();
    config.port = port;
    config.device_id = format!("device-mirror-{}", uuid::Uuid::new_v4());
    config.device_name = "镜像测试".into();
    config.bc_exe_path = None;
    config.download_dir = data_dir.join("downloads");
    let _ = std::fs::create_dir_all(&config.download_dir);
    Arc::new(AppState::new(
        config,
        data_dir.join("config.toml"),
    ))
}

/// 一个"当下没人监听"的端口：内核分配后立刻放掉（`start_server` 拿 0 当代号会返回 0）。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// `Explorer` 里"磁盘上的大小"就是这个：实际分配的字节数。占位符应当是 0。
fn on_disk_bytes(path: &std::path::Path) -> u64 {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), None) };
    low as u64
}

use std::os::windows::ffi::OsStrExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placeholder_hydrates_from_peer_and_leaves_nothing_behind() {
    // 这条链路是"平台回调 → 我们"的，出问题时日志是唯一的线索（测试里默认不初始化 tracing，
    // 那些 warn 会凭空消失）。`cargo test -- --nocapture` 能看到。
    // 平台回调那几句的 target 是 `orbdrop_lib::mirror::win`（`orbdrop=…` 只按 `::` 前缀匹配，
    // 盖不到它），所以两个都要放进来。
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("orbdrop=debug,orbdrop_lib=debug"))
        .with_writer(std::io::stdout)
        .try_init();

    // 数据目录整包搬走：凭据、身份证书、镜像根全落在这个临时目录里，不碰真实数据。
    let data_dir = std::env::temp_dir().join(format!("orbdrop_mirror_data_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).unwrap();
    std::env::set_var("ORBDROP_DATA_DIR", &data_dir);

    let share_root = remote_share_dir();
    let data_dir_cleanup = data_dir.clone();
    let share_root_cleanup = share_root.clone();
    // 从这一刻起，无论测试怎么结束（包括 panic）都会把镜像与临时目录收干净。
    let _cleanup = Cleanup { data_dir: data_dir_cleanup, share_root: share_root_cleanup };
    let state = make_state(&data_dir, free_port());
    let share = ShareItem::new(share_root.clone());
    let share_id = share.id.clone();
    state.config.lock().shares.push(share);
    let base = state.config.lock().port;
    let port = orbdrop_lib::server::start_server(state.clone(), base)
        .await
        .expect("起服务");

    // 把自己当对端：地址簿一行 + 凭据库里它的 token（回环过来的请求本来也认成本机，但
    // `peer_fs::coords` 要求这两样都在）。
    let peer = format!("device-mirror-peer-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: peer.clone(),
        name: "对端".into(),
        ip: "127.0.0.1".into(),
        port,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false, relay: None,
    });
    credential::set_peer(&peer, "mirror-test-token").unwrap();

    // ---- 1. 放一个文件镜像：只建占位符，零下载 ----
    let item = MirroredItem {
        peer_id: peer.clone(),
        share_id: share_id.clone(),
        path: "big.bin".into(),
        name: "big.bin".into(),
        is_dir: false,
    };
    let local = mirror::place(&state, &item).await.expect("放镜像");

    let meta = std::fs::metadata(&local).expect("占位符应当存在");
    assert_eq!(meta.len() as usize, BIG_LEN, "占位符要报真实大小，否则目标会拷出 0 字节");
    assert_eq!(
        on_disk_bytes(&local),
        0,
        "占位符不该占数据块（镜像的磁盘占用只与条目数有关）"
    );

    // ---- 2. 目标读它：这一步才真的从对端拉 ----
    //
    // 资源管理器是按 **1 MiB 一段**读的（真机日志里 725 段、每段中位 41 ms），所以这里也按段读
    // —— 既验"多次区间水合"的正确性（含不对齐的末尾那一段），也把每段耗时打出来：客户端复用
    // 与否就差在这里（每段新建客户端 = 每段一次 TLS 握手）。
    let chunk_path = local.clone();
    let (got, spans) = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&chunk_path).expect("打开占位符");
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut spans = Vec::new();
        loop {
            let began = std::time::Instant::now();
            let n = f.read(&mut buf).expect("按段读占位符（会触发水合）");
            if n == 0 {
                break;
            }
            spans.push(began.elapsed().as_millis() as u64);
            out.extend_from_slice(&buf[..n]);
        }
        (out, spans)
    })
    .await
    .unwrap();
    assert_eq!(got, big_payload(), "水合出来的字节必须与对端逐字节相同（含不对齐的末尾）");
    println!("按 1 MiB 分段读（首次访问 = 边走网络边水合）：{} 段，耗时 {:?} ms", spans.len(), spans);

    // 再整份读一次：这次应当全在本地（不该再有水合回调）。
    let again_path = local.clone();
    let second = tokio::task::spawn_blocking(move || std::fs::read(&again_path))
        .await
        .unwrap()
        .expect("再读一次");
    assert_eq!(second, big_payload(), "第二次读的字节同样要一致");
    // 水合之后平台会不会把本地副本留在盘上，是策略/平台的账（我们开了 AUTO_DEHYDRATION_ALLOWED，
    // 它可以在读完之后自己脱水）——所以这里只记录、不断言。
    println!("水合后 磁盘上的大小 = {} 字节", on_disk_bytes(&local));

    // ---- 2.5 平台撤销过的那一段，随后还得能拉回来 ----
    //
    // 真实的那条路（实机日志里就是这么卡死的）：资源管理器**落下时**要把 `CF_HDROP` 给出的路径
    // 解析成 shell 条目，读一页文件头就撒手 —— 句柄一关，平台把还在拉的区间撤销掉；紧接着它为
    // 真正的复制**重发**同一页。撤销只许作废它撤销的那**一次**请求：老实现把撤销记成"这个偏移
    // 以后都别拉"，重发的这次就被上一张旧单子静默吞掉 —— 既不交数据也不报复，那个读一直挂在
    // 平台上（目标侧现象：复制进度停在 0%、`DoDragDrop` 也不返回）。
    //
    // 这里用一个**子进程**读一页就退出（它的句柄一关，平台就会撤销还在飞的区间），再用本进程
    // 把整份读回来 —— 撤销过的那段也得拿到字节，断言就这么下。
    let cancel_item = MirroredItem {
        peer_id: peer.clone(),
        share_id: share_id.clone(),
        path: "cancel.bin".into(),
        name: "cancel.bin".into(),
        is_dir: false,
    };
    let cancel_local = mirror::place(&state, &cancel_item).await.expect("放镜像（撤销场景）");
    let probe = cancel_local.to_string_lossy().replace('\'', "''");
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$fs=[IO.File]::Open('{probe}','Open','Read','ReadWrite'); \
                 $b=New-Object byte[] 4096; $fs.Read($b,0,4096)|Out-Null; $fs.Close()"
            ),
        ])
        .status()
        .expect("起子进程读一页就撒手");
    assert!(status.success(), "子进程读一页应当成功（那一页得被交付）");

    // 读回整份。**必须带死线**：老实现下这里会一直挂着（平台那个读没人交付也没人判失败），
    // 而"挂住"正是用户看到的现象 —— 死线把"卡死"变成一条能读的失败信息。
    let read_path = cancel_local.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read(&read_path));
    });
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("撤销之后平台重发的读没被交付：平台的请求一直挂着（= 实机上的卡死）")
        .expect("读回被撤销过的占位符");
    assert_eq!(got, cancel_payload(), "撤销只该作废那次请求，不该影响后来重发的同一段");

    // ---- 3. 目录镜像：目录占位符是"未填充"的，枚举它才长出子树 ----
    let dir_item = MirroredItem {
        peer_id: peer.clone(),
        share_id: share_id.clone(),
        path: "sub".into(),
        name: "sub".into(),
        is_dir: true,
    };
    let dir_local = mirror::place(&state, &dir_item).await.expect("放目录镜像");
    {
        use std::os::windows::fs::MetadataExt;
        let attrs = std::fs::symlink_metadata(&dir_local).expect("目录占位符").file_attributes();
        println!("目录占位符属性 = 0x{attrs:08X}（0x00400010 = RECALL_ON_DATA_ACCESS|DIRECTORY = 未填充）");
    }
    let dir_path = dir_local.clone();
    // 枚举必须由**另一个进程**来做：平台不会为 provider 自己的枚举去请求按需填充（我们自己看到
    // 的只是本地那份（空）视图）。真实场景里枚举的是资源管理器或目标应用 —— 这里用 `cmd /c dir`
    // 顶替它，这样测的才是真的那条路。
    let mut names: Vec<String> = Vec::new();
    for attempt in 0..50 {
        let target = dir_path.to_string_lossy().to_string();
        let out = std::process::Command::new("cmd")
            .args(["/c", "dir", "/b", &target])
            .output()
            .expect("起一个子进程去枚举");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        names = text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        names.sort();
        if !names.is_empty() {
            println!("第 {attempt} 次子进程枚举拿到 {}", names.join(", "));
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(names, vec!["a.txt".to_string(), "b.bin".to_string()], "子树应当按需长出来");

    // 子项的字节也要能读出来（子占位符的身份 blob 指向的是**原始**远端路径）
    let child = dir_local.join("a.txt");
    let child_path = child.clone();
    let child_bytes = tokio::task::spawn_blocking(move || std::fs::read(&child_path))
        .await
        .unwrap()
        .expect("读子占位符");
    assert_eq!(child_bytes, SUB_A);

    // ---- 4. 幂等：同一项再放一次（镜像里已有同名占位符）不能失败 ----
    let again = mirror::place(&state, &item).await.expect("重复放同一项应当幂等");
    assert_eq!(again, local);

    // ---- 5. 父目录链：平台会问"我们自己建出来的普通目录"的填充（实测踩过）----
    //
    // 资源管理器在落下时要把这个路径解析成 shell 条目，途中会碰到镜像里的父目录
    // （`<镜像根>/<peer>/<share>`）—— 那些是我们 `create_dir_all` 出来的**普通目录**，没有身份
    // blob。如果平台去问它们的填充、我们又答不上来，整条路径在目标眼里就"不可用"，
    // **落下后什么都没有**（这正是实机日志里的现象）。所以这里用子进程把父链整个走一遍，
    // 要求每一层都能正常列出来。
    for (label, dir) in [
        ("镜像根", mirror::root()),
        ("peer 目录", mirror::root().join(&peer)),
        ("share 目录", mirror::root().join(&peer).join(&share_id)),
    ] {
        let target = dir.to_string_lossy().to_string();
        let out = std::process::Command::new("cmd")
            .args(["/c", "dir", "/b", &target])
            .output()
            .expect("起子进程");
        assert!(
            out.status.success(),
            "{label} 列不出来（平台来问填充时会答不上来，目标就看不到落点）：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // ---- 6. 镜像不可被共享 / 不可被推送（`unsendable::is_mirror_path`）----
    //
    // 实机踩过：把镜像里的文件拖回面板，它就成了一条共享项（接着 watcher 还挂不上，一行警告）。
    // 镜像里放的是占位符 —— 对端拿到的是一个读不出内容的空壳，所以入口处就得挡住。
    let err = orbdrop_lib::add_share_impl(&state, local.to_string_lossy().to_string())
        .await
        .expect_err("镜像里的文件不该能添加为共享项");
    assert!(err.contains("镜像"), "错误信息要说清是镜像：{err}");
    let err = orbdrop_lib::add_share_impl(&state, mirror::root().to_string_lossy().to_string())
        .await
        .expect_err("镜像根本身也不该能添加为共享项");
    assert!(err.contains("镜像"), "{err}");
    // 数据目录**里面**的东西（这里用真的凭据文件）同样不行：它既不是数据目录本身、也不在
    // 镜像里，只查那两条会把它放过去 —— 而它是全部设备的 token。
    let vault = data_dir.join("credentials.dat");
    assert!(vault.exists(), "测试里应当已经写过凭据（vault）");
    let err = orbdrop_lib::add_share_impl(&state, vault.to_string_lossy().to_string())
        .await
        .expect_err("数据目录里的东西不该能添加为共享项");
    assert!(err.contains("数据目录"), "错误信息要说清是数据目录：{err}");

    // ---- 7. 拖放这条入口：对端项交出去的就是镜像里的本地路径 ----
    let drag_req = |path: &str, name: &str, is_dir: bool| orbdrop_lib::drag::DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id: share_id.clone(),
        path: path.into(),
        name: name.into(),
        is_dir,
    };
    let ticket = state
        .drag
        .prepare(&state, &drag_req("small.txt", "small.txt", false))
        .expect("登记拖拽");
    let paths = wait_payload(&state, &ticket.ticket).await;
    let handed_out = paths[0].clone();
    assert!(
        handed_out.starts_with(mirror::root()),
        "交出去的应当是镜像里的路径：{}",
        handed_out.display()
    );
    let read_path = handed_out.clone();
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&read_path))
        .await
        .unwrap()
        .expect("读镜像路径");
    assert_eq!(bytes, SMALL);

    // 界面的 `is_dir` 可能过时（对端 `/shares` 的旧判断）：**以元数据为准**。
    // 这里把文件说成目录，镜像必须仍按文件建占位符（报真实大小），否则目标会拷出 0 字节。
    let ticket = state
        .drag
        .prepare(&state, &drag_req("small.txt", "small.txt", true))
        .expect("登记拖拽");
    let paths = wait_payload(&state, &ticket.ticket).await;
    let stale = paths[0].clone();
    let meta = std::fs::metadata(&stale).expect("占位符");
    assert!(meta.is_file(), "元数据说是文件，落点就必须是文件：{}", stale.display());
    assert_eq!(meta.len(), SMALL.len() as u64, "而且报的是真实大小");

    // ---- 8. 退出清干净：镜像树与同步根注册都不留 ----
    let mirror_root = mirror::root();
    assert!(mirror_root.exists(), "跑的时候镜像根应当在");
    mirror::shutdown();
    assert!(!mirror_root.exists(), "退出后镜像树应当被清掉");
    assert!(!sync_root_registered(&mirror_root), "退出后不该还挂着同步根注册");

    orbdrop_lib::server::stop_server(&state).await;
    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&share_root);
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 失败（panic）时也要收摊：否则每次跑挂的测试都会在本机留下一个同步根注册 —— 实测踩过这坑，
/// 六个失败运行留下六个（`%TEMP%` 里那几坨，得手动注销）。所以清理挂在一个 Drop 上，
/// 而不是只写在测试末尾。
struct Cleanup {
    data_dir: PathBuf,
    share_root: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // 幂等：正常跑完时末尾已经收过一遍（再收一次只是"目录不存在"）。
        mirror::shutdown();
        let _ = std::fs::remove_dir_all(&self.data_dir);
        let _ = std::fs::remove_dir_all(&self.share_root);
    }
}

/// 这个路径此刻是不是一个同步根。用 `BASIC`（8 字节）——`STANDARD` 是 1 KB 级的，
/// 缓冲区给小了会返回 `ERROR_MORE_DATA`，被读成"没注册"（实现里踩过这个坑）。
fn sync_root_registered(path: &std::path::Path) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::CloudFilters::{
        CfGetSyncRootInfoByPath, CF_SYNC_ROOT_INFO_BASIC,
    };
    if !path.exists() {
        return false;
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = [0u8; 32];
    unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(wide.as_ptr()),
            CF_SYNC_ROOT_INFO_BASIC,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            None,
        )
    }
    .is_ok()
}

async fn wait_payload(state: &Arc<AppState>, ticket: &str) -> orbdrop_lib::drag::Payload {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match state.drag.payload(ticket) {
            Ok(Some(payload)) => return payload,
            Ok(None) => {}
            Err(e) => panic!("拖拽准备失败: {e}"),
        }
        assert!(std::time::Instant::now() < deadline, "等拖拽内容超时");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}