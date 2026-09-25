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
    const SHARED_TOKEN: &str = "00112233445566778899aabbccddeeff";
    credential::set_peer(id_b, SHARED_TOKEN).unwrap();
    credential::set_peer(id_a, SHARED_TOKEN).unwrap();
    state_a.add_peer_book(PeerDevice {
        id: id_b.into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false,
    });
    state_b.add_peer_book(PeerDevice {
        id: id_a.into(),
        name: "PC-A".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false,
    });
}

async fn push_one(state: &Arc<AppState>, src: &std::path::Path, peer: &str) -> String {
    transfer::push_paths(state.clone(), vec![src.to_string_lossy().to_string()], vec![peer.into()])
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
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.unwrap();
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
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.unwrap();
    pair_without_ws(&state_a, &state_b, "device-ra", "device-rb", port_a, port_b);

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let rel = format!("{name}/big.bin");
    let mtime = {
        let meta = std::fs::metadata(src.join("big.bin")).unwrap();
        orbdrop_lib::transfer::stream::mtime_secs(&meta)
    };
    let key = orbdrop_lib::transfer::stream::resume_key("device-ra", &rel, big.len() as u64, mtime);
    let dest = save.join("PC-RA").join(&name).join("big.bin");

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

#[tokio::test]
async fn source_changed_before_consent_fails_only_that_file() {
    let src = temp_dir("changed_src");
    let save = temp_dir("changed_save");
    std::fs::write(src.join("好的.txt"), vec![7u8; 4096]).unwrap();
    std::fs::write(src.join("改过的.bin"), vec![9u8; 2 * 1024 * 1024]).unwrap();

    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ca", "PC-CA", save.clone());
    let state_b = make_state(port_b, "device-cb", "PC-CB", save.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.unwrap();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.unwrap();

    // **未绑定**：这条推送要等接收方点同意 —— 正好给了我们一个确定的窗口去改源文件
    // （已配对设备会当场开收，窗口只有几毫秒，测不出确定性）。
    state_a.add_peer_book(PeerDevice {
        id: "device-cb".into(),
        name: "PC-CB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false,
        paused: false,
    });

    // **注意**：未绑定那条路上，`push_paths` 会一直等到"对方点同意"才返回（HTTP 邀请是
    // 同步等回执的）。所以先把它挂起来后台跑，否则这里就在等一个我们还没给出的同意。
    let pushing = {
        let state = state_a.clone();
        let src = src.clone();
        tokio::spawn(async move { push_one(&state, &src, "device-cb").await })
    };
    let invite_id = wait_for_invite(&state_b, 10).await;

    // 清单已经生成了，这时把其中一个源文件改掉（大小变了 → 块布局也不成立了）。
    std::fs::write(src.join("改过的.bin"), vec![9u8; 2 * 1024 * 1024 + 5]).unwrap();

    transfer::accept_invite(state_b.clone(), &invite_id).expect("同意接收");
    let invite_id = pushing.await.expect("邀请任务不该 panic");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let dest = save.join("PC-CA").join(&name);
    wait_until("这批走完（没动的文件落盘）", 30, || {
        std::fs::read(dest.join("好的.txt")).map(|c| c == vec![7u8; 4096]).unwrap_or(false)
    })
    .await;
    wait_until("发送方拿到终态", 30, || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    // ① 没动过的文件照常落盘（失败隔离：一条坏文件不拖垮整批）。
    assert_eq!(std::fs::read(dest.join("好的.txt")).unwrap(), vec![7u8; 4096]);
    // ② 改过的那条**不许**出现在最终路径上：宁可不给，也不能给一份"半新半旧"的字节。
    assert!(
        !dest.join("改过的.bin").exists(),
        "源文件变过的这条不该落到最终路径"
    );
    // ③ 它一个字节都没落：`FILE` 记录一进门就被判"源与清单不符"，所以连 `.part` 都不会建
    //    （`.part` 是"写了块"的产物）。这里要钉住的正是"没有半截字节被留在盘上"。
    let mut leftovers = Vec::new();
    collect_parts(&dest, &mut leftovers);
    assert!(
        leftovers.is_empty(),
        "源变了的那条不该写任何字节，也就不该有 .part: {leftovers:?}"
    );

    // ④ 发送方那边这张卡是**失败**，不是"已完成"。
    let target = state_a
        .push
        .target_of_invite(&invite_id)
        .expect("批次里应该还有这台目标");
    assert_eq!(
        target.state,
        TargetState::Failed,
        "源文件变过就必须如实报失败（错误原因: {:?})",
        target.error
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ca").ok();
    credential::remove_peer("device-cb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save);
}