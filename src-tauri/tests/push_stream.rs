//! 新数据面（`POST /push/stream`：块级并行 + 小文件打包 + 块位图续传）的端到端验证。
//!
//! 为什么单独一个测试二进制：`ORBDROP_DATA_DIR` 是进程级的，每个测试二进制各自把它指到
//! 临时目录最省事（见 `isolate_data_dir`），也天然与 `p1_integration.rs` 互不干扰。
//!
//! 覆盖三件事，都是"用户真会碰上"的：
//! 1. **混合目录**（上百个小文件 + 中等文件 + 一个必须分块的大文件）整份传过来、逐字节一致；
//! 2. **断了再推**：取消一次，留下块位图与 `.part`，重推之后接着传，最终文件逐字节一致；
//! 3. **源文件在同意之前被改**：那条文件必须**判失败**（不能拼出一份半新半旧的文件），
//!    而同一批里没动过的文件照常落盘（失败隔离）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orbdrop_lib::config::{Config, PeerDevice};
use orbdrop_lib::credential;
use orbdrop_lib::state::AppState;
use orbdrop_lib::transfer;
use orbdrop_lib::transfer::push_job::{TargetState, CAP_STREAM};

/// 数据目录（连带凭据库、续传文件）指到临时目录：别碰用户真实那份。
fn isolate_data_dir() {
    static INIT: std::sync::LazyLock<()> = std::sync::LazyLock::new(|| {
        let dir = std::env::temp_dir().join(format!("orbdrop_stream_data_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时数据目录");
        std::env::set_var("ORBDROP_DATA_DIR", &dir);
    });
    std::sync::LazyLock::force(&INIT);
}

fn make_state(port: u16, device_id: &str, device_name: &str, save_dir: PathBuf) -> Arc<AppState> {
    isolate_data_dir();
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    config.download_dir = save_dir;
    config.bc_exe_path = None;
    let config_path = std::env::temp_dir().join(format!(
        "orbdrop_stream_cfg_{}.toml",
        uuid::Uuid::new_v4()
    ));
    Arc::new(AppState::new(config, config_path))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_stream_{tag}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 一个"当下没人监听"的端口（内核分配后立刻放掉，与 `p1_integration` 同一套）。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// 两个互不相同的空闲端口。
fn two_free_ports() -> (u16, u16) {
    let a = free_port();
    let mut b = free_port();
    while b == a {
        b = free_port();
    }
    (a, b)
}

/// 等条件成立（100ms 一拍，超时 panic）。
async fn wait_until(what: &str, secs: u64, ok: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timeout waiting for: {what}");
}

/// 等接收方手上出现一条邀请，返回它的 id（未绑定那条路上"先挂后台推、再点同意"要用）。
async fn wait_for_invite(state: &Arc<AppState>, secs: u64) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if let Some(id) = state.push.inbound.lock().keys().next().cloned() {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timeout waiting for: 接收方拿到邀请");
}

/// 造一份"有大有小、有深有浅"的目录：返回 (根, 每个文件相对路径 → 内容)。
fn build_mixed_source(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    // 120 个小文件，摊在若干层子目录里（走"打包"那条路）。
    for i in 0..120 {
        let rel = format!("小文件/目录{}/f{i:03}.txt", i % 7);
        let body = vec![(i % 251) as u8; 1 + (i * 37) % 8192];
        let path = root.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &body).unwrap();
        files.push((rel, body));
    }
    // 三个中等文件（整块一条任务）。
    for i in 0..3 {
        let rel = format!("中等/m{i}.bin");
        let body: Vec<u8> = (0..3 * 1024 * 1024).map(|b| (b + i * 7) as u8).collect();
        let path = root.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &body).unwrap();
        files.push((rel, body));
    }
    // 一个大文件：20MB / 4MB 块 = 5 块，真的会并行分块。
    let rel = "大文件/big.bin".to_string();
    let body: Vec<u8> = (0..20 * 1024 * 1024).map(|b| (b % 253) as u8).collect();
    let path = root.join(&rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &body).unwrap();
    files.push((rel, body));

    // 空文件也要能落地（没有块可传，走"直接收尾"那条路）。
    let rel = "空/zero.txt".to_string();
    let path = root.join(&rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"").unwrap();
    files.push((rel, Vec::new()));
    files
}

/// 逐字节核对落盘结果，并断言没有残留的 `.part`。
fn assert_landed(dest_root: &std::path::Path, files: &[(String, Vec<u8>)]) {
    for (rel, expect) in files {
        let got = std::fs::read(dest_root.join(rel)).unwrap_or_else(|e| {
            panic!("{rel} 没落盘: {e}");
        });
        assert_eq!(got.len(), expect.len(), "{rel} 长度不对");
        assert!(got == *expect, "{rel} 内容不一致（长度相同也要逐个字节比）");
    }
    // `.part` 是"进行中"的痕迹：收完必须一个都不剩。
    let mut leftovers = Vec::new();
    collect_parts(dest_root, &mut leftovers);
    assert!(leftovers.is_empty(), "收完还留着临时文件: {leftovers:?}");
}

fn collect_parts(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_parts(&path, out);
        } else if path
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(".orbdrop-part"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

/// 两侧"已配对但没有长连接"：邀请走 HTTP、已绑定设备不用点同意、要带 token 认人。
/// 这是最常见的真实组合，也把新端点那条路的认人规则一起测了。
fn pair_without_ws(
    state_a: &Arc<AppState>,
    state_b: &Arc<AppState>,
    id_a: &str,
    id_b: &str,
    port_a: u16,
    port_b: u16,
) {
    pair_without_ws_with_token(state_a, state_b, id_a, id_b, port_a, port_b, &pairing_token());
}

/// 一次配对用哪枚 token。
///
/// **每次新铸一枚**（而不是全文件共用一个常量）：凭据库是**整个测试进程共用**的（数据目录按
/// pid 分，不按用例分），而"token → 设备 id"的解析在库里是**同一枚 token 对上多台设备**时
/// 谁先命中谁说话 —— 用一个常量的话，任何一条用例先失败（没走到清理那一步）就会把它的设备
/// 留在库里，后面每条用例的邀请都可能解析到**别人**身上，于是 `POST /push/invite` 一片 401。
/// 每对设备各自一枚，这条污染链就断了（产品里每枚 token 本来就是配对时随机铸的）。
fn pairing_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn pair_without_ws_with_token(
    state_a: &Arc<AppState>,
    state_b: &Arc<AppState>,
    id_a: &str,
    id_b: &str,
    port_a: u16,
    port_b: u16,
    token: &str,
) {
    credential::set_peer(id_b, token).unwrap();
    credential::set_peer(id_a, token).unwrap();
    state_a.add_peer_book(PeerDevice {
        id: id_b.into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false, relay: None,
    });
    state_b.add_peer_book(PeerDevice {
        id: id_a.into(),
        name: "PC-A".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false, relay: None,
    });
}

async fn push_one(state: &Arc<AppState>, src: &std::path::Path, peer: &str) -> String {
    transfer::push_paths(state.clone(), vec![src.to_string_lossy().to_string()], vec![peer.into()], None, None)
        .await
        .expect("邀请应该发得出去")
        .targets[0]
        .invite_id
        .clone()
}

#[tokio::test]
async fn stream_push_transfers_mixed_folder_intact() {
    let src = temp_dir("src");
    let save = temp_dir("save");
    let files = build_mixed_source(&src);
    let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-sa", "PC-SA", save.clone());
    let state_b = make_state(port_b, "device-sb", "PC-SB", save.clone());
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-sa", "device-sb", port_a, port_b);

    let invite_id = push_one(&state_a, &src, "device-sb").await;

    // 邀请必须声明能力位 —— 接收方就是靠它选新数据面的。
    wait_until("接收方拿到邀请", 10, || {
        state_b.push.inbound.lock().contains_key(&invite_id)
    })
    .await;
    let caps = state_b
        .push
        .inbound
        .lock()
        .get(&invite_id)
        .map(|inv| inv.caps.clone())
        .unwrap_or_default();
    assert!(caps.iter().any(|c| c == CAP_STREAM), "邀请应带 stream1 能力位: {caps:?}");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let dest = save.join("PC-SA").join(&name);

    // 等整批走完（发送方拿到完成回执 = 接收方那边已经收尾并报过 done）。
    // 不去轮询"传输中途的进度"：这条链路上 26MB 只要几百毫秒，轮询窗口抓不到它，
    // 断言那种东西只会变成随机失败。进度链路由 `interrupted_push_resumes...` 里
    // "传输途中就看得见已完成的块"那条断言守着（那条是确定性的）。
    wait_until("整批收完（收尾完成回执）", 120, || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    // 逐字节核对（含空文件与深目录），并确认没有 `.part` 残留。
    assert_landed(&dest, &files);
    assert!(total > 20 * 1024 * 1024);
    assert!(
        !state_b.push.inbound.lock().contains_key(&invite_id),
        "传完就该清掉邀请，不留僵尸"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-sa").ok();
    credential::remove_peer("device-sb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

#[tokio::test]
async fn interrupted_push_resumes_from_the_block_record() {
    let src = temp_dir("resume_src");
    let save = temp_dir("resume_save");
    // 一个 24MB 文件 = 6 块：取消时总能留下"传了一半"的状态。
    let big: Vec<u8> = (0..24 * 1024 * 1024).map(|b| (b % 241) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ra", "PC-RA", save.clone());
    let state_b = make_state(port_b, "device-rb", "PC-RB", save.clone());
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-ra", "device-rb", port_a, port_b);

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let rel = format!("{name}/big.bin");
    let mtime = {
        let meta = std::fs::metadata(src.join("big.bin")).unwrap();
        orbdrop_lib::transfer::stream::mtime_secs(&meta)
    };
    let dest_dir = save.join("PC-RA").join(&name);
    // 身份里含**落盘目录**（`stream::resume_key`）：这里要说的正是"这个文件收到哪儿"。
    let key = orbdrop_lib::transfer::stream::resume_key(
        "device-ra",
        &dest_dir,
        &rel,
        big.len() as u64,
        mtime,
    );
    let dest = dest_dir.join("big.bin");

    // ---- 第一趟：开传就取消 --------------------------------------------------------
    //
    // 为什么"开传就取消"而不是"等传了一半再取消"：这条链路是回环 TLS，24MB 只要几百毫秒，
    // 按时间点去掐会有随机性。这里改成抓**不变量**（下面四条），它在上面的任何时刻都成立：
    //   ① 邀请被撤掉；② 最终路径上要么没有文件、要么是**完整且逐字节一致**的文件
    //   （绝不会出现"半截文件顶着真名"）；③ 没收完时留着 `.part` 与续传记录。
    // "只补缺的块"这件事由 `stream::missing_blocks` 的单元测试守着（确定性），
    // 这里守的是端到端结果正确。
    let invite_one = push_one(&state_a, &src, "device-rb").await;
    // 记录在会话**开始**时就写好了（不依赖有没有传完块），所以这条等待是确定性的。
    wait_until("续传记录已建立", 30, || {
        state_b.push.resume.file_record(&key).is_some()
    })
    .await;
    transfer::cancel_receive(&state_b, &invite_one);
    wait_until("接收方收手", 30, || {
        !state_b.push.inbound.lock().contains_key(&invite_one)
    })
    .await;

    // ② 最终路径的两种合法状态：还没有 / 已经完整。别的都是 bug。
    if dest.exists() {
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            big,
            "出现在最终路径上的文件必须是完整的"
        );
    } else {
        // ③ 没传完：记录与 `.part` 都留着（这就是"下次接着传"的全部依据）。
        let rec = state_b.push.resume.file_record(&key).unwrap_or_else(|| {
            // 取消得足够早时可能一块都没落（位图为空但记录在 —— 记录是 prepare 阶段写的）。
            panic!("取消后续传记录不该消失");
        });
        assert_eq!(rec.size, big.len() as u64);
        assert!(rec.done_bytes() <= big.len() as u64);
        if rec.done_bytes() > 0 {
            let part = PathBuf::from(&rec.part);
            assert!(part.exists(), "有进度就必须有 .part: {part:?}");
            let part_len = std::fs::metadata(&part).unwrap().len();
            assert!(
                part_len >= rec.done_bytes(),
                "记录说的进度不能超过盘上的字节（{part_len} vs {}）",
                rec.done_bytes()
            );
        }
    }

    // ---- 第二趟：同一份源再推一次，应当接着传 ------------------------------------
    let invite_two = push_one(&state_a, &src, "device-rb").await;
    wait_until("第二趟收完（发送方拿到完成回执）", 60, || {
        state_a.push.jobs.lock().get(&invite_two).is_none()
    })
    .await;
    assert_eq!(std::fs::read(&dest).unwrap(), big, "续传之后必须逐字节一致");
    assert!(
        state_b.push.resume.file_record(&key).is_none(),
        "收完之后记录要丢掉（文件已经在最终路径上了）"
    );
    let mut leftovers = Vec::new();
    collect_parts(&save, &mut leftovers);
    assert!(leftovers.is_empty(), "收完不该留 .part: {leftovers:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ra").ok();
    credential::remove_peer("device-rb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

/// **接收端关掉应用之后自己接着收**（发送方什么都不用做，也不用重推）。
///
/// 与上一条（`interrupted_push_resumes_from_the_block_record`）的分工：
/// - 那条是"用户点了取消、又让发送方重推一次" —— 靠的是文件级块位图跨邀请复用；
/// - 这条是"这次没成、应用重启后自己接上" —— 靠的是 `resume.json` 里那份**待续传任务**
///   （邀请 id + 清单 + 落盘根 + 对端地址）。判据是"发送方一次都没重推，接收端自己把那条
///   请求重新发起并跑完"。
///
/// 第一趟怎么掐得**确定**：给接收端一个**写不进去的落盘根**（那个路径上躺着一个文件，
/// `create_dir_all` 必失败）—— 下载建目录那一步当场失败，接收端按"这次没成、推送还在"收手
/// （`fail_attempt`：摘邀请、**留任务**），正是"应用被杀/关掉"留在盘上的样子。
/// 不调 `cancel_receive`：那是"用户喊停"，会正当地把任务删掉（那样就没得续了）。
/// 也不靠"传一半再掐服务"：`stop_server` 是**优雅**停机（在飞的请求会跑完），掐不干净。
#[tokio::test]
async fn a_restarted_receiver_resumes_on_its_own() {
    let src = temp_dir("selfresume_src");
    let save = temp_dir("selfresume_save");
    // 24MB / 4MB 块 = 6 块：真有分块，重试那条路也真的走一遍。
    let big: Vec<u8> = (0..24 * 1024 * 1024).map(|b| (b % 239) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    // 那个"写不进去的落盘根"：路径上先躺一个**文件**，`create_dir_all` 必失败。
    let blocked = save.join("blocked");
    std::fs::write(&blocked, b"x").unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-za", "PC-ZA", save.clone());
    let state_b = make_state(port_b, "device-zb", "PC-ZB", blocked);
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-za", "device-zb", port_a, port_b);

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let dest = save.join("PC-ZA").join(&name).join("big.bin");

    // ---- 第一趟：接收端自动收下（已配对），建目录那一步当场失败 ----------------------------
    let invite = push_one(&state_a, &src, "device-zb").await;
    wait_until("待续传任务已落盘", 30, || {
        state_b.push.resume.pending_ids().contains(&invite)
    })
    .await;
    wait_until("这一趟收手（邀请摘掉）", 30, || {
        !state_b.push.inbound.lock().contains_key(&invite)
    })
    .await;
    assert!(
        state_b.push.resume.pending_ids().contains(&invite),
        "没传完的这条必须留在待续传名单里（重启后靠它接着收）"
    );
    assert!(!dest.exists(), "没传完就不该有最终文件");
    assert!(
        state_a.push.jobs.lock().get(&invite).is_some(),
        "发送方那条 job 还该在（它什么都没做）"
    );

    // ---- "重启"接收端：同一份数据目录 / device id，全新一份 AppState（这次落盘根是好的）----
    let state_b2 = make_state(port_b, "device-zb", "PC-ZB", save.clone());
    let pending = state_b2.push.resume.pending_entries();
    let kept = pending
        .iter()
        .find(|p| p.invite_id == invite)
        .expect("重启之后要认得这条没传完的接收（任务在盘上）");
    assert_eq!(kept.sender_id, "device-za");
    assert_eq!(kept.files.len(), 1);
    assert_eq!(kept.files[0].size, big.len() as u64);

    // 启动那一拍（`lib.rs` 里跑的就是这一句）：自己接着收 —— 发送方一次都没重推。
    transfer::resume_pending(&state_b2);

    wait_until("自己接着收完（发送方那一台收到完成回执）", 120, || {
        state_a.push.jobs.lock().get(&invite).is_none()
    })
    .await;
    assert_eq!(std::fs::read(&dest).unwrap(), big, "接着收之后必须逐字节一致");
    assert!(
        !state_b2.push.resume.pending_ids().contains(&invite),
        "传完要把待续传任务删掉（重启后不该再收一遍）"
    );
    let mut leftovers = Vec::new();
    collect_parts(&save, &mut leftovers);
    assert!(leftovers.is_empty(), "收完不该留 .part: {leftovers:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-za").ok();
    credential::remove_peer("device-zb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

/// **同一个文件收在两个目录下**：两份记录各算各的（`resume_key` 里含落盘目录）。
///
/// 这是"落盘目录并进身份"这条改动的回归闸：键里没有目录时，第二次推送会**共用**第一个目录
/// 那份记录 —— 按着它的位图去新目录找 `.part`（找不到 → 从零，这个可以接受），顺手把那份记录
/// **覆盖**成空位图、把旧目录里的 `.part` 变成没人认领的垃圾（`sweep_stale` 只清"还有记录
/// 指着"的那些）。所以这条用例守的是"**旧的那份记录与半截文件一个字都没动**"。
#[tokio::test]
async fn the_same_file_into_two_dirs_keeps_both_records() {
    let src = temp_dir("twodirs_src");
    let save = temp_dir("twodirs_save");
    let big: Vec<u8> = (0..24 * 1024 * 1024).map(|b| (b % 233) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    let dir1 = save.join("one");
    let dir2 = save.join("two");
    std::fs::create_dir_all(&dir1).unwrap();
    std::fs::create_dir_all(&dir2).unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-2a", "PC-2A", save.clone());
    let state_b = make_state(port_b, "device-2b", "PC-2B", dir1.clone());
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-2a", "device-2b", port_a, port_b);

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let rel = format!("{name}/big.bin");
    let mtime = {
        let meta = std::fs::metadata(src.join("big.bin")).unwrap();
        orbdrop_lib::transfer::stream::mtime_secs(&meta)
    };
    let land1 = dir1.join("PC-2A").join(&name);
    let key1 = orbdrop_lib::transfer::stream::resume_key("device-2a", &land1, &rel, big.len() as u64, mtime);

    // ---- 第一趟：收了一半就取消（留下记录 + 半截 `.part`）------------------------------
    let invite_one = push_one(&state_a, &src, "device-2b").await;
    wait_until("第一趟的续传记录已建立", 30, || {
        state_b.push.resume.file_record(&key1).is_some()
    })
    .await;
    transfer::cancel_receive(&state_b, &invite_one);
    wait_until("接收端收手", 30, || {
        !state_b.push.inbound.lock().contains_key(&invite_one)
    })
    .await;
    let before = state_b
        .push
        .resume
        .file_record(&key1)
        .expect("取消之后记录要留着（下次接着传的全部依据）");
    assert!(before.part.starts_with(&dir1.to_string_lossy().to_string()), "记录指着第一个目录: {}", before.part);

    // ---- 换到第二个目录再推一次：它必须**另起一份**，别碰第一份 ------------------------
    state_b.config.lock().download_dir = dir2.clone();
    let invite_two = push_one(&state_a, &src, "device-2b").await;
    wait_until("第二趟收完（发送方拿到完成回执）", 60, || {
        state_a.push.jobs.lock().get(&invite_two).is_none()
    })
    .await;

    let land2 = dir2.join("PC-2A").join(&name);
    assert_eq!(std::fs::read(land2.join("big.bin")).unwrap(), big, "新目录那份必须逐字节一致");
    assert!(
        state_a
            .push
            .resume
            .file_record(&orbdrop_lib::transfer::stream::resume_key(
                "device-2a",
                &land2,
                &rel,
                big.len() as u64,
                mtime
            ))
            .is_none(),
        "新目录那份收完就该丢掉（文件已在最终路径上）"
    );
    // **第一份一个字都没动**：记录还在、还指着第一个目录。
    let after = state_b
        .push
        .resume
        .file_record(&key1)
        .expect("第二个目录那一趟不该覆盖第一份记录");
    assert_eq!(after.part, before.part, "第一份记录的 .part 路径不该被改写");
    assert_eq!(after.done_bytes(), before.done_bytes(), "第一份记录的进度也不该被改写");
    if before.done_bytes() > 0 {
        assert!(PathBuf::from(&before.part).exists(), "第一份的 .part 还要在（它是「接着传」的凭据）");
    }
    // 收完的那个目录里不留临时文件（另一个目录里的那份是"下次接着传"的凭据，不算残留）。
    let mut leftovers = Vec::new();
    collect_parts(&dir2, &mut leftovers);
    assert!(leftovers.is_empty(), "收完的目录不该留 .part: {leftovers:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-2a").ok();
    credential::remove_peer("device-2b").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

/// **两端都重启过**：发送端那一半也要能接上（`push_out.json`）。
///
/// 与 `a_restarted_receiver_resumes_on_its_own` 的分工：那条只换接收端，发送端那条 job 一直在
/// 内存里；这条**两边都换一身 AppState** —— 发送端必须从盘上把自己那条 job 捡回来，否则对面
/// 来要块时只会拿到 410。
///
/// 第一趟照旧用"写不进去的落盘根"确定性掐断（`stop_server` 是优雅停机，掐不干净）。
#[tokio::test]
async fn both_sides_restarted_resume() {
    let src = temp_dir("both_src");
    let save = temp_dir("both_save");
    let big: Vec<u8> = (0..24 * 1024 * 1024).map(|b| (b % 227) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    let blocked = save.join("blocked");
    std::fs::write(&blocked, b"x").unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ba", "PC-BA", save.clone());
    let state_b = make_state(port_b, "device-bb", "PC-BB", blocked);
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-ba", "device-bb", port_a, port_b);

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let dest = save.join("PC-BA").join(&name).join("big.bin");

    // ---- 第一趟：B 自动收下（并把"开始拉了"回报给 A），然后因落盘根写不进去而收手 ---------
    let invite = push_one(&state_a, &src, "device-bb").await;
    wait_until("接收端接住这条（待续传任务落盘）", 30, || {
        state_b.push.resume.pending_ids().contains(&invite)
    })
    .await;
    // 发送端要知道"对面真的开始拉了" —— 这一刻它才会把这条落盘（`accepted = true`）。
    wait_until("发送端记下「对面开始拉了」", 30, || {
        state_a.push.job_status(&invite).as_deref().is_some_and(|s| s != "invited")
    })
    .await;
    wait_until("这一趟收手（邀请摘掉）", 30, || {
        !state_b.push.inbound.lock().contains_key(&invite)
    })
    .await;
    assert!(!dest.exists(), "这一趟一个字节都没落地（目录建不出来）");
    // 断言**只认自己这一条**：`push_out.json` 是**整个测试进程共用**的（数据目录按 pid 分），
    // 兄弟用例留下的记录（例如刚走完、还在 `SETTLED_GRACE` 里的批次）也在里面 —— 数总量必挂。
    let (jobs_on_disk, batches_on_disk) = state_a.push.out.snapshot();
    assert_eq!(
        jobs_on_disk.iter().filter(|j| j.id == invite).count(),
        1,
        "发送端那条 job 应该已经落盘"
    );
    assert!(jobs_on_disk.iter().any(|j| j.id == invite && j.accepted), "而且记着「对面开始拉了」");
    assert!(
        batches_on_disk
            .iter()
            .any(|b| b.targets.iter().any(|t| t.invite_id == invite)),
        "那张卡也落盘了"
    );

    // ---- 两边都"重启"：同一份数据目录 / device id / 端口，全新一身 AppState ----------------
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let state_a2 = make_state(port_a, "device-ba", "PC-BA", save.clone());
    let state_b2 = make_state(port_b, "device-bb", "PC-BB", save.clone());
    let port_a2 = orbdrop_lib::server::start_server_loopback(state_a2.clone(), port_a).await.unwrap();
    let port_b2 = orbdrop_lib::server::start_server_loopback(state_b2.clone(), port_b).await.unwrap();
    assert_eq!((port_a, port_b), (port_a2, port_b2));

    // A2 启动那一拍：把没走完的发送任务捡回来（`lib.rs` 的启动块调的就是这一句）。
    assert_eq!(orbdrop_lib::transfer::push_job::restore_outbound(&state_a2), 1);
    assert!(state_a2.push.jobs.lock().contains_key(&invite), "取文件的凭据回来了");
    let cards = state_a2.push.all_batch_views();
    assert!(
        cards
            .iter()
            .any(|c| c.targets.iter().any(|t| t.invite_id == invite && t.state == "sending")),
        "卡也回来了，而且那一行写着「传输中」: {:?}",
        cards
    );

    // B2 启动那一拍：自己接着收（发送端一次都没重推）。
    // **按运行时的节拍反复叫**（`lib.rs` 的 5s 探活循环就是这么调的）：一次没成（够不着、
    // 恰好撞上握手的边）下一拍会再试 —— 单叫一次与真实行为不符，而且会把"本会自愈的那一次"
    // 记成失败。真的落终态（例如 410）时，下一拍就找不到待续传任务了，下面的断言照样会失败。
    wait_until("接收端拿到这一趟的判决（续传收完或收手）", 120, || {
        if !state_b2.push.resume.pending_ids().contains(&invite) {
            return true;
        }
        transfer::resume_pending(&state_b2);
        false
    })
    .await;
    // ---- 本机能定下来的事实（与任何回执通道无关）------------------------------------------
    assert_eq!(std::fs::read(&dest).unwrap(), big, "接着收之后必须逐字节一致");
    assert_eq!(
        state_b2.push.answer_invite_state(&invite, "device-ba").state,
        "done",
        "接收端那边的结局必须是「完成」"
    );

    // ---- 发送端那半边的收尾：走的是**尽力而为**的回执（三次尝试，见 `report_to_sender`）----
    // 它**不是本功能承诺的东西**（承诺的是"取文件的凭据还在、对面来要块要得到" —— 上面断言过了）：
    // 没有长连接的场合回执丢了不会有人来纠正（`PushAsk` 对账要求长连接），卡片会停在 100%，直到
    // 7 天窗口按"最后报满了 → 已送达"收尾（`sweep_outbound`）。所以两种结局都接受，但"不许变成
    // 假话"要断言：它绝不能被判成失败。
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut converged = false;
    while std::time::Instant::now() < deadline {
        if state_a2.push.jobs.lock().get(&invite).is_none() {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if converged {
        let (jobs_left, batches_left) = state_a2.push.out.snapshot();
        assert!(
            !jobs_left.iter().any(|j| j.id == invite),
            "收到回执就该把发送侧那份凭据删掉: {jobs_left:?}"
        );
        // 卡片那份不要求"此刻盘上为空"：整批走完还留一段宽限期（`SETTLED_GRACE`，见 `batch_set`），
        // 到点由 `sweep_settled_batches` 收掉；即使这期间重启，"全是终态的批次"在恢复时也会被丢掉。
        // 所以这里断言的是**这一条再也恢复不出一张卡**（照样只看自己那条：文件是整个进程共用的）。
        assert!(
            !batches_left.iter().any(|b| {
                b.targets
                    .iter()
                    .any(|t| t.invite_id == invite && (t.state == "sending" || t.state == "invited"))
            }),
            "盘上不该再有这一条「还在等 / 在传」的行: {batches_left:?}"
        );
    } else {
        let status = state_a2.push.job_status(&invite).unwrap_or_default();
        assert!(
            status.starts_with("downloading"),
            "回执丢了可以（对面已经收完，本机只是还不知道），但**绝不许被判成失败**：{status}"
        );
        eprintln!(
            "[note] 发送端没在 30s 内收到完成回执（尽力而为那条路），卡片停在 100%；\
             7 天窗口到期会按「最后报满了 → 已送达」收尾（`sweep_outbound`）"
        );
    }

    orbdrop_lib::server::stop_server(&state_a2).await;
    orbdrop_lib::server::stop_server(&state_b2).await;
    credential::remove_peer("device-ba").ok();
    credential::remove_peer("device-bb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

/// 发送端**没**把那条记录捡回来（老版本、降级回来、盘上那份被删了）：对面来要块只能拿到 410。
///
/// 这条守的是"说不出来的话"那一半：收尾要发生，而且理由是 `SENDER_GONE` ——
/// **不能再写成「发送方已取消」**（`Missing` 分不出"它撤了"和"它那台没有这条 job 了"）。
#[tokio::test]
async fn without_the_sender_record_the_receiver_is_told_so() {
    let src = temp_dir("nosrc_src");
    let save = temp_dir("nosrc_save");
    std::fs::write(src.join("big.bin"), vec![3u8; 8 * 1024 * 1024]).unwrap();
    let blocked = save.join("blocked");
    std::fs::write(&blocked, b"x").unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-na", "PC-NA", save.clone());
    let state_b = make_state(port_b, "device-nb", "PC-NB", blocked);
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-na", "device-nb", port_a, port_b);

    let invite = push_one(&state_a, &src, "device-nb").await;
    wait_until("接收端接住这条", 30, || {
        state_b.push.resume.pending_ids().contains(&invite)
    })
    .await;
    wait_until("这一趟收手", 30, || {
        !state_b.push.inbound.lock().contains_key(&invite)
    })
    .await;

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    // A2：**故意不调用 `restore_outbound`** —— 就是"发送端那台没有这条 job 了"。
    let state_a2 = make_state(port_a, "device-na", "PC-NA", save.clone());
    let state_b2 = make_state(port_b, "device-nb", "PC-NB", save.clone());
    let _port_a2 = orbdrop_lib::server::start_server_loopback(state_a2.clone(), port_a).await.unwrap();
    let port_b2 = orbdrop_lib::server::start_server_loopback(state_b2.clone(), port_b).await.unwrap();
    assert_eq!(port_b, port_b2);

    transfer::resume_pending(&state_b2);
    wait_until("接收端收手（410 → 不再重试）", 60, || {
        !state_b2.push.resume.pending_ids().contains(&invite)
    })
    .await;
    let answer = state_b2.push.answer_invite_state(&invite, "device-na");
    assert_eq!(answer.state, "failed", "这条要如实落终态");
    assert_eq!(
        answer.reason.as_deref(),
        Some(orbdrop_lib::transfer::SENDER_GONE),
        "理由要说「不再提供」，而不是「发送方已取消」"
    );

    orbdrop_lib::server::stop_server(&state_a2).await;
    orbdrop_lib::server::stop_server(&state_b2).await;
    credential::remove_peer("device-na").ok();
    credential::remove_peer("device-nb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

#[tokio::test]
async fn a_changed_source_retracts_the_waiting_push() {
    let src = temp_dir("changed_src");
    let save = temp_dir("changed_save");
    std::fs::write(src.join("好的.txt"), vec![7u8; 4096]).unwrap();
    std::fs::write(src.join("改过的.bin"), vec![9u8; 2 * 1024 * 1024]).unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ca", "PC-CA", save.clone());
    let state_b = make_state(port_b, "device-cb", "PC-CB", save.clone());
    let _port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();

    // **未绑定**：这条推送要等接收方点同意 —— 正好给了我们一个确定的等待窗口
    // （已配对设备当场开收，那个窗口只有几毫秒，测不出确定性）。
    state_a.add_peer_book(PeerDevice {
        id: "device-cb".into(),
        name: "PC-CB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false,
        paused: false, relay: None,
    });

    let invite_id = push_one(&state_a, &src, "device-cb").await;
    wait_for_invite(&state_b, 10).await;

    // 清单已经生成了，这时把其中一个源文件改掉 → **发送方收回这次推送**（源变 = 我这边
    // 取消，见 `transfer::watch`）。不再有"只让那一条文件失败、其余照收"这一档：
    // 邀请还挂在对方屏幕上、谁也没点头，这就是"我改主意了"。
    std::fs::write(src.join("改过的.bin"), vec![9u8; 2 * 1024 * 1024 + 5]).unwrap();

    wait_until("发送方那一台落「已取消」", 10, || {
        state_a.push.target_of_invite(&invite_id).map(|t| t.state)
            == Some(TargetState::Cancelled)
    })
    .await;
    assert_eq!(
        state_a.push.target_of_invite(&invite_id).unwrap().error.as_deref(),
        Some("源文件已变化，本机已取消"),
        "卡片上要说清是**源变了**，不是用户点了取消"
    );
    assert!(
        state_a.push.jobs.lock().get(&invite_id).is_none(),
        "撤销之后这条 job 也要清掉（再没人会来取文件）"
    );
    // 对端那张邀请卡跟着收掉 —— 不然它点「接收」才发现早就没了。
    wait_until("对端那张邀请卡被撤掉", 10, || {
        !state_b.push.inbound.lock().contains_key(&invite_id)
    })
    .await;

    // **一个字节都不落**：整批被收回，连没动过的那个文件也不给；`.part` 这类半截产物
    // 更不该出现（下载压根没开始）。
    tokio::time::sleep(Duration::from_millis(400)).await;
    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let dest = save.join("PC-CA").join(&name);
    assert!(
        !dest.join("好的.txt").exists(),
        "整批撤回了，没动过的那个文件也不该落盘"
    );
    let mut leftovers = Vec::new();
    collect_parts(&save, &mut leftovers);
    assert!(
        leftovers.is_empty(),
        "被撤回的推送不该在盘上留下任何东西: {leftovers:?}"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ca").ok();
    credential::remove_peer("device-cb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}

/// S-12 关掉那层时：推送直接落在落盘根里（与手动下载同一层），**不建** `<发送方名>/`。
///
/// 跑的是真链路（HTTP 邀请 + 已配对免同意 + 新数据面），所以这条同时证明开关经
/// `accept_invite` → `download::run` → `resume::landing_dir` 一路传到了落盘处，
/// 而不只是那个纯函数自己算对了。开着的那一档由上面几条夹具钉着（`save.join("PC-SA")`）。
#[tokio::test]
async fn device_subdir_off_lands_directly_in_the_download_dir() {
    let src = temp_dir("src-flat");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), b"flat landing").unwrap();
    std::fs::write(src.join("sub").join("b.txt"), b"nested").unwrap();

    let save = temp_dir("save-flat");
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-fa", "PC-FA", save.clone());
    let state_b = make_state(port_b, "device-fb", "PC-FB", save.clone());
    // 接收方关掉 `<发送方名>/` 那一层（S-12，默认开）。
    state_b.config.lock().device_subdir = false;
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-fa", "device-fb", port_a, port_b);

    let invite_id = push_one(&state_a, &src, "device-fb").await;
    wait_until("整批收完（收尾完成回执）", 60, || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    // 落点 = `<下载目录>/<源目录名>/…`（相对路径照旧展开），而不是再套一层发送方名。
    let name = src.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(
        std::fs::read(save.join(&name).join("a.txt"))
            .expect("关掉那层后 a.txt 该直接落在下载目录里的源目录名下"),
        b"flat landing"
    );
    assert_eq!(
        std::fs::read(save.join(&name).join("sub").join("b.txt")).unwrap(),
        b"nested",
        "目录结构照旧完整保留"
    );
    assert!(
        !save.join("PC-FA").exists(),
        "关掉那层之后不该再多出 `<发送方名>/` 目录"
    );
    let mut leftovers = Vec::new();
    collect_parts(&save, &mut leftovers);
    assert!(leftovers.is_empty(), "收完还留着临时文件: {leftovers:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-fa").ok();
    credential::remove_peer("device-fb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}
/// 覆盖名（F-119）：**发送应用**那条路走到对端落盘的名字。
///
/// 源是"某台设备上的 `base.apk`"——测试里就是一份只读的普通文件（真实那条路的区别只在
/// 路径长什么样：`/data/app/~~…/base.apk`，本机不拷、不能改名）。发送时给一个覆盖名，
/// 对端落盘的**文件名**必须就是它、字节必须逐字节一致。
///
/// 这条守的是两件容易走散的事：① 覆盖名要一路活到落盘处（单文件推送那条路上对端只认邀请里的
/// `name` —— 少了它，对端收到的会是一条叫 `base.apk` 的文件）；② 本机那份翻译表仍按**源路径**
/// 开文件（覆盖名只改"对端看到的样子"，不该把本机读字节那一路也换掉）。
#[tokio::test]
async fn a_renamed_source_lands_under_the_override_name() {
    let src = temp_dir("rename_src");
    let save = temp_dir("rename_save");
    // 假的"安装包"：内容无所谓，要的是"路径与名字完全无关"这件事。
    let body: Vec<u8> = (0..512 * 1024).map(|b| (b % 251) as u8).collect();
    let apk = src.join("base.apk");
    std::fs::write(&apk, &body).unwrap();
    let shown = "微信_8.0.50(2600)_com.tencent.mm.apk";

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-na", "PC-NA", save.clone());
    let state_b = make_state(port_b, "device-nb", "PC-NB", save.clone());
    let port_a = orbdrop_lib::server::start_server_loopback(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server_loopback(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-na", "device-nb", port_a, port_b);

    let view = transfer::push_paths(
        state_a.clone(),
        vec![apk.to_string_lossy().to_string()],
        vec!["device-nb".into()],
        Some(vec![shown.to_string()]),
        None,
    )
    .await
    .expect("邀请应该发得出去");
    // 邀请上那条名字也要是覆盖名：单文件那条路上对端只有它，没有清单可看。
    assert_eq!(view.name, shown, "卡片/邀请上该写覆盖名");
    let invite_id = view.targets[0].invite_id.clone();

    wait_until("整批收完（收尾完成回执）", 60, || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    // 落盘名就是覆盖名（`<下载目录>/<发送方名>/<覆盖名>`），字节一个不差。
    let landed = save.join("PC-NA").join(shown);
    let got = std::fs::read(&landed).unwrap_or_else(|e| panic!("{landed:?} 没落盘: {e}"));
    assert_eq!(got.len(), body.len());
    assert!(got == body, "内容不一致");
    assert!(
        !save.join("PC-NA").join("base.apk").exists(),
        "源名不该出现在对端那份里（覆盖名取代了它）"
    );
    let mut leftovers = Vec::new();
    collect_parts(&save, &mut leftovers);
    assert!(leftovers.is_empty(), "收完还留着临时文件: {leftovers:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-na").ok();
    credential::remove_peer("device-nb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}
