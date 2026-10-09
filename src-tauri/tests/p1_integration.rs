//! P1 集成测试：双实例 mDNS 无关（走 HTTP/WS 127.0.0.1）配对 + 推送 + 落盘。
//!
//! 覆盖校验清单：
//! - 配对：A 发起（本机生成 6 位码并显示）→ B 输入该码 → B 向 A 换取 token（F-12）
//! - 配对只到"已配对"：配对本身**不**建长连接，拨号由用户点「连接」发起（F-16）
//! - 推送：已绑定（已配对）设备**不经同意**直接收（token 就是那份信任）；未绑定的仍是
//!   "邀请 → 同意 → 流式下载" → 文件落盘下载目录（F-50~F-52；收件目录已并入下载目录）
//! - 推送进度事件（F-54 进度数据）
//! - 多设备推送（F-57）：一份清单分头发给多台、逐台失败隔离、批次快照的实时计数
//!
//! 注：凭据落在数据目录的 `credentials.dat`（P1 时曾是系统凭据库，那条路连同迁移代码
//! 已经删掉）。测试用临时 device id，结束清理，不污染真实配对；**读路径不做迁移**，
//! 所以跑测试不会把本机真实的配对搬走或删掉。需要完全隔离的用例（凭据文件本身的行为）
//! 走 `ORBDROP_DATA_DIR`，见 `tests/credentials_vault.rs`。
//!
//! 到对端那一跳全是 TLS（自签证书 + 指纹 pinning）：测试里凡是打对端地址的请求都必须用
//! `net::client()`（它带着 pinning 校验器），随手 `Client::new()` 只会在自签证书上失败。

use std::sync::Arc;
use std::time::{Duration, Instant};

use orbdrop_lib::credential;
use orbdrop_lib::state::{AppState, LinkIntent};
use orbdrop_lib::transfer;
use orbdrop_lib::ws::{LinkState, StopReason};
use orbdrop_lib::config::{Config, PeerDevice, ShareItem};

/// 给测试一个"便携版"假 BeyondCompare：预写 profile 只会写进这个临时目录，
/// 不会碰用户真实的 BC 配置（`AppState::new` 之后的配对流程会触发预写）。
fn hermetic_bc_exe() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_it_bc_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("BCompare.exe"), b"stub").unwrap();
    std::fs::write(dir.join("BCState.xml"), b"<BCState/>").unwrap();
    dir.join("BCompare.exe")
}

/// 数据目录（连带凭据库）指到临时目录：**别碰用户真实那份**。
///
/// 文件头一直写着"不污染真实配对"，但这一步以前**没实现**（那句话是靠"就算写进去也不影响
/// 读路径"兜的，而它会影响读路径）：`credential::set_peer` 写的是 `data_dir()` 下的
/// `credentials.dat` —— **真实那份**。于是同一台机器上**跑过一遍之后**，`AppState::new`
/// 会先按凭据合成一行（在 `add_peer_book` 之前），例如 C 眼里 A 变成"已配对"、不再走未绑定
/// 那条免认证的路 → 推送当场 `HTTP 401`。表现就是那条多设备用例"干净机器上第一遍绿、
/// 之后必红"（而它自己收尾的 `credential::remove_peer` 在 panic 时根本走不到）。
///
/// `LazyLock`：环境变量是进程级的，同一个测试二进制里只设一次（多线程反复 `set_var` 是
/// 未定义行为，这里让第一个到达的用例设、其余复用）。
fn isolate_data_dir() {
    static INIT: std::sync::LazyLock<()> = std::sync::LazyLock::new(|| {
        let dir = std::env::temp_dir().join(format!("orbdrop_it_data_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时数据目录");
        std::env::set_var("ORBDROP_DATA_DIR", &dir);
    });
    std::sync::LazyLock::force(&INIT);
}

fn make_state(port: u16, device_id: &str, device_name: &str, save_dir: std::path::PathBuf) -> Arc<AppState> {
    // 必须在任何凭据访问之前（每个用例都是先 `make_state` 再动 `credential`）。
    isolate_data_dir();
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    // 落盘根（手动下载 + 推送接收共用同一个目录）。
    config.download_dir = save_dir.clone();
    config.bc_exe_path = Some(hermetic_bc_exe());
    let config_path = std::env::temp_dir().join(format!("orbdrop_it_config_{}.toml", uuid::Uuid::new_v4()));
    Arc::new(AppState::new(config, config_path))
}

fn temp_share_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_it_{}_{}", name, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("hello.txt"), b"hello world from A").unwrap();
    std::fs::write(dir.join("sub").join("nested.txt"), b"nested content").unwrap();
    dir
}

/// 等着某个条件成立（轮询间隔 100ms，10s 超时）。
///
/// 只能 async：`#[tokio::test]` 跑在单线程运行时上，`std::thread::sleep` 会把
/// 唯一的 worker 线程焊死，被测的 WS/拨号任务根本没机会推进。
async fn wait_until<F: Fn() -> bool>(what: &str, ok: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timeout waiting for: {what}");
}

/// 一个"当下没人监听"的端口：让内核分配后立刻放掉。比硬编码端口安全 ——
/// 别的测试的服务器在端口被占时会顺延（+1..+10），可能正好落到硬编码的那个号上，
/// 于是"死地址"变成了别人家的服务器。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// 两个互不相同的空闲端口（每次分配都会立刻放掉）。
fn two_free_ports() -> (u16, u16) {
    let a = free_port();
    let mut b = free_port();
    while b == a {
        b = free_port();
    }
    (a, b)
}

/// 该实例的 BC profile 文件：`make_state` 用的是便携版假 BC（exe 同目录有 `BCState.xml`），
/// 所以 profile 就写在那个临时目录里，不会碰用户真实的 BC 配置。
fn bc_profile_path(state: &Arc<AppState>) -> std::path::PathBuf {
    let exe = state.config.lock().bc_exe_path.clone().expect("bc_exe_path 已配置");
    exe.parent().expect("exe 有父目录").join("BCProfiles.xml")
}

/// 从磁盘读回配置 —— "重启"的入口：重启后能恢复什么，取决于文件里到底写了什么
/// （内存里的状态一律不算）。
fn read_config(path: &std::path::Path) -> Config {
    let content = std::fs::read_to_string(path).expect("config file should exist");
    toml::from_str(&content).expect("config file should parse")
}

#[tokio::test]
async fn pairing_then_push_transfers_file_to_receiver() {
    let root = temp_share_dir("A");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_save_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    // Two instances (same process): A and B.
    let state_a = make_state(3990, "device-a", "PC-A", save_dir.clone());
    let state_b = make_state(3991, "device-b", "PC-B", save_dir.clone());

    // Give A a share.
    state_a.config.lock().shares.push(ShareItem::new(root.clone()));

    // Start both servers.
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3990).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3991).await.expect("B server");
    assert_eq!(port_a, 3990);
    assert_eq!(port_b, 3991);

    let client = orbdrop_lib::net::client();

    // ---- Pairing (F-12) ----
    // 1. A initiates: it issues the 6-digit code on itself and displays it.
    //    (It knows B's address from its device list — manual add / mDNS.)
    state_a.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-b".into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    let code = state_a.pair.create_code();
    assert_eq!(code.len(), 6, "6-digit pairing code");

    // 2. B's user reads that code and enters it in B's OrbDrop → B posts it to A.
    let resp = client
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({
            "peerId": "device-b",
            "peerName": "PC-B",
            "code": code,
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "valid code accepted");
    let conf: serde_json::Value = resp.json().await.unwrap();
    let token = conf["token"].as_str().expect("token").to_string();
    assert!(!token.is_empty(), "32-byte hex token");
    // A answers with its own identity, for B to store against.
    assert_eq!(conf["peerId"], "device-a");
    assert_eq!(conf["peerName"], "PC-A");

    // A stored the token under B's id (server side) and marked B paired,
    // keeping the address it already had for B.
    assert!(credential::has_peer("device-b"), "A stored token for B");
    let peer_b = state_a.get_peer("device-b").expect("B stays in A's list");
    assert!(peer_b.paired, "A marks B paired");
    assert_eq!((peer_b.ip.as_str(), peer_b.port), ("127.0.0.1", port_b));

    // A wrong code is refused, and a code is consumed on first success.
    let resp = client
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({"peerId": "device-b", "peerName": "PC-B", "code": "000000"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 401, "wrong code refused");
    let resp = client
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({"peerId": "device-b", "peerName": "PC-B", "code": code}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 401, "replayed code refused");

    // 3. B stores the token under A's id (client side, mirrors pair_confirm).
    credential::set_peer("device-a", &token).unwrap();
    assert!(credential::has_peer("device-a"));

    // B registers A so its WS link + download can resolve A's address.
    state_b.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-a".into(),
        name: "PC-A".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 4. Pairing stops at "已配对" — it never dials. A's user hits 「连接」 here
    // (mirrors `connect_peer`) to open the WS link the push rides on.
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-b");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if state_a.ws.has_connection("device-b") && state_b.ws.has_connection("device-a") {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("WS link did not establish in time");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ---- Push (F-50) ----
    // 一个够大的文件：既验证"传输过程中发送方就能看到进度"，也把"按块休眠把速度
    // 按死在几十 KB/s"这个坑钉住（见下面的时限断言）。
    let big = root.join("big.bin");
    std::fs::write(&big, vec![0x5Au8; 32 * 1024 * 1024]).unwrap();

    // A pushes a local directory to B (推送的是用户选的本机路径，与共享项无关）。
    // B 是**已绑定**（已配对）设备 → 这条推送不需要它点同意：整条链路里没有一处调用
    // `accept_invite`，下面"文件真的落了盘"就是"自动收下"的证明（下载循环唯一的入口
    // 就是它 —— 界面点「接收」，或已绑定设备在收到邀请时自动放行）。
    let invite_id = push_invite(state_a.clone(), vec![root.to_string_lossy().to_string()], "device-b").await
        .expect("invite sent over WS")
        .targets[0].invite_id.clone();

    // Wait for completion: file appears under the receiver's save dir, in the
    // <发送方名>/<推送名>/ 子目录.
    // The invite name is the share item name — ShareItem::new uses the dir name.
    let share_name = root.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-A").join(&share_name).join("hello.txt");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut done = false;
    // 传输过程中发送方应该**看得到进度**（不是传完才跳一下）：轮询 job 状态 ——
    // 它由收到对端进度回执的那条路径更新（与推给界面的 push://progress 同一处）。
    let mut saw_progress = false;
    let big_out = save_dir.join("PC-A").join(&share_name).join("big.bin");
    let big_len = std::fs::metadata(&big).unwrap().len();
    let nested = save_dir.join("PC-A").join(&share_name).join("sub").join("nested.txt");
    while std::time::Instant::now() < deadline {
        if let Some(job) = state_a.push.job(&invite_id) {
            if let transfer::push_job::PushStatus::Downloading { received, .. } = job.status {
                if received > 0 {
                    saw_progress = true;
                }
            }
        }
        let hello_ok = std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false);
        let nested_ok = std::fs::read(&nested).map(|c| c == b"nested content").unwrap_or(false);
        let big_ok = std::fs::metadata(&big_out).map(|m| m.len() == big_len).unwrap_or(false);
        if hello_ok && nested_ok && big_ok {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(done, "整份目录（含 32MB 大文件）应落到接收方下载目录: {:?}", expected);
    assert!(saw_progress, "发送方应在传输过程中收到进度，而不是只在结束时跳一下");

    // Nested file preserved.
    assert_eq!(std::fs::read(&nested).unwrap(), b"nested content");

    // Sender-side job should be removed after completion.
    // 用等待而不是直接断言：文件落到磁盘 ≠ 完成回执已经走完那条路 —— 后者还要几毫秒，
    // 直接断言等于赌这两件事的先后（实测约 1/5 概率翻车）。
    wait_until("发送方收到完成回执并清掉 job", || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-a").ok();
    credential::remove_peer("device-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 未绑定（没配对、两边都没有 token）的设备照样能被推送（F-50）：邀请走 HTTP 直达，
/// 接收方同意后从发送方拉取，进度/完成回执也走 HTTP —— 全程**没有** WS 长连接。
///
/// 这条链路是本轮新增的：以前推送只能走长连接，于是"未绑定"和"已配对但没点连接"
/// 这两种设备上的「推送」按钮必然报"对方离线"。
#[tokio::test]
async fn push_to_unpaired_peer_rides_http() {
    let src = temp_share_dir("U");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_usave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-UA", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UB", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    assert_eq!(state_a.server_port.load(std::sync::atomic::Ordering::SeqCst), port_a);

    // A 只是"看得见" B（mDNS/手动添加给到地址）：没配对、没有 token、没有长连接。
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    assert!(!state_a.ws.has_connection("device-ub"), "前提：没有长连接");
    assert!(!credential::has_peer("device-ub"), "前提：没有配对 token");

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("推送应该直接发出去（HTTP 邀请）")
        .targets[0].invite_id.clone();

    // B 的界面上收到了邀请卡片，请求还挂在那儿等它决定。
    wait_until("B 收到推送邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 未绑定设备的推送**必须**等人点同意：邀请到了、没人点，一个字节都不该落盘。
    // （`<下载目录>/<发送方名>` 这层目录是下载循环建的 —— 它存在 = 下载已经开始了。）
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !save_dir.join("PC-UA").exists(),
        "未绑定设备：没点同意就不该开始传"
    );

    // ---- 发送方撤销：HTTP 这条路上也要到得了对端（回执走 POST /push/report）----
    // 此时 B 还没点同意，撤销必须让那张邀请卡消失，而不是等它自己 60s 超时。
    transfer::cancel_push(&state_a, &invite_id).expect("cancel");
    wait_until("撤销后 B 的邀请被撤掉", || {
        !state_b.push.inbound.lock().contains_key(&invite_id)
    })
    .await;

    // ---- 重新推一次（这次真传）----
    let big = src.join("big.bin");
    std::fs::write(&big, vec![0x5Au8; 32 * 1024 * 1024]).unwrap();
    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("再推一次")
        .targets[0].invite_id.clone();
    wait_until("B 收到第二次邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    transfer::accept_invite(state_b.clone(), &invite_id, None).expect("B 同意");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-UA").join(&name).join("hello.txt");
    let big_out = save_dir.join("PC-UA").join(&name).join("big.bin");
    let big_len = std::fs::metadata(&big).unwrap().len();
    let nested = save_dir.join("PC-UA").join(&name).join("sub").join("nested.txt");

    // 时限 = 回归闸：以前接收端每块睡 50ms（块只有几 KB），32MB 要几分钟 ——
    // 发送方的进度条就"一直为 0"，接收方那张卡也一直停在 0%。
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_progress = false;
    let mut done = false;
    while std::time::Instant::now() < deadline {
        if let Some(job) = state_a.push.job(&invite_id) {
            if let transfer::push_job::PushStatus::Downloading { received, .. } = job.status {
                if received > 0 {
                    saw_progress = true;
                }
            }
        }
        let hello_ok = std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false);
        let nested_ok = std::fs::read(&nested).map(|c| c == b"nested content").unwrap_or(false);
        let big_ok = std::fs::metadata(&big_out).map(|m| m.len() == big_len).unwrap_or(false);
        if hello_ok && nested_ok && big_ok {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(done, "HTTP 通道也该把整份目录传完（含 32MB 大文件）");
    assert_eq!(std::fs::read(&expected).unwrap(), b"hello world from A");
    assert!(
        std::fs::read(&nested).unwrap() == b"nested content",
        "目录结构完整保留"
    );
    assert!(saw_progress, "HTTP 通道下发送方也应在传输过程中收到进度");

    // 完成回执也是 HTTP：A 收到后把 job 清掉。
    wait_until("发送方收到完成回执", || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 已绑定（已配对）但**没建长连接**的设备：推送走 HTTP 直达，同样不需要接收方同意 ——
/// token 就是那份信任。"自动收下"在长连接那条路和 HTTP 这条路上是两段代码
/// （`ws::session` 的 `push_invite` 分支 / `routes_push::push_invite_handler`），两条都得测。
#[tokio::test]
async fn paired_push_without_a_long_connection_skips_consent_http() {
    let src = temp_share_dir("PH");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_phsave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-pha", "PC-PHA", save_dir.clone());
    let state_b = make_state(port_b, "device-phb", "PC-PHB", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");

    // 两侧互相已配对：地址簿标 paired，凭据里存着**同一枚** token（真实配对换来的就是
    // 这一枚 —— 谁给对方发邀请就用它做 Bearer）。凭据文件是进程内共用的一份，所以这里
    // 用一对独一无二的 device id + token，跑完清掉，别踩到别的用例。
    const SHARED_TOKEN: &str = "0123456789abcdef0123456789abcdef";
    credential::set_peer("device-phb", SHARED_TOKEN).unwrap();
    credential::set_peer("device-pha", SHARED_TOKEN).unwrap();
    state_a.add_peer_book(PeerDevice {
        id: "device-phb".into(),
        name: "PC-PHB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });
    state_b.add_peer_book(PeerDevice {
        id: "device-pha".into(),
        name: "PC-PHA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });
    assert!(!state_a.ws.has_connection("device-phb"), "前提：没有长连接（走 HTTP 那条路）");

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-phb")
        .await
        .expect("已绑定设备的邀请应该直接发出去")
        .targets[0].invite_id.clone();
    // 同前：没有任何一处调用 `accept_invite`，落盘即"自动收下"。
    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-PHA").join(&name).join("hello.txt");
    wait_until("已绑定但未长连接的设备：没点同意也把文件收下", || {
        std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false)
    })
    .await;

    // 这条路没有 WS：完成回执走 POST /push/report，发送方的 job 由它清掉。
    wait_until("发送方收到完成回执（HTTP）", || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;
    assert!(
        !state_b.push.inbound.lock().contains_key(&invite_id),
        "传完就该清掉，不留一条僵尸邀请"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-pha").ok();
    credential::remove_peer("device-phb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 「每次询问」（S-02）选定的落盘根：未绑定设备那条同意路上，用户在目录选择框里挑的目录就是
/// **这一次**接收的根 —— `<发送方名>/` 那层照旧拼在它下面，设置里的下载目录这次一个字都不写。
///
/// 顺带钉住校验那一下：不合格的根（相对路径）当场报错、**这条邀请还留着**（用户能换个目录
/// 再点一次「接收」），而不是把卡片吃掉、让发送方在那儿干等。
#[tokio::test]
async fn consent_push_lands_in_the_chosen_root() {
    let src = temp_share_dir("R");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_rsave_{}", uuid::Uuid::new_v4()));
    let chosen = std::env::temp_dir().join(format!("orbdrop_it_rpick_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ra", "PC-RA", save_dir.clone());
    let state_b = make_state(port_b, "device-rb", "PC-RB", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");

    // A 只是"看得见" B：没配对、没有 token、没有长连接（与未绑定那条用例同一档）。
    state_a.add_peer_book(PeerDevice {
        id: "device-rb".into(),
        name: "PC-RB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-rb")
        .await
        .expect("推送应该直接发出去（HTTP 邀请）")
        .targets[0].invite_id.clone();
    wait_until("B 收到推送邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 不合格的根：当场拒绝，且**邀请原样留着**。
    assert!(
        transfer::accept_invite(state_b.clone(), &invite_id, Some("relative/dir".into())).is_err(),
        "相对路径不该被当成落盘根"
    );
    assert!(
        state_b.push.inbound.lock().contains_key(&invite_id),
        "校验不过不该把邀请吃掉（用户还能换个目录重来）"
    );

    transfer::accept_invite(state_b.clone(), &invite_id, Some(chosen.clone())).expect("B 同意");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = chosen.join("PC-RA").join(&name).join("hello.txt");
    wait_until("收进用户挑的那个根（<根>/<发送方名>/…）", || {
        std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false)
    })
    .await;
    assert!(
        !save_dir.join("PC-RA").exists(),
        "挑了别的根，设置里的下载目录这次一个字都不该写"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_dir_all(&chosen);
}

/// n 个互不相同的空闲端口（每次分配都会立刻放掉）。
fn free_ports(n: usize) -> Vec<u16> {
    let mut out = Vec::new();
    while out.len() < n {
        let p = free_port();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// 多设备推送（F-50）：一次选择、**一份清单**、每台各有一条邀请。
///
/// 与单台推送的区别只有三件事，这里盯的正是它们：
/// - **失败隔离**：一台离线就地失败（F-53，不排队），另外两台照走；
/// - **一份清单**：两台收到的邀请内容一字不差（不是各建一份，落盘路径也就一致）；
/// - **整批的实时状态**：每台的成败/在途都落在同一批快照里，批次活到最后一台结束。
///
/// "还差几台"这件事靠**按住 C 的同意**测：C 未绑定，不点同意就不开始 —— 于是
/// "B 成了、C 还在等"这一刻是确定性的窗口，快照里的 成功/等待/失败 三个数可以逐一对上。
#[tokio::test]
async fn multi_device_push_isolates_targets_and_shares_one_manifest() {
    let src = temp_share_dir("MD");
    let big = src.join("big.bin");
    let big_len: u64 = 32 * 1024 * 1024;
    std::fs::write(&big, vec![0x5Au8; big_len as usize]).unwrap();
    let save_b = std::env::temp_dir().join(format!("orbdrop_it_mdb_{}", uuid::Uuid::new_v4()));
    let save_c = std::env::temp_dir().join(format!("orbdrop_it_mdc_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_b).unwrap();
    std::fs::create_dir_all(&save_c).unwrap();

    let ports = free_ports(3);
    let (port_a, port_b, port_c) = (ports[0], ports[1], ports[2]);
    let state_a = make_state(port_a, "device-mda", "PC-MDA", save_b.clone());
    let state_b = make_state(port_b, "device-mdb", "PC-MDB", save_b.clone());
    let state_c = make_state(port_c, "device-mdc", "PC-MDC", save_c.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    let port_c = orbdrop_lib::server::start_server(state_c.clone(), port_c).await.expect("C server");

    // B 已配对（收到就收，不问人）、C 未绑定（要它自己点头）、D 已配对但离线。
    const SHARED_TOKEN: &str = "fedcba9876543210fedcba9876543210";
    credential::set_peer("device-mdb", SHARED_TOKEN).unwrap();
    credential::set_peer("device-mda", SHARED_TOKEN).unwrap();
    let peer = |id: &str, name: &str, port: u16, online: bool, paired: bool| PeerDevice {
        id: id.into(),
        name: name.into(),
        ip: if online { "127.0.0.1".into() } else { String::new() },
        port: if online { port } else { 0 },
        online,
        paired,
        auto_connect: false, paused: false, relay: None,
    };
    state_a.add_peer_book(peer("device-mdb", "PC-MDB", port_b, true, true));
    state_a.add_peer_book(peer("device-mdc", "PC-MDC", port_c, true, false));
    state_a.add_peer_book(peer("device-mdd", "PC-MDD", 0, false, true));
    state_b.add_peer_book(peer("device-mda", "PC-MDA", port_a, true, true));
    state_c.add_peer_book(peer("device-mda", "PC-MDA", port_a, true, false));
    assert!(!state_a.ws.has_connection("device-mdb"), "前提：走 HTTP 那条路（这条用例不测 WS 通道）");

    // 同一台勾两次只发一条：4 个 ptr 里有一个重复项，落成 3 台。
    let paths = vec![src.to_string_lossy().to_string()];
    let view = transfer::push_paths(
        state_a.clone(),
        paths,
        vec!["device-mdb".into(), "device-mdb".into(), "device-mdc".into(), "device-mdd".into()],
    )
    .await
    .expect("两台在线：整批该发出去");
    let batch_id = view.batch_id.clone();

    // ── 每台各有独立状态：离线那台当场失败（F-53），其余两台在等 ──
    assert_eq!((view.targets.len(), view.waiting, view.failed), (3, 2, 1), "3 台目标：2 台在等、1 台离线失败");
    let offline = view.targets.iter().find(|t| t.peer_id == "device-mdd").expect("离线那一行还在");
    assert_eq!(offline.state, "failed");
    assert!(
        offline.error.as_deref().unwrap_or("").contains("不在网上"),
        "失败原因要说清是离线：{:?}",
        offline.error
    );
    let invite_b = view.targets.iter().find(|t| t.peer_id == "device-mdb").unwrap().invite_id.clone();
    let invite_c = view.targets.iter().find(|t| t.peer_id == "device-mdc").unwrap().invite_id.clone();
    assert!(!invite_b.is_empty() && !invite_c.is_empty() && invite_b != invite_c, "每台各有一条邀请");
    assert_eq!(state_a.push.batch_id_of(&invite_b).as_deref(), Some(batch_id.as_str()), "邀请反查得到自己那一批");
    assert_eq!(state_a.push.batch_id_of(&invite_c).as_deref(), Some(batch_id.as_str()), "两台归在同一批");

    // ── 一份清单：两台收到的邀请内容一字不差（此刻 B 还在传、C 还没点头，两条邀请都在） ──
    wait_until("C 收到邀请", || state_c.push.inbound.lock().contains_key(&invite_c)).await;
    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let manifest = |state: &Arc<AppState>, invite: &str| {
        state
            .push
            .inbound
            .lock()
            .get(invite)
            .map(|inv| (inv.name.clone(), inv.files.len(), inv.total_bytes()))
    };
    let mb = manifest(&state_b, &invite_b);
    let mc = manifest(&state_c, &invite_c);
    assert!(mb.is_some(), "B 那条邀请还在（下载循环拿它当'没被取消'的标志）");
    assert_eq!(mb, mc, "两台收到的清单必须完全一致（清单只建一次）");

    // ── 传输中：整批快照里出现"传输中"那一档（界面那张卡的实时数字就是它） ──
    let mut saw_sending = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Some(shot) = state_a.push.batch_snapshot(&batch_id) {
            if shot.targets.iter().any(|t| t.state == "sending") {
                assert_eq!((shot.done, shot.waiting, shot.failed), (0, 2, 1), "传的过程中：成功 0 · 等待 2 · 失败 1");
                assert!(shot.received > 0 && shot.total > 0, "聚合进度该往前走");
                saw_sending = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_sending, "整批快照里应出现'传输中'（不是传完才跳一下）");

    // ── B 传完、C 仍在等：计数行就是"成功 1 · 等待 1 · 失败 1"（C 不点头它就一直等着） ──
    let expected_b = save_b.join("PC-MDA").join(&name).join("big.bin");
    wait_until("B 自动收下整份目录", || {
        std::fs::metadata(&expected_b).map(|m| m.len() == big_len).unwrap_or(false)
    })
    .await;
    wait_until("快照里 B 已成功、C 还在等", || {
        state_a
            .push
            .batch_snapshot(&batch_id)
            .map(|shot| (shot.done, shot.waiting, shot.failed) == (1, 1, 1))
            .unwrap_or(false)
    })
    .await;

    // ── C 点头（未绑定设备的同意制在批次里照旧）→ 也传完 ──
    transfer::accept_invite(state_c.clone(), &invite_c, None).expect("C 同意");
    let expected_c = save_c.join("PC-MDA").join(&name).join("hello.txt");
    // 等**整份目录**都落齐再断言：下载是逐文件的顺序循环，`hello.txt` 写完不代表
    // `sub/nested.txt` 也写完了 —— 只等其中一个、再断言另一个，是一条会偶发的竞态
    //（串行也照样会撞上，与谁先谁后无关，只看两台的调度）。
    let expected_nested = save_c.join("PC-MDA").join(&name).join("sub").join("nested.txt");
    wait_until("C 也收下整份目录", || {
        std::fs::read(&expected_c).map(|c| c == b"hello world from A").unwrap_or(false)
            && std::fs::read(&expected_nested)
                .map(|c| c == b"nested content")
                .unwrap_or(false)
    })
    .await;
    assert!(
        std::fs::metadata(&expected_nested).is_ok(),
        "目录结构照旧完整保留"
    );

    // ── 整批的最后一步：没有一台还在路上。**批次不再立刻丢** —— 留 `SETTLED_GRACE`
    //    宽限期（对端的回执可能还在路上，链路回来之后的对账也要先找得到这批），
    //    这样"对面其实收完了、本机写着失败"才纠正得回来（`transfer::apply_push_states`）。
    //    可观察的契约于是是"全部落终态 + 批次到点才被清扫掉"。 ──
    wait_until("整批都落终态", || {
        state_a
            .push
            .batch_snapshot(&batch_id)
            .map(|shot| shot.waiting == 0 && shot.done == 2)
            .unwrap_or(false)
    })
    .await;
    state_a.push.sweep_settled_batches();
    assert!(
        state_a.push.batch_snapshot(&batch_id).is_some(),
        "宽限期内不清：纠正（对账）还得找得到这批"
    );
    assert!(
        state_a.push.target_of_invite(&invite_b).is_some(),
        "按邀请号也找得到 —— 对账答案要落到这一行上"
    );

    // ── 一台都派不出去 = 单台那条"就地失败"（F-53）：整体报错，界面弹提示、不建卡片 ──
    let err = transfer::push_paths(
        state_a.clone(),
        vec![src.to_string_lossy().to_string()],
        vec!["device-mdd".into(), "device-missing".into()],
    )
    .await
    .expect_err("都收不了就不该建卡片");
    assert!(err.contains("都收不了"), "错误要说清是「没一台能收」：{err}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    orbdrop_lib::server::stop_server(&state_c).await;
    credential::remove_peer("device-mda").ok();
    credential::remove_peer("device-mdb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_b);
    let _ = std::fs::remove_dir_all(&save_c);
}

/// 多选推送（F-50）：选中的几项合并成**一条**推送 —— 对端只收到一张邀请卡、只同意一次，
/// 落盘仍是一棵完整的树（每个源各占一个顶层前缀），同名源自动去重。
#[tokio::test]
async fn multi_select_push_is_one_job_with_one_consent() {
    let base = std::env::temp_dir().join(format!("orbdrop_it_multi_{}", uuid::Uuid::new_v4()));
    let (dir_a, dir_x, dir_y) = (base.join("甲目录"), base.join("x"), base.join("y"));
    for d in [&dir_a, &dir_x, &dir_y] {
        std::fs::create_dir_all(d.join("sub")).unwrap();
    }
    std::fs::write(dir_a.join("报告.txt"), b"report A").unwrap();
    std::fs::write(dir_a.join("sub").join("明细.txt"), b"detail A").unwrap();
    let loose = base.join("散文件.txt");
    std::fs::write(&loose, b"loose file").unwrap();
    let same_x = dir_x.join("同名.txt");
    let same_y = dir_y.join("同名.txt");
    std::fs::write(&same_x, b"same name from x").unwrap();
    std::fs::write(&same_y, b"same name from y").unwrap();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_msave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-UM", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UN", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UN".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    // ---- 第一批：一个目录 + 一个散文件（两个源）—— 各以自己的名字平铺在 <发送方>/ 下 ----
    let picks: Vec<String> = [&dir_a, &loose]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let invite_id = push_invite(state_a.clone(), picks, "device-ub")
        .await
        .expect("多选也该直接发出去")
        .targets[0].invite_id.clone();
    wait_until("B 收到邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 多选**不套**"推送名"那一层：直接传多文件就是多文件，不是在目录里。
    let root = save_dir.join("PC-UM");
    let first_batch = [
        (root.join("甲目录").join("报告.txt"), &b"report A"[..]),
        (root.join("甲目录").join("sub").join("明细.txt"), &b"detail A"[..]),
        (root.join("散文件.txt"), &b"loose file"[..]),
    ];
    run_one_push(&state_a, &state_b, &invite_id, &first_batch).await;

    // 一条邀请 = 一次同意：多选不是 N 次推送。
    assert_eq!(
        state_b.push.inbound.lock().len(),
        0,
        "传完就该清掉（不是留了 N 条）"
    );

    // ---- 第二批：两个同名文件、父目录不同 → 平铺 + 同名自动去重 ----
    let picks: Vec<String> = [&same_x, &same_y]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let invite_id = push_invite(state_a.clone(), picks, "device-ub")
        .await
        .expect("第二批")
        .targets[0].invite_id.clone();
    wait_until("B 收到第二批邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;
    let manifest_len = state_b
        .push
        .inbound
        .lock()
        .get(&invite_id)
        .map(|inv| inv.files.len())
        .unwrap_or(0);
    assert_eq!(manifest_len, 2, "两个源两条清单项");

    let second_batch = [
        (root.join("同名.txt"), &b"same name from x"[..]),
        (root.join("同名 (2).txt"), &b"same name from y"[..]),
    ];
    run_one_push(&state_a, &state_b, &invite_id, &second_batch).await;

    // 整棵落地形状：<下载目录>/<发送方名>/ 下面就是那几项，没有多余的一层。
    assert!(
        !root.join("2 项").exists() && !root.join("甲目录").join("甲目录").exists(),
        "不该有「推送名」那一层目录"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 单台推送 = 一批只有一台的推送：测试要的是那条邀请 id（`BatchView.targets[0]`）。
///
/// 多设备推送（一批 N 台）的行为在 `push_paths` 自己那儿由 `BatchView` 全量给出，
/// 这里只是把"一台"这件常见事收敛成一句。
async fn push_invite(
    state: Arc<AppState>,
    paths: Vec<String>,
    peer_id: &str,
) -> Result<transfer::push_job::BatchView, String> {
    transfer::push_paths(state, paths, vec![peer_id.to_string()]).await
}

/// 「同意 → 等这批文件全部落地 → 等发送方收到完成回执」——多选用例的两批都走这一套。
async fn run_one_push(
    state_a: &Arc<AppState>,
    state_b: &Arc<AppState>,
    invite_id: &str,
    expect: &[(std::path::PathBuf, &[u8])],
) {
    transfer::accept_invite(state_b.clone(), invite_id, None).expect("同意");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if expect
            .iter()
            .all(|(p, want)| std::fs::read(p).map(|got| got == *want).unwrap_or(false))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "每个源都该按前缀落到对端：{:?}",
            expect.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_until("发送方收到完成回执", || {
        state_a.push.jobs.lock().get(invite_id).is_none()
    })
    .await;
}

/// **对端自报的名字是恶意字符串时，落盘根仍然钉在下载目录里面**（P0-1 回归）。
///
/// `sender.name` 是邀请报文里的自由字段（HTTP 与 WS 两条路都是），包一层 `..` 或写成绝对
/// 路径原本会让 `download_dir.join(name)` 直接跑到下载目录之外 —— `PathBuf::push` 对带前缀
/// /绝对路径的入参会**清空**已累积的基址。两条形态都测：
///   ① 相对逃逸：`..\escaped`（原本会落到 `<下载目录>/../escaped`）；
///   ② 绝对路径：把 `<base>/absolute-attack` 整个当名字报过来（原本会落到那儿）。
/// 断言不仅"文件落对了位置"，还包括"逃逸目标一个都没被创建"。
#[tokio::test]
async fn a_hostile_sender_name_still_lands_inside_the_download_dir() {
    let base = std::env::temp_dir().join(format!("orbdrop_it_evil_{}", uuid::Uuid::new_v4()));
    let save_dir = base.join("dl");
    std::fs::create_dir_all(&save_dir).unwrap();
    // 源文件放在 base **之外**：base 里只应有下载目录，这样"下载目录之外一个 A.txt 都没有"
    // 才是一条能直接断言的判据。
    let src_dir = std::env::temp_dir().join(format!("orbdrop_it_evil_src_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&src_dir).unwrap();
    let file = src_dir.join("A.txt");
    std::fs::write(&file, b"hostile name body").unwrap();

    // ② 用的绝对路径：**故意**只在 base 里面，这样"没被逃出去"这件事可以被断言。
    let absolute_attack = base.join("absolute-attack");
    let mut ports: Vec<u16> = Vec::new();
    while ports.len() < 3 {
        let p = two_free_ports().0;
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    let (port_a1, port_a2, port_b) = (ports[0], ports[1], ports[2]);
    let state_a1 = make_state(port_a1, "device-evil-1", "..\\escaped", save_dir.clone());
    let state_a2 = make_state(
        port_a2,
        "device-evil-2",
        absolute_attack.to_string_lossy().as_ref(),
        save_dir.clone(),
    );
    let state_b = make_state(port_b, "device-evil-b", "PC-EVB", save_dir.clone());
    let _a1 = orbdrop_lib::server::start_server(state_a1.clone(), port_a1)
        .await
        .expect("A1 server");
    let _a2 = orbdrop_lib::server::start_server(state_a2.clone(), port_a2)
        .await
        .expect("A2 server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b)
        .await
        .expect("B server");
    for (id, port) in [("device-evil-1", port_a1), ("device-evil-2", port_a2)] {
        state_b.add_peer_book(PeerDevice {
            id: id.into(),
            name: "attacker".into(),
            ip: "127.0.0.1".into(),
            port,
            online: true,
            paired: false,
            auto_connect: false,
            paused: false, relay: None,
        });
    }

    for state_a in [state_a1.clone(), state_a2.clone()] {
        state_a.add_peer_book(PeerDevice {
            id: "device-evil-b".into(),
            name: "PC-EVB".into(),
            ip: "127.0.0.1".into(),
            port: port_b,
            online: true,
            paired: false,
            auto_connect: false,
            paused: false, relay: None,
        });
        let invite_id = push_invite(
            state_a.clone(),
            vec![file.to_string_lossy().to_string()],
            "device-evil-b",
        )
        .await
        .expect("邀请发出去")
        .targets[0]
        .invite_id
        .clone();
        wait_until("B 收到邀请", || {
            state_b.push.inbound.lock().contains_key(&invite_id)
        })
        .await;
        transfer::accept_invite(state_b.clone(), &invite_id, None).expect("B 同意");
        wait_until("文件落到下载目录里", || {
            find_file_under(&save_dir, "A.txt")
                .map(|p| std::fs::read(&p).map(|c| c == b"hostile name body").unwrap_or(false))
                .unwrap_or(false)
        })
        .await;
    }

    // 逃逸目标必须**一个都不存在**（存在 = 名字没被净化，基址被顶掉了）。
    assert!(
        !base.join("escaped").exists(),
        "`..\\escaped` 逃出了下载目录：{:?}",
        base.join("escaped")
    );
    assert!(
        !absolute_attack.join("A.txt").exists(),
        "绝对路径名字把落盘根顶到了 {:?}",
        absolute_attack
    );
    // 而且下载目录之外（base 里面）不该有任何 A.txt。
    assert!(
        find_file_under(&base, "A.txt")
            .map(|p| p.starts_with(&save_dir))
            .unwrap_or(true),
        "有文件落在下载目录之外"
    );
    assert!(find_file_under(&save_dir, "A.txt").is_some(), "文件应当落在下载目录里");

    orbdrop_lib::server::stop_server(&state_a1).await;
    orbdrop_lib::server::stop_server(&state_a2).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&src_dir);
}

/// 在 `root` 下（含子目录）找第一个叫 `name` 的文件。
fn find_file_under(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().map(|n| n == name).unwrap_or(false) {
                return Some(path);
            }
        }
    }
    None
}

/// 推**单个文件**时对面不套同名目录：`A.txt` 直接落在 `<下载目录>/<发送方名>/A.txt`，
/// 不是 `<下载目录>/<发送方名>/A.txt/A.txt`（那一层子目录只对"目录 / 多项"推送有分组意义）。
#[tokio::test]
async fn a_single_pushed_file_lands_as_a_file() {
    let src_dir = std::env::temp_dir().join(format!("orbdrop_it_one_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&src_dir).unwrap();
    let file = src_dir.join("A.txt");
    std::fs::write(&file, b"single file body").unwrap();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_osave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-UO", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UP", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UP".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    let invite_id = push_invite(state_a.clone(), vec![file.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("邀请发出去")
        .targets[0].invite_id.clone();
    wait_until("B 收到邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;
    transfer::accept_invite(state_b.clone(), &invite_id, None).expect("B 同意");

    let landed = save_dir.join("PC-UO").join("A.txt");
    wait_until("单个文件直接落盘（不套同名目录）", || {
        std::fs::read(&landed).map(|c| c == b"single file body").unwrap_or(false)
    })
    .await;
    assert!(landed.is_file(), "落下来的是一个文件: {landed:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// **同步上下文**里点「同意接收」也必须真的开始下载。
///
/// Tauri 的同步命令跑在主线程上：那里没有 Tokio reactor。`tokio::spawn` 会 panic（点一下
/// App 就闪退），而"没有运行时就跳过"会让点击**静默失效** —— 用户侧看到的是两边都停在 0%，
/// 日志里只有一行 `no tokio runtime here; task skipped task="push download"`。
#[tokio::test]
async fn accepting_a_push_from_a_sync_context_still_downloads() {
    let src = temp_share_dir("S");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_ssave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-US", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UT", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UT".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("邀请发出去")
        .targets[0].invite_id.clone();
    wait_until("B 收到邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 「同意接收」：在一个**没有 reactor** 的普通线程上调用 —— 同步 Tauri 命令就是这个上下文。
    let accept_state = state_b.clone();
    let accept_id = invite_id.clone();
    std::thread::spawn(move || {
        transfer::accept_invite(accept_state, &accept_id, None).expect("accept");
    })
    .join()
    .expect("accept 线程");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-US").join(&name).join("hello.txt");
    wait_until("同步上下文点同意后，文件也要真的落盘", || {
        std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false)
    })
    .await;

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 端口被占时服务会顺延，`/pair/info` 必须报**实际**监听端口 —— 对端会把这里
/// 报的端口存下来当拨号地址，报错就等于让它去连别的进程（或连不上）。
#[tokio::test]
async fn pair_info_reports_the_port_actually_serving() {
    let squatter = std::net::TcpListener::bind(("0.0.0.0", 3995)).unwrap();
    let state = make_state(3995, "device-pa", "PC-PA", std::env::temp_dir());
    let port = orbdrop_lib::server::start_server(state.clone(), 3995).await.expect("server");
    assert_eq!(port, 3996, "3995 被占 → 顺延");

    let info: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/pair/info"))
        .await.unwrap().json().await.unwrap();
    assert_eq!(info["port"].as_u64(), Some(u64::from(port)), "pair/info 必须报实际端口");
    assert_eq!(info["id"], "device-pa");

    drop(squatter);
    orbdrop_lib::server::stop_server(&state).await;
}

/// 启动/任意时刻的 panic 必须留痕：`record_crash` 同步追加，不经 tracing 的异步刷盘。
#[test]
fn crash_recorder_appends_synchronously() {
    let dir = std::env::temp_dir().join(format!("orbdrop_crash_{}", uuid::Uuid::new_v4()));
    let path = dir.join("nested").join("crash.log");
    orbdrop_lib::log::record_crash(&path, "boom-1");
    orbdrop_lib::log::record_crash(&path, "boom-2");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2, "每次崩溃一行、追加而不是覆盖");
    assert!(text.contains("boom-1") && text.contains("boom-2"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 解除配对（F-14）必须是**双向**的，而且不能把设备行抹掉：对方还在网络上看得见，
/// 就该继续以"未绑定"显示（离线才消失）。
///
/// 特意先把长连接断掉再解除：旧的实现靠 WS 事件通知对端，事件是排队发送的，紧跟着
/// 的 drop_peer 会把它一起丢掉，链路不通时更是必然丢 —— 对端就一直停在"已配对"。
#[tokio::test]
async fn unpair_clears_both_sides_and_keeps_the_device_listed() {
    credential::remove_peer("device-ua").ok();
    credential::remove_peer("device-ub").ok();

    let dir = std::env::temp_dir().join(format!("orbdrop_unpair_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_a = make_state(3998, "device-ua", "PC-UA", dir.clone());
    let state_b = make_state(3999, "device-ub", "PC-UB", dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3998).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3999).await.expect("B server");

    // 配对：B 用 A 显示的码向 A 换取 token（A 从请求里拿到 B 的地址）。
    let code = state_a.pair.create_code();
    let resp = orbdrop_lib::net::client()
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({
            "peerId": "device-ub",
            "peerName": "PC-UB",
            "peerPort": port_b,
            "code": code,
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let token = resp.json::<serde_json::Value>().await.unwrap()["token"].as_str().unwrap().to_string();
    credential::set_peer("device-ua", &token).unwrap();
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-ua".into(),
        name: "PC-UA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 配对不再自己建链：A 点「连接」（镜像 connect_peer）才有链路。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ub");
    wait_until("点连接后建链", || {
        state_a.ws.has_connection("device-ub") && state_b.ws.has_connection("device-ua")
    }).await;

    // 故意先把长连接断掉：解除配对不允许依赖 WS 通道。
    state_a.ws.pause("device-ub");
    wait_until("断开生效", || !state_b.ws.has_connection("device-ua")).await;

    // 配对时 A 就把 BC profile 预写好了（BC 只在启动那一刻登记这个文件，必须提前落盘），
    // 账号用的是应用名 —— 记录里认得出这条连接是本应用发起的。
    let profile_file = bc_profile_path(&state_a);
    let written = std::fs::read_to_string(&profile_file).expect("配对后应预写好 profile");
    assert!(
        written.contains(&format!("TWebDAVProfile Value=\"127.0.0.1:{port_b}\"")),
        "配对后该有指向 B 的 profile:\n{written}"
    );
    assert!(
        written.contains("Username Value=\"OrbDrop\""),
        "profile 账号是应用名:\n{written}"
    );
    // 用户自己的 profile（用户名不是我们的，名字也不像 `IP:端口`）必须一个字都不动。
    let foreign = "\t<TFtpProfile Value=\"MyFtp\">\n\t\t<Host Value=\"example.com\"/>\n\t</TFtpProfile>";
    let with_foreign = written.replace("</BCProfiles>", &format!("{foreign}\n</BCProfiles>"));
    std::fs::write(&profile_file, &with_foreign).unwrap();

    orbdrop_lib::unpair_peer(&state_a, "device-ub").await.expect("unpair");

    // 关系断了 → 指向它的 profile 一起清掉：token 已作废，留着只会让 BC 拿死凭据去撞；
    // 而用户自己那条必须还在（清理只认"我们写下的"条目）。
    let after = std::fs::read_to_string(&profile_file).unwrap();
    assert!(
        !after.contains(&format!("TWebDAVProfile Value=\"127.0.0.1:{port_b}\"")),
        "解除配对后该清掉那条 profile:\n{after}"
    );
    assert!(after.contains(foreign.trim()), "用户手建的 profile 必须原样保留:\n{after}");

    // A 侧：token 清掉、链路没了，但设备行还在、只是回到"未绑定"。
    assert!(!credential::has_peer("device-ub"), "A 的 token 必须清掉");
    assert!(!state_a.ws.has_connection("device-ub"));
    let on_a = state_a.get_peer("device-ub").expect("A 仍列出该设备（未绑定）");
    assert!(!on_a.paired, "A 侧回到未绑定");

    // B 侧：被通知到，同样清 token、保留设备行。
    // 通知在**后台**跑（`unpair_peer` 不等它，见那里的注释）：A 那一侧早已清完，
    // 这里等的是那条 HTTP 通知落地。
    wait_until("对端被通知到", || !credential::has_peer("device-ua")).await;
    let on_b = state_b.get_peer("device-ua").expect("B 仍列出该设备（未绑定）");
    assert!(!on_b.paired, "B 侧回到未绑定");

    // 被解除配对后，对方（尤其**旧版本**对端，它不知道要清 token）拿着旧 token 再来拨号时：
    // 必须被拒，而且**不能**被登记成本机的 "self" —— 否则对端显示"长连接"、本机却把它
    // 当"未绑定"，正是用户遇到的现象。
    credential::set_peer("device-ua", "stale-token-after-unpair").unwrap();
    orbdrop_lib::ws::ensure_link_now(&state_b, "device-ua");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let on_a = state_a.get_peer("device-ub").expect("A 仍列出该设备");
    assert!(!on_a.paired, "被解除配对后不能因为对端强行拨号就变回已配对");
    assert!(
        !state_a.ws.has_connection("device-ub") && !state_a.ws.has_connection("self"),
        "过期 token 的拨号必须被拒（不得以 self 身份混进本机）"
    );
    assert!(!state_b.ws.has_connection("device-ua"), "对端也不该连上");

    // 旧版本对端还握着旧 token 时，连共享项列表也拿不到（Bearer 反解不出身份 → 401）。
    // 来源用本机 LAN 地址，绕开"回环 = 本机 UI"的免鉴权分支。
    if let Some(lan_ip) = local_lan_ip() {
        let resp = orbdrop_lib::net::client()
            .get(format!("https://{lan_ip}:{port_a}/shares"))
            .bearer_auth("stale-token-after-unpair")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "被解除配对后，旧 token 不得再读到本机共享项"
        );
    }

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ua").ok();
    credential::remove_peer("device-ub").ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 解除配对**不等对端**：本机动作先做完，那条通知甩到后台（F-14「本机动作为主」）。
///
/// 对端地址挂在一个"只 accept、不回话"的 TCP 上：`POST /unpair` 拿不到任何响应，只能等满
/// 3s 超时才收场。旧实现把这次通知摆在本机动作前面 —— 用户点「解除配对」，凭据与界面就要
/// 干等好几秒（对端离线时必现，离线对端连 TCP 都拨不通）。
#[tokio::test]
async fn unpair_does_not_wait_for_an_unresponsive_peer() {
    credential::remove_peer("device-us").ok();

    let dir = std::env::temp_dir().join(format!("orbdrop_unpair_fast_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = make_state(3996, "device-um", "PC-UM", dir.clone());

    // 只收不回的死对端：连接收下就一直攥着，TLS 握手永远等不到 ServerHello。
    // 计数用来看住"那条通知确实发出去了"（否则本用例可能只证明了"什么都没发"）。
    let sink_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = sink_hits.clone();
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind sink");
    let sink_port = sink.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = sink.accept().await {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held.push(stream);
        }
    });

    state.add_peer_book(PeerDevice {
        id: "device-us".into(),
        name: "PC-US".into(),
        ip: "127.0.0.1".into(),
        port: sink_port,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false,
        relay: None,
    });
    credential::set_peer("device-us", &"e".repeat(64)).unwrap();

    let t0 = Instant::now();
    orbdrop_lib::unpair_peer(&state, "device-us").await.expect("unpair");
    let took = t0.elapsed();

    // 本机这一侧当场就清了 —— 不等那次通知。
    assert!(!credential::has_peer("device-us"), "本机凭据必须已经清掉");
    assert!(
        state.get_peer("device-us").is_some_and(|p| !p.paired),
        "设备行保留、回到未绑定"
    );
    assert!(
        took < Duration::from_secs(1),
        "解除配对不该等对端（通知的超时是 3s），实际耗时 {took:?}"
    );
    // 而且那条通知**确实**发了出去（只是没挡在上面那条路径上）—— 否则这个用例只证明了
    // "什么都没发"，什么都没守住。
    wait_until("后台通知发出去了", || sink_hits.load(std::sync::atomic::Ordering::SeqCst) > 0).await;

    let _ = std::fs::remove_dir_all(&dir);
}

/// NAT 后面的对端（安卓模拟器）：**来源地址不能当对端地址记**，可达性只能由长连接说话。
///
/// 用户报的现象是"PC 上那台模拟器一直显示「离线」，可配对、长连接、消息都正常"。根因在
/// 模拟器把宿主回环别名（`10.0.2.2`）拍成宿主的 `127.0.0.1`：**本机**这一侧看到的来源
/// 地址就是本机自己，配上对端自报的端口（默认同样是 3890）正好是本机的监听端点。记进
/// 地址簿之后一路错下去：探活问回自己、把**本机的**证书指纹钉到对端名下，最后这一行被
/// 当成"这个地址上换了设备"作废 —— 而它此后再没有地址可问，设备行就永远停在「离线」。
///
/// 这里把那一刻原样摆出来（邀请从回环来、`port` 报的正是**被访问实例自己**的监听端口），
/// 盯三件事：① 那一对地址一处都不记、本机的指纹也不许落到对端名下；② 链路建起来之前
/// 不许自称"在线"；③ 链路活着 = 在线、链路断了 = 回到离线（这种对端没有地址，探活、
/// mDNS 都够不着它，链路是唯一的证据）。
#[tokio::test]
async fn nat_peer_keeps_no_address_and_its_liveness_follows_the_link() {
    // 凭据库是机器级的：先抹掉上一轮（尤其是跑挂的那轮）留下的 token。
    credential::remove_peer("device-nat").ok();
    let save_dir = temp_share_dir("nat_save");
    let port = free_port();
    let state = make_state(port, "device-pc", "PC-NAT", save_dir.clone());
    let serving = orbdrop_lib::server::start_server(state.clone(), port)
        .await
        .expect("server");

    // ---- 模拟器发来配对邀请：来源 127.0.0.1，自报端口正是本机的监听端口 ----
    let invite_id = "invite-nat".to_string();
    let posted = {
        let url = format!("http://127.0.0.1:{serving}/pair/invite");
        let body = serde_json::json!({
            "inviteId": invite_id,
            "peerId": "device-nat",
            "peerName": "PJJ110",
            "port": serving,
        });
        tokio::spawn(async move {
            orbdrop_lib::net::client().post(url).json(&body).send().await
        })
    };
    wait_until("本机界面上出现这条邀请", || state.pair.has_invite(&invite_id)).await;
    assert!(
        !state.get_peer("device-nat").is_some_and(|p| p.paired),
        "还没人点「接收」，不许变成已配对"
    );
    orbdrop_lib::pair::accept_invite(&state, &invite_id).expect("accept invite");
    let resp = posted.await.expect("invite task").expect("invite request completes");
    assert_eq!(resp.status(), 200, "被接受后发起方拿到回执");
    let grant: serde_json::Value = resp.json().await.unwrap();
    let token = grant["token"].as_str().expect("token").to_string();

    // ---- ① 那一对地址一处都不记：本机拨不到它（来源地址指的就是本机自己） ----
    let row = state.get_peer("device-nat").expect("配对完设备行应该在");
    assert!(row.paired, "配对关系已经建立");
    assert_eq!(
        (row.ip.as_str(), row.port),
        ("", 0),
        "来源是本机自己的监听端点：不能当对端地址记下来（记下来会让探活问回自己）"
    );
    assert!(
        !row.online,
        "还没有链路：这种对端此刻送不到，不许先替它挂一颗绿点"
    );
    let our_fingerprint = orbdrop_lib::net::load_or_create_identity().unwrap().fingerprint;
    assert_ne!(
        credential::get_peer_fingerprint("device-nat").unwrap().as_deref(),
        Some(our_fingerprint.as_str()),
        "**本机的**证书指纹不许钉到对端名下"
    );

    // ---- ② 对端拨进来（长连接）：这是它可达的唯一证据 ----
    let url = orbdrop_lib::net::peer_ws_url("127.0.0.1", serving, &token, false);
    let ws = orbdrop_lib::net::connect_ws(&url, "127.0.0.1", serving)
        .await
        .expect("inbound ws from the nat'd peer");
    wait_until("本机把它认成在线的长连接设备", || {
        state.get_peer("device-nat").is_some_and(|p| p.online)
            && state.ws.has_connection("device-nat")
    })
    .await;

    // ---- ③ 链路一断就回到离线 ----
    drop(ws);
    wait_until("链路断开后回到离线", || {
        !state.get_peer("device-nat").is_some_and(|p| p.online)
    })
    .await;
    assert!(
        !state.ws.has_connection("device-nat"),
        "链路已经断了，不许还写着长连接"
    );

    orbdrop_lib::server::stop_server(&state).await;
    credential::remove_peer("device-nat").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 本机的局域网地址：用来模拟"来自另一台设备"的请求（回环会被当成本机 UI 免鉴权）。
fn local_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?; // 只为挑出出口网卡，不实际发包
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() { None } else { Some(ip) }
}

/// 同步上下文（没有 Tokio 运行时）里点「连接」/「同意接收推送」不能把进程打崩。
///
/// Tauri 的**同步**命令就跑在这种上下文里：修复前 `tokio::spawn` 直接 panic
/// （用户侧表现为设备 A 点「连接」时 App 闪退）。现在 `task::spawn` 会退到 Tauri 的全局
/// 运行时 —— 任务真的跑起来，但这里的目标是对端端口 1（没人听），所以会话照样建立不了。
/// 这条测试跑在普通 `#[test]` 里，正是那个没有 reactor 的场景。
#[test]
fn button_tasks_without_a_tokio_runtime_do_not_panic() {
    credential::remove_peer("device-zb").ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_nort_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    // 点「连接」必然走到 spawn 那一步（拨号只由这个入口发起）。
    let state = make_state(3994, "device-za", "PC-ZA", save_dir.clone());
    state.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-zb".into(),
        name: "PC-ZB".into(),
        ip: "127.0.0.1".into(),
        port: 1,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 「连接」
    orbdrop_lib::ws::ensure_link_now(&state, "device-zb");
    assert!(
        !state.ws.has_connection("device-zb"),
        "没有运行时就不该（也不可能）建立会话"
    );

    // 「同意接收推送」→ spawn_download（同样是同步命令触发的后台任务）
    credential::set_peer("device-zb", "nort-token").unwrap();
    state.push.insert_invite(orbdrop_lib::transfer::push_job::InboundInvite {
        invite_id: "nort-invite".into(),
        sender: orbdrop_lib::ws::events::PeerInfo { id: "device-zb".into(), name: "PC-ZB".into() },
        name: "hello.txt".into(),
        size: 11,
        is_dir: false,
        count: 1,
        files: vec![],
        // 老对端那条路（不带能力位）：这个用例测的是"没有 reactor 也不能 panic"。
        caps: Vec::new(),
        root: None,
        callback: Some(("127.0.0.1".into(), 1)),
        responder: None,
    });
    orbdrop_lib::transfer::accept_invite(state.clone(), "nort-invite", None).expect("accept");

    credential::remove_peer("device-zb").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// F-16 回归：配对只到"已配对"——**配对本身不建长连接**，谁都不许自己拨号。
///
/// 用户报的现象正是"配对完自己就变成长连接了"：配对只该确立关系。这里让配对双方
/// 都有地址、都握着 token（地址还都是对方自报/来源 IP 给的，等于把自动拨号的所有
/// 条件都摆齐），再给 mDNS 发现 / 5s 探活 / 启动这三条旧的自动路径留出出手的时间窗，
/// 断言一条链路都不许冒出来；随后用户点「连接」，链路才建立。
///
/// 后半段是「连接 / 断开」两个按钮的既有语义（与自动建链无关，仍然必须成立）。
#[tokio::test]
async fn pairing_stops_at_paired_until_the_user_clicks_connect() {
    // 凭据库是机器级的，进程结束也不会清：先抹掉上一轮（尤其是跑挂的那轮）留下的
    // token，否则 A 一启动就从凭据库重建出 B 的条目，"从未见过 B" 这个前提不成立。
    credential::remove_peer("device-aa").ok();
    credential::remove_peer("device-ab").ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_auto_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    let state_a = make_state(3992, "device-aa", "PC-AA", save_dir.clone());
    let state_b = make_state(3993, "device-ab", "PC-AB", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3992).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3993).await.expect("B server");

    // A 显示自己的配对码；A 此前对 B 一无所知（无 mDNS、无手动添加、无地址）。
    let code = state_a.pair.create_code();
    assert!(
        state_a.get_peer("device-ab").map_or(true, |p| p.ip.is_empty() && p.port == 0),
        "A must have no address for B before pairing"
    );

    // B 见过 A（mDNS/手动添加），读到 A 的码后向 A 换取 token。
    state_b.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-aa".into(),
        name: "PC-AA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    let resp = orbdrop_lib::net::client()
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({
            "peerId": "device-ab",
            "peerName": "PC-AB",
            "peerPort": port_b,
            "code": code,
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "valid code accepted");
    let conf: serde_json::Value = resp.json().await.unwrap();
    let token = conf["token"].as_str().expect("token").to_string();

    // B 存下 A 的 token 并标记已配对（镜像 pair_confirm）。
    credential::set_peer("device-aa", &token).unwrap();
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-aa".into(),
        name: "PC-AA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // A 从配对请求里学到了 B 的地址（来源 IP + B 自报端口）并落库 —— 有地址也不自己拨。
    let learned = state_a.get_peer("device-ab").expect("A now knows B");
    assert!(learned.paired, "A marks B paired");
    assert_eq!((learned.ip.as_str(), learned.port), ("127.0.0.1", port_b));

    // 关键断言：双方都已配对、地址齐全、token 在手，但没人点「连接」→ 一条链路都不许有。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_a.ws.has_connection("device-ab"),
        "配对不该自己建立长连接（A 侧）"
    );
    assert!(
        !state_b.ws.has_connection("device-aa"),
        "配对不该自己建立长连接（B 侧）"
    );

    // 用户点「连接」（A 侧，镜像 connect_peer）→ 链路这才起来。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ab");
    wait_until("点连接必须建立长连接", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;

    // ---- 下面是「断开 / 连接」两个按钮的既有语义 ----

    // 1) 被拨号的那一台（B，A 拨的号）点"断开"：双方都必须掉线。
    //    修复前只有本机认为断了，对端一直显示"长连接"。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop the peer's session too", || {
        !state_a.ws.has_connection("device-ab") && !state_b.ws.has_connection("device-aa")
    }).await;
    assert!(state_b.ws.is_paused("device-aa"));

    // 2) 断开方自己点"连接"：显式点击必须能连回来（哪个方向点都算数）。
    state_b.ws.resume("device-aa");
    orbdrop_lib::ws::ensure_link_now(&state_b, "device-aa");
    wait_until("断开方点连接必须能重新建立长连接", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;

    // 而且必须**稳住**：对端（A）的自动拨号循环此刻仍在跑，如果它无视"已经连上"
    // 再拨一条，两条链接会互相 register/unregister —— 界面就是"一会儿连一会儿断"。
    let mut flips = 0;
    let mut connected = true;
    let mut pattern = String::new();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa");
        if now != connected {
            flips += 1;
            pattern.push(if now { '1' } else { '0' });
            connected = now;
        }
    }
    assert_eq!(flips, 0, "重连后链路抖动（连/断交替，pattern 尾段={pattern}）");

    // 实验：两端同时点「连接」（各拨一条），应仍只有一条会话且不抖。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop", || !state_a.ws.has_connection("device-ab")).await;
    state_b.ws.resume("device-aa");
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ab");
    orbdrop_lib::ws::ensure_link_now(&state_b, "device-aa");
    wait_until("同时点连接应能连上", || state_a.ws.has_connection("device-ab")).await;
    let mut flips2 = 0;
    let mut connected2 = state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa");
    let mut pattern2 = String::new();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa");
        if now != connected2 {
            flips2 += 1;
            pattern2.push(if now { '1' } else { '0' });
            connected2 = now;
        }
    }
    assert_eq!(flips2, 0, "两端同时连接后抖动（pattern 尾段={pattern2}）");

    // 3) 再断开一次：对端（A）的自动重连要一直被拒，否则"断开"一秒后就被撤销。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop", || !state_a.ws.has_connection("device-ab")).await;
    tokio::time::sleep(Duration::from_millis(1200)).await; // A 的首次重拨已发生
    assert!(
        !state_a.ws.has_connection("device-ab"),
        "对端自动重连必须被拒（本机保持断开）"
    );

    // 4) 对端点"连接"（manual=1）：放行，并结束本机的暂停。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ab");
    wait_until("对端点连接必须能连上", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;
    assert!(
        !state_b.ws.is_paused("device-aa"),
        "对端显式请求连接 → 本机暂停结束"
    );

    // 5) manual 只算一次：上面 A 是"手动"拨上来的，B 再断开后不能因为 A 的循环
    //    还在跑就替 B 撤销暂停（否则 B 的「断开」又被悄悄推翻）。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop", || !state_a.ws.has_connection("device-ab")).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !state_a.ws.has_connection("device-ab"),
        "手动拨号的循环不能把对端的「断开」一直撤销掉"
    );
    assert!(state_b.ws.is_paused("device-aa"));

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-aa").ok();
    credential::remove_peer("device-ab").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// F-16 回归：关机时处于长连接的设备，重启后必须自己把链路接回来。
///
/// 用户的痛点：关掉 App 再启动，设备全回到"已配对"，得一台一台重新点「连接」。
/// 这里的"重启"是从**磁盘**重新读回配置再造一个 `AppState`，然后跑启动恢复
/// （`restore_links_on_startup`）—— 内存里什么都没带过去。
///
/// 反向同样要成立：点过「断开」的设备，重启后不许自己连回来（那个意图也跨重启）。
#[tokio::test]
async fn long_connection_is_restored_after_a_restart() {
    // 凭据库是机器级的：先抹掉上一轮（尤其是跑挂的那轮）留下的 token。
    credential::remove_peer("device-ra").ok();
    credential::remove_peer("device-rb").ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_restore_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    let state_a = make_state(3995, "device-ra", "PC-RA", save_dir.clone());
    let state_b = make_state(3996, "device-rb", "PC-RB", save_dir.clone());
    let config_path = state_a.config_path.clone();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3996).await.expect("B server");

    // 配对结果：一张 token，两侧各按对端 id 存一份（`PairResponse` 就是这个流向）。
    // token 用短值：Windows 单条凭据只有 2.5KB，测试条目别把上限顶掉。
    let token = format!("t-r-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    credential::set_peer("device-rb", &token).unwrap();
    credential::set_peer("device-ra", &token).unwrap();
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-rb".into(),
        name: "PC-RB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-ra".into(),
        name: "PC-RA".into(),
        ip: "127.0.0.1".into(),
        port: 3995,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 用户点「连接」（A 侧）→ 链路建立。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-rb");
    wait_until("点连接建立链路", || {
        state_a.ws.has_connection("device-rb") && state_b.ws.has_connection("device-ra")
    })
    .await;

    // 记忆写在**链路建立那一刻**（不是退出时），所以强杀进程/断电都不会丢。
    assert!(
        read_config(&config_path)
            .peers
            .iter()
            .find(|p| p.id == "device-rb")
            .expect("A 的地址簿里有 B")
            .auto_connect,
        "链路一建立就要把长连接意图写进地址簿"
    );
    assert!(
        read_config(&state_b.config_path)
            .peers
            .iter()
            .find(|p| p.id == "device-ra")
            .expect("B 的地址簿里有 A")
            .auto_connect,
        "被拨号的一端同样记下这次长连接（重启后这一侧也要接回来）"
    );

    // 模拟 A 退出：链路和拨号循环一起消失（`pause` 在测试里代表"进程不再持有链路"），
    // 磁盘上那句"上次是长连接"留着 —— 用户关掉 App 时就是这个状态。
    state_a.ws.pause("device-rb");
    wait_until("退出后链路消失", || !state_b.ws.has_connection("device-ra")).await;

    // ---- 重启 A：只从磁盘读配置，不点任何按钮 ----
    let state_a2 = Arc::new(AppState::new(read_config(&config_path), config_path.clone()));
    orbdrop_lib::ws::restore_links_on_startup(&state_a2);
    wait_until("重启后自动接回长连接", || {
        state_a2.ws.has_connection("device-rb") && state_b.ws.has_connection("device-ra")
    })
    .await;

    // ---- 反向：点过「断开」的设备，重启后不许自己连回来 ----
    state_a2.ws.pause("device-rb");
    state_a2.set_link_intent("device-rb", LinkIntent::Paused); // 镜像 disconnect_peer 的两步
    wait_until("断开 must drop", || !state_b.ws.has_connection("device-ra")).await;

    let state_a3 = Arc::new(AppState::new(read_config(&config_path), config_path.clone()));
    orbdrop_lib::ws::restore_links_on_startup(&state_a3);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_a3.ws.has_connection("device-rb"),
        "「断开」过的设备重启后不该自动连回来"
    );
    assert!(
        !state_b.ws.has_connection("device-ra"),
        "B 侧也不该出现链路（本机没拨号，B 也没有拨号循环）"
    );

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ra").ok();
    credential::remove_peer("device-rb").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

// ---------------------------------------------------------------------------
// 「断开」被对端记忆撤销的那条路（重启后）
// ---------------------------------------------------------------------------

/// F-16 回归：本机点过「断开」，**重启后仍要拒绝对端的自动重拨**。
///
/// 用户实际遇到的那条路：本机点「断开」→ 退出 → 重开。本机这边确实不再主动拨
/// （`auto_connect` 已清），但对端**没点过断开**、它还记着这次长连接：它一看见本机
/// 回到网上就拨过来。原来"拒绝对端的重拨"只活在内存里（`WsManager::paused`），重启
/// 把它一起带走了 —— 本机照收那条拨号，双方界面回到「长连接」，而且被拨号的一端顺手
/// 把 `auto_connect` 写回来，从此每次重启都自己连（用户看到的："主动断开、退出重开
/// 又连上了"）。
///
/// 所以这里验三件事：暂停**落盘**（`paused`）、重启后被读回并挡住对端的自动拨号
/// （403），以及"对端的人真的点了「连接」"这条正路照旧通（`manual=1` 结束本机暂停）。
#[tokio::test]
async fn a_disconnect_also_survives_a_restart_against_the_peers_memory() {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-hb-a-{tag}");
    let id_b = format!("d-hb-b-{tag}");

    let save_dir = std::env::temp_dir().join(format!("orbdrop_hb_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, &id_a, "PC-HBA", save_dir.clone());
    let state_b = make_state(port_b, &id_b, "PC-HBB", save_dir.clone());
    let config_path_a = state_a.config_path.clone();
    let config_path_b = state_b.config_path.clone();

    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let token = format!("t-hb-{tag}");
    credential::set_peer(&id_a, &token).unwrap();
    credential::set_peer(&id_b, &token).unwrap();

    // 两边都得起服务：重启后要验的正是**入站闸门**（对端拨过来时被 403 挡掉），
    // 没有服务就没有那个闸门可验。
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.mark_paired(&PeerDevice {
        id: id_b.clone(), name: "PC-HBB".into(), ip: "127.0.0.1".into(), port: port_b,
        online: true, paired: true, auto_connect: false, paused: false, relay: None,
    });
    state_b.mark_paired(&PeerDevice {
        id: id_a.clone(), name: "PC-HBA".into(), ip: "127.0.0.1".into(), port: port_a,
        online: true, paired: true, auto_connect: false, paused: false, relay: None,
    });

    // A 点「连接」→ 链路建立，两侧都记下这次长连接。
    orbdrop_lib::ws::ensure_link_now(&state_a, &id_b);
    wait_until("点连接建立链路", || {
        state_a.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;
    assert!(
        read_config(&config_path_b)
            .peers
            .iter()
            .find(|p| p.id == id_a)
            .expect("B 的地址簿里有 A")
            .auto_connect,
        "被拨号的一端也记着这次长连接 —— 这正是下面「重启后它还会拨过来」的前提"
    );

    // ---- A 点「断开」（镜像 `disconnect_peer`），然后退出 ----
    state_a.ws.pause(&id_b);
    state_a.set_link_intent(&id_b, LinkIntent::Paused);
    wait_until("断开后链路消失", || !state_b.ws.has_connection(&id_a)).await;
    let row = read_config(&config_path_a);
    let row = row.peers.iter().find(|p| p.id == id_b).expect("A 的地址簿里有 B");
    assert!(
        !row.auto_connect && row.paused,
        "「断开」的两个方向都要落盘：本机不拨（auto_connect=false）、也拒绝对端的拨（paused=true）"
    );

    // ---- 重启 A：服务也随进程换掉（新 state 起在同一个端口上）----
    orbdrop_lib::server::stop_server(&state_a).await;
    let state_a2 = Arc::new(AppState::new(read_config(&config_path_a), config_path_a.clone()));
    assert!(
        state_a2.ws.is_paused(&id_b),
        "暂停标记要从地址簿读回来 —— 它要是只活在内存里，重启就等于把「断开」撤了"
    );
    let port_a2 = orbdrop_lib::server::start_server(state_a2.clone(), port_a).await.expect("A server (restarted)");
    assert_eq!(port_a2, port_a, "重启后端口不变，对端存的地址才还能用");
    orbdrop_lib::ws::restore_links_on_startup(&state_a2);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!state_a2.ws.has_connection(&id_b), "本机不主动拨（这条没变过）");

    // ---- 对端还记着这次长连接：它看见本机回到网上就拨过来（真机上由探活/mDNS 触发）----
    orbdrop_lib::ws::redial_if_remembered(&state_b, &id_a);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_a2.ws.has_connection(&id_b) && !state_b.ws.has_connection(&id_a),
        "重启后「断开」仍然有效：对端的自动重拨必须被拒（403），链路不许自己接回来"
    );
    assert!(
        !read_config(&config_path_a)
            .peers
            .iter()
            .find(|p| p.id == id_b)
            .expect("A 的地址簿里有 B")
            .auto_connect,
        "被拨号的一端不该顺手把「要这条链」写回来"
    );

    // ---- 正路照旧：对端的人点「连接」→ `manual=1` → 本机的暂停结束 ----
    orbdrop_lib::ws::ensure_link_now(&state_b, &id_a);
    wait_until("对端点「连接」→ 恢复链路", || {
        state_a2.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;
    assert!(!state_a2.ws.is_paused(&id_b), "对端显式请求连接 → 本机的暂停结束");
    let row = read_config(&config_path_a);
    let row = row.peers.iter().find(|p| p.id == id_b).expect("A 的地址簿里有 B");
    assert!(
        row.auto_connect && !row.paused,
        "这条链是有人点「连接」要的：意图落盘，重启后该自动接回来"
    );

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a2).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path_a);
    let _ = std::fs::remove_file(&config_path_b);
}

// ---------------------------------------------------------------------------
// 自动恢复遇到"对端还没上线"
// ---------------------------------------------------------------------------

/// 关机时是长连接、但下次启动时对端还没开机：恢复拨号会先失败（地址没人应答），
/// 等对端真的上线（地址被修正 + 标记在线 —— 真机上这一步是 mDNS / 5s 探活做的），
/// **不需要任何人点「连接」**，它就该自己接上。
///
/// 这也解释了为什么自动拨号只在前几次死磕：对端不在的那些分钟里反复发 TCP 只会
/// 每次都等满超时（10060）；等它出现在网上再拨才准。
#[tokio::test]
async fn auto_restore_connects_once_the_peer_shows_up() {
    // device id 带短后缀：机器级凭据库是所有测试共用的，固定 id 会被别的用例的收尾
    // `remove_peer` 顺手抹掉（表现为"没有 token"这种莫名其妙的失败）。别用整条 uuid ——
    // Windows 单条凭据只有 2.5KB，测试条目越长，并行跑时越容易顶到上限（见 requirements 风险表）。
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-on-a-{tag}");
    let id_b = format!("d-on-b-{tag}");

    let save_dir = std::env::temp_dir().join(format!("orbdrop_online_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    // 端口也让内核现挑（配置里的端口只在真的起服务时用，这里 A 不起服务）。
    let state_a = make_state(0, &id_a, "PC-SA", save_dir.clone());
    let state_b = make_state(0, &id_b, "PC-SB", save_dir.clone());
    let config_path = state_a.config_path.clone();

    let token = format!("t-on-{tag}");
    credential::set_peer(&id_b, &token).unwrap();
    credential::set_peer(&id_a, &token).unwrap();
    // A 记得这次长连接，但手上的地址是个没人监听的端口（对端没开机 / 地址过期）。
    let (dead_port, base_port) = two_free_ports();
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-SB".into(),
        ip: "127.0.0.1".into(),
        port: dead_port,
        online: false,
        paired: true,
        auto_connect: true, paused: false, relay: None,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-SA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 「重启」A：它照记忆自动拨，但对端不在 → 先连不上。
    let state_a2 = Arc::new(AppState::new(read_config(&config_path), config_path.clone()));
    orbdrop_lib::ws::restore_links_on_startup(&state_a2);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !state_a2.ws.has_connection(&id_b),
        "对端还没上线，这一轮当然连不上"
    );

    // 对端上线：起它的服务，并把地址修正 + 标记在线（真机上由 mDNS/探活完成）。
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), base_port)
        .await
        .expect("B server");
    state_a2.upsert_peer(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-SB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    wait_until("对端上线后自动接回（无需点「连接」）", || {
        state_a2.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

// ---------------------------------------------------------------------------
// 明确拒绝 = 停手（403 / 401），以及 403 之后的恢复
// ---------------------------------------------------------------------------

/// 对端点过「断开」= **明确拒绝**：本机的自动重拨必须停手（不再每分钟去敲一次 ——
/// 那永远敲不出连接），并把理由留给界面。
///
/// 恢复有两条路：对端点「连接」拨过来，或本机点「连接」（`manual=1` 会替它结束暂停）。
#[tokio::test]
async fn a_peer_that_said_disconnect_stops_the_retry_loop() {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-st-a-{tag}");
    let id_b = format!("d-st-b-{tag}");
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_stop_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state_a = make_state(0, &id_a, "PC-SA", save_dir.clone());
    let state_b = make_state(0, &id_b, "PC-SB", save_dir.clone());

    // 一张 token，两侧各按对端 id 存一份（`PairResponse` 的流向）。
    let token = format!("t-st-{tag}");
    credential::set_peer(&id_b, &token).unwrap();
    credential::set_peer(&id_a, &token).unwrap();
    let (base_port, _spare) = two_free_ports();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), base_port)
        .await
        .expect("B server");
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-SB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: true, paused: false, relay: None,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-SA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // B 先点「断开」：它的 `events_ws_handler` 会把 A 的重拨一律 403。
    state_b.ws.pause(&id_a);

    // A 重启恢复 → 被明确拒绝 → 停手，并留下理由。
    orbdrop_lib::ws::restore_links_on_startup(&state_a);
    wait_until("对端拒绝后 A 必须停手", || {
        matches!(state_a.ws.link_state(&id_b), Some(LinkState::Stopped { .. }))
    })
    .await;
    assert_eq!(
        state_a.ws.link_state(&id_b),
        Some(LinkState::Stopped { reason: StopReason::PeerDisconnected }),
        "理由要留给界面（对端点过断开）"
    );
    assert!(!state_a.ws.has_connection(&id_b));

    // 停手就是停手：这段时间里不该再冒出会话（旧行为是每分钟敲一次，永远敲不通）。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!state_a.ws.has_connection(&id_b));
    assert!(!state_b.ws.has_connection(&id_a));
    assert_eq!(
        state_a.ws.link_state(&id_b),
        Some(LinkState::Stopped { reason: StopReason::PeerDisconnected })
    );

    // 恢复：本机点「连接」（`manual=1` 会替对端结束它自己的暂停）。
    state_a.ws.resume(&id_b);
    orbdrop_lib::ws::ensure_link_now(&state_a, &id_b);
    wait_until("点连接必须能连回来", || {
        state_a.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;
    assert!(!state_b.ws.is_paused(&id_a), "对端显式请求连接 → 它的暂停结束");
    assert_eq!(state_a.ws.link_state(&id_b), Some(LinkState::Connected));

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 对端不认本机的 token（它清过数据 / 已解除配对）= 401：同样必须停手，理由要能
/// 让界面说清"得重新配对"（继续拨到天荒地老也不会变成连接）。
#[tokio::test]
async fn a_peer_that_rejects_our_token_stops_the_retry_loop() {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-tk-a-{tag}");
    let id_b = format!("d-tk-b-{tag}");
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_tok_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state_a = make_state(0, &id_a, "PC-TA", save_dir.clone());
    let state_b = make_state(0, &id_b, "PC-TB", save_dir.clone());

    // A 手里有"给 B 的 token"，但 B 的凭据库里没有**别人**的这枚 token（它只能把它
    // 解到自己头上 → 401）。这正是"对端清过数据"之后本机的处境。
    let token = format!("t-tk-{tag}");
    credential::set_peer(&id_b, &token).unwrap();
    let (base_port, _spare) = two_free_ports();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), base_port)
        .await
        .expect("B server");
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-TB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: true, paused: false, relay: None,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-TA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    orbdrop_lib::ws::restore_links_on_startup(&state_a);
    wait_until("token 被否后 A 必须停手", || {
        matches!(state_a.ws.link_state(&id_b), Some(LinkState::Stopped { .. }))
    })
    .await;
    assert_eq!(
        state_a.ws.link_state(&id_b),
        Some(LinkState::Stopped { reason: StopReason::TokenRejected }),
        "理由要留给界面（需重新配对）"
    );
    assert!(!state_a.ws.has_connection(&id_b));
    assert!(!state_b.ws.has_connection(&id_a));

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn pair_info_answers_who_it_recognises_the_caller_as() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_info_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(3993, "device-info", "PC-INFO", save_dir.clone());
    let port = orbdrop_lib::server::start_server(state.clone(), 3993).await.expect("server");

    // 对端替我们存的那一条（键 = 我们 id，值 = 这张 token）。
    let caller = format!("device-caller-{}", uuid::Uuid::new_v4());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&caller, &token).unwrap();

    let client = orbdrop_lib::net::client();
    let url = format!("http://127.0.0.1:{port}/pair/info");

    // 带 token：回「你是谁」+「我把你认作谁」。对端换过 device id 之后，本机手里
    // 那条 token 还有没有用，只能靠这一问（`pairedAs` = 本机 id ⇒ 关系还活着）。
    let body: serde_json::Value = client
        .get(&url)
        .bearer_auth(&token)
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(body["id"], "device-info");
    assert_eq!(body["pairedAs"], caller.as_str(), "认得出带 token 的调用方");

    // 不带 token / token 不认识：回 null，而且绝不回显凭据本身。
    let body: serde_json::Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    assert!(body["pairedAs"].is_null(), "没带 token 就没什么可认的");
    let body: serde_json::Value = client
        .get(&url)
        .bearer_auth("not-a-token")
        .send().await.unwrap()
        .json().await.unwrap();
    assert!(body["pairedAs"].is_null(), "认不出的 token 不算数");

    // 探活/发现走的就是这一问：解析出来的身份与"它把本机认作谁"要一模一样。
    let seen = orbdrop_lib::state::fetch_identity(&orbdrop_lib::net::client(), "127.0.0.1", port, Some(&token))
        .await
        .expect("问得到身份");
    assert_eq!((seen.id.as_str(), seen.name.as_str()), ("device-info", "PC-INFO"));
    assert_eq!(seen.paired_as.as_deref(), Some(caller.as_str()));

    orbdrop_lib::server::stop_server(&state).await;
    credential::remove_peer(&caller).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 用户在界面上看到的那种"两台同名设备"：地址上已经是对方的新身份，旧行却因为
/// "这个地址有人答话"被判成永远在线。一轮探活之后旧行必须离线、忘掉地址 ——
/// 列表里只剩它真正的新身份，而配对记录与凭据一点都不能丢。
#[tokio::test]
async fn probe_hides_a_row_whose_address_answers_with_another_device_id() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_probe_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state_b = make_state(3999, "device-b-new", "PC-B", save_dir.clone());
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3999).await.expect("B server");

    let state_a = make_state(3994, "device-own-a", "PC-A", save_dir.clone());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer("device-b-old", &token).unwrap();
    // 地址簿里的旧行：对端在被清数据前的身份，地址还指着它现在的机器。
    state_a.add_peer_book(PeerDevice {
        id: "device-b-old".into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    orbdrop_lib::state::probe_peers_once(&state_a).await;

    let stale = state_a.get_peer("device-b-old").expect("旧行还在");
    assert!(!stale.online, "旧行必须离线：界面只显示在线设备，这就是去重");
    assert_eq!(
        (stale.ip.as_str(), stale.port),
        ("", 0),
        "地址作废，否则每轮探活都去问一个已经不属于它的地址"
    );
    assert!(stale.paired, "配对记录留着（对端拿原 id 回来还能用）");
    assert!(credential::has_peer("device-b-old"), "凭据不能因为探活被清掉");

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-b-old").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 回归（用户报的"设备列表里那台华为设备显示的是 deviceid"）：**占位名要能被探活问出的
/// 真名换掉**。
///
/// 占位名的来源是中转那条路：`observe_verified_addr` 为"核实过地址、但本机从没发现过"的
/// 设备建行时拿设备 id 顶着名字（`/relay/open` 的请求方自报、`relay::open` 核实过的目标）。
/// 这类设备本机 mDNS 看不见（跨段，正是它走中转的原因），会话信封也不会来 —— `/pair/info`
/// 那一问的回话于是成了唯一的真名来源，而此前探活只拿它判死活，名字随手丢掉。
#[tokio::test]
async fn probe_fills_a_placeholder_name_from_the_pair_info_answer() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_probe_name_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    let uniq = uuid::Uuid::new_v4().to_string();
    let peer_id = format!("device-name-{}", &uniq[..8]);
    let state_b = make_state(4321, &peer_id, "华为 Mate60", save_dir.clone());
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 4321).await.expect("B server");

    let state_a = make_state(4322, "device-own-name", "PC-A", save_dir.clone());
    // 本机从没看见过它，只是"这个地址被核实过"（中转控制面 / 路由记录那条路）：行里拿
    // 设备 id 顶着名字。
    state_a.observe_verified_addr(&peer_id, "127.0.0.1", port_b);
    assert_eq!(state_a.get_peer(&peer_id).unwrap().name, peer_id, "前提：占位名");
    assert!(!state_a.is_learned(&peer_id), "前提：它是本机自己的行");

    orbdrop_lib::state::probe_peers_once(&state_a).await;

    let row = state_a.get_peer(&peer_id).expect("行还在");
    assert_eq!(row.name, "华为 Mate60", "探活问出的真名要写回设备行");
    assert!(row.online, "同一拍里它当然是在线的");

    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 对端清过一部分状态、本机手里那条凭据还有效：本机两行（旧 id 已配对 + 新 id 未绑定）
/// 必须收敛成**一行**，而且配对状态跟着新 id 走 —— 用户不用重新配对。
#[test]
fn peer_reset_with_surviving_credential_migrates_the_row_to_the_new_id() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_migrate_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(3997, "own-device-mig", "PC-OWN", save_dir.clone());
    let config_path = state.config_path.clone();

    let old_id = format!("device-old-{}", uuid::Uuid::new_v4());
    let new_id = format!("device-new-{}", uuid::Uuid::new_v4());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&old_id, &token).unwrap();

    // 地址簿里的旧行：已配对、地址在、关机时是长连接。
    state.add_peer_book(PeerDevice {
        id: old_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.5".into(),
        port: 3890,
        online: true,
        paired: true,
        auto_connect: true, paused: false, relay: None,
    });
    // mDNS 已经发现了它对端的新身份：同一地址、另一个 id、未绑定。
    state.upsert_peer(&PeerDevice {
        id: new_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.5".into(),
        port: 3890,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    let seen = orbdrop_lib::state::PeerIdentity {
        id: new_id.clone(),
        name: "PC-B".into(),
        // 对端拿本机带过去的 token 反解出**本机 id**：配对关系还活着。
        paired_as: Some("own-device-mig".into()),
        relay: false,
        nets: Vec::new(),
        port: None,
    };
    assert!(
        orbdrop_lib::state::apply_identity_reconcile(&state, &old_id, &seen),
        "认得出凭据时要迁到新 id"
    );

    let peers = state.peers.lock().clone();
    let here: Vec<&PeerDevice> = peers
        .iter()
        .filter(|p| p.ip == "10.0.0.5" && p.port == 3890)
        .collect();
    assert_eq!(here.len(), 1, "同一台设备只该剩一行：{here:?}");
    let row = here[0];
    assert_eq!(row.id, new_id, "留下来的是它现在的身份");
    assert!(row.paired && row.online, "配对状态跟着新 id：仍是已配对");
    assert!(row.auto_connect, "「关机时连着长连接」的记忆一起迁过去");
    assert!(
        !peers.iter().any(|p| p.id == old_id),
        "旧 id 的行必须消失，否则又是两行"
    );

    // 凭据换键：token 值不变，键改成新 id（两边照旧互相认）。
    assert_eq!(
        credential::get_peer_token(&new_id).unwrap().as_deref(),
        Some(token.as_str())
    );
    assert!(
        credential::get_peer_token(&old_id).unwrap().is_none(),
        "旧键不能留：留着就成了死凭据"
    );

    // 落盘：重启后读到的就是这一行（内存里的状态不算）。
    let saved = read_config(&config_path);
    assert_eq!(saved.peers.len(), 1, "地址簿里也只留一行：{:?}", saved.peers);
    assert_eq!(saved.peers[0].id, new_id);
    assert!(saved.peers[0].paired && saved.peers[0].auto_connect);
    assert_eq!(
        (saved.peers[0].ip.as_str(), saved.peers[0].port),
        ("10.0.0.5", 3890)
    );

    credential::remove_peer(&new_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

/// 对端清数据时把凭据库一起清了：配对关系已经断了。这一行必须**从列表里消失**
/// （离线 + 忘掉地址），剩下对面真正的新身份；但凭据与配对记录不能被销毁 —— 对端
/// 拿原 id 回来时这一行还得能用。
#[test]
fn peer_reset_without_our_credential_hides_the_stale_row() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_stale_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(3998, "own-device-stale", "PC-OWN", save_dir.clone());
    let config_path = state.config_path.clone();

    let old_id = format!("device-old-{}", uuid::Uuid::new_v4());
    let new_id = format!("device-new-{}", uuid::Uuid::new_v4());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&old_id, &token).unwrap();
    state.add_peer_book(PeerDevice {
        id: old_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.6".into(),
        port: 3890,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });
    state.upsert_peer(&PeerDevice {
        id: new_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.6".into(),
        port: 3890,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    let seen = orbdrop_lib::state::PeerIdentity {
        id: new_id.clone(),
        name: "PC-B".into(),
        paired_as: Some("some-other-device".into()), // 它不认我们的 token
        relay: false,
        nets: Vec::new(),
        port: None,
    };
    assert!(
        !orbdrop_lib::state::apply_identity_reconcile(&state, &old_id, &seen),
        "认不出凭据就不能迁走配对关系"
    );

    let peers = state.peers.lock().clone();
    let stale = peers.iter().find(|p| p.id == old_id).expect("旧行还在");
    assert!(!stale.online, "作废的行必须离线（界面只显示在线设备）");
    assert_eq!(
        (stale.ip.as_str(), stale.port),
        ("", 0),
        "地址要忘掉，否则每轮探活都去问一个不属于它的地址"
    );
    assert!(stale.paired, "配对记录留着：对端拿原 id 回来还能用");
    assert_eq!(
        credential::get_peer_token(&old_id).unwrap().as_deref(),
        Some(token.as_str()),
        "凭据不能被销毁"
    );
    assert_eq!(
        peers.iter().filter(|p| p.ip == "10.0.0.6").count(),
        1,
        "地址上只剩对面真正的新身份"
    );

    // 落盘的地址同样要作废：重启后不再去探它，也不会以"在线"身份冒出来。
    let saved = read_config(&config_path);
    let saved_stale = saved
        .peers
        .iter()
        .find(|p| p.id == old_id)
        .expect("地址簿里旧行还在（只是没有地址了）");
    assert_eq!((saved_stale.ip.as_str(), saved_stale.port), ("", 0));
    assert!(
        !saved_stale.online,
        "重启后不许再以「在线」出现：它没有地址，没有任何东西会把它纠正回来"
    );

    credential::remove_peer(&old_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

/// 重新配对必须把上一次留下的"停手理由"清掉。
///
/// token 失效（对端清过数据 / 它把我们解除配对了）时，那台设备停在
/// `Stopped{TokenRejected}` —— 界面显示「需配对」、行内给的是「配对」按钮。用户点它
/// 走完配对、换到新 token 之后，这个停手理由就失效了：不清掉，界面会继续显示
/// 「需配对」并继续给「配对」按钮 —— 配对明明成功了，界面却还在叫他重新配对。
#[test]
fn re_pairing_clears_the_stale_stop_reason() {
    let save_dir = temp_share_dir("repair_save_dir");
    let state = make_state(3977, "device-repair", "PC-REPAIR", save_dir.clone());
    let config_path = state.config_path.clone();
    let dead = PeerDevice {
        id: "device-dead-token".to_string(),
        name: "PC-DEAD".to_string(),
        ip: "10.0.0.9".to_string(),
        port: 3890,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    };
    state.upsert_peer(&dead);
    state
        .ws
        .set_link_state(&dead.id, LinkState::Stopped { reason: StopReason::TokenRejected });
    assert_eq!(
        state.ws.link_state(&dead.id),
        Some(LinkState::Stopped { reason: StopReason::TokenRejected }),
        "前置条件：这台设备此刻停在「需配对」"
    );

    // 配对成功这一刻（两条路径 —— 对端拿本机的码来换 token、本机输入对端的码 ——
    // 都收敛到 `mark_paired`）。
    state.mark_paired(&dead);

    assert_eq!(
        state.ws.link_state(&dead.id),
        None,
        "换到新 token 之后不该再显示「需配对」（否则界面会一直把他按回配对流程）"
    );
    assert!(state.get_peer(&dead.id).expect("设备行还在").paired);

    credential::remove_peer(&dead.id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

/// 配对邀请（F-12 免配对码那条路）：A 点「发起邀请」→ 邀请作为一次 HTTP POST 推到 B，
/// 在 B 的面板右下角挂一条带「接收/拒绝」的通知 → B 点「接收」→ 两边各存下对方铸的 token。
/// **全程没有人输入配对码。**
///
/// 换来的两枚 token 必须真能过对面的认证（不是"写进文件就算配对"），所以最后拿它们各调
/// 一次对端的受保护接口（`/shares`）。另外钉住两道边界：没人点「接收」之前谁都不许变成
/// "已配对"；自称已配对设备却不带 token 的邀请必须被拒。
#[tokio::test]
async fn pair_invite_swaps_tokens_without_typing_a_code() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_invite_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    // 上一次跑到中途失败会留下凭据（AppState::new 会按凭据库里的 id 合成一行：
    // 没有地址、离线）—— 先把这两台清干净，测试才不依赖上一次跑成什么样。
    credential::remove_peer("device-inv-a").ok();
    credential::remove_peer("device-inv-b").ok();
    let (pa, pb) = two_free_ports();
    let state_a = make_state(pa, "device-inv-a", "PC-A", save_dir.clone());
    let state_b = make_state(pb, "device-inv-b", "PC-B", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), pa).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), pb).await.expect("B server");

    // A 知道 B 的地址（mDNS 发现 / 手动添加），但两边**都还没配对** —— 这正是邀请要解决的场合。
    state_a.upsert_peer(&PeerDevice {
        id: "device-inv-b".into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    // ---- A 点「发起邀请」----
    let invite_id = orbdrop_lib::pair::invite_peer(state_a.clone(), "device-inv-b")
        .await
        .expect("invite sent");
    wait_until("B 界面上出现这条邀请", || state_b.pair.has_invite(&invite_id)).await;
    assert!(
        !state_b.get_peer("device-inv-a").is_some_and(|p| p.paired),
        "还没人点「接收」，两边都不许变成已配对"
    );

    // ---- B 在通知里点「接收」----
    let name = orbdrop_lib::pair::accept_invite(&state_b, &invite_id).expect("accept invite");
    assert_eq!(name, "PC-A", "接受邀请返回的是发起方设备名");

    wait_until("A 落库这条配对", || {
        state_a.get_peer("device-inv-b").is_some_and(|p| p.paired)
    })
    .await;
    assert!(
        state_b.get_peer("device-inv-a").is_some_and(|p| p.paired),
        "B 也变「已配对」"
    );

    // 一对设备只有**一枚** token（由"收下配对"的那一侧铸出，两边各按对方的 id 存下来）——
    // 与配对码那条路同一个约定：A 手里那枚是 B 铸的，B 手里就是同一枚。
    let token_for_a = credential::get_peer_token("device-inv-b")
        .unwrap()
        .expect("A 存下 B 给的 token");
    let token_for_b = credential::get_peer_token("device-inv-a")
        .unwrap()
        .expect("B 存下 A 给的 token");
    assert_eq!(token_for_a.len(), 64, "32 字节 hex");
    assert_eq!(token_for_a, token_for_b, "一对设备共用一枚 token，两边都存下了它");

    let client = orbdrop_lib::net::client();
    let resp = client
        .get(format!("http://127.0.0.1:{port_b}/shares"))
        .bearer_auth(&token_for_a)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "A 拿邀请换来的 token 必须能过 B 的认证");
    let resp = client
        .get(format!("http://127.0.0.1:{port_a}/shares"))
        .bearer_auth(&token_for_b)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "B 拿邀请换来的 token 必须能过 A 的认证");

    // ---- 冒充已配对设备：不带 token 的邀请必须被拒 ----
    // 没有这道关，谁都能自称你的老搭档；用户点一次「接收」，那台设备的 token 就被换成了
    // 对面手里的那枚（与 `/push/invite` 同一道关）。
    let forged = client
        .post(format!("http://127.0.0.1:{port_b}/pair/invite"))
        .json(&serde_json::json!({
            "inviteId": uuid::Uuid::new_v4().to_string(),
            "peerId": "device-inv-a",
            "peerName": "冒充的 PC-A",
            "port": port_a,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 401, "自称已配对设备必须出示 token");

    // ---- 重新配对同一条路：这次 B 拒绝，已有的关系一个字节都不该被动 ----
    let invite2 = orbdrop_lib::pair::invite_peer(state_a.clone(), "device-inv-b")
        .await
        .expect("second invite");
    wait_until("B 界面上出现第二条邀请", || state_b.pair.has_invite(&invite2)).await;
    orbdrop_lib::pair::reject_invite(&state_b, &invite2);
    wait_until("A 收到拒绝、这条邀请收尾", || !state_a.pair.has_outbound(&invite2)).await;
    assert!(!state_b.pair.has_invite(&invite2), "拒绝后邀请从本机的表里摘掉");
    assert_eq!(
        credential::get_peer_token("device-inv-b").unwrap().as_deref(),
        Some(token_for_a.as_str()),
        "被拒的邀请不能改动已有的凭据"
    );

    // ---- 撤回（用户点「取消」）：对端界面上那条通知跟着撤掉 ----
    let invite3 = orbdrop_lib::pair::invite_peer(state_a.clone(), "device-inv-b")
        .await
        .expect("third invite");
    wait_until("B 界面上出现第三条邀请", || state_b.pair.has_invite(&invite3)).await;
    orbdrop_lib::pair::withdraw_invite_remote(state_a.clone(), &invite3)
        .await
        .expect("withdraw");
    wait_until("B 那条邀请被撤回后自己消失", || !state_b.pair.has_invite(&invite3)).await;

    credential::remove_peer("device-inv-a").ok();
    credential::remove_peer("device-inv-b").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&state_a.config_path);
    let _ = std::fs::remove_file(&state_b.config_path);
}

/// 「离线」（隐身）把**一切**入站请求挡在门外，切回在线立刻恢复。
///
/// 这条是隐身能不能成立的关键：对端每 5 秒打一次 `/pair/info` 探活，只要还应答，它就把
/// 本机当"在线"（`state::probe_step` 认的是 200）——只注销 mDNS 瞒得住发现、瞒不住状态。
/// WS 升级（`/events`）同样在公开路由上，也必须在升级之前被拦下。
#[tokio::test]
async fn offline_refuses_every_inbound_request() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_off_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(4190, "device-off", "PC-Off", save_dir.clone());
    let port = orbdrop_lib::server::start_server(state.clone(), 4190)
        .await
        .expect("server");
    let client = orbdrop_lib::net::client();
    let info = format!("http://127.0.0.1:{port}/pair/info");
    let events = format!("http://127.0.0.1:{port}/events?token=whatever");

    assert_eq!(client.get(&info).send().await.unwrap().status(), 200, "默认在线");

    state.set_local_online(false);
    assert_eq!(
        client.get(&info).send().await.unwrap().status(),
        503,
        "离线：探活不许再回 200，否则对端一直当本机在线"
    );
    assert_eq!(
        client.get(&events).send().await.unwrap().status(),
        503,
        "离线：WS 升级也要在升级之前被拦下"
    );
    assert_eq!(
        state.config.lock().online,
        false,
        "离线要落盘（跨重启有效）"
    );

    state.set_local_online(true);
    assert_eq!(client.get(&info).send().await.unwrap().status(), 200, "切回在线立刻恢复");

    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&state.config_path);
}

/// 跨网段发现（F-10 延伸）的服务端契约：已配对的对端来问「本机此刻看得见的设备」，
/// 拿到的必须是**整份视野**（多网卡时每一块网卡上的设备都在里面，不按拨号地址划段）；
/// 呼叫方自己、本机自己、离线的、以及**别台设备转述来的**（`state.learned`，发现不做
/// 多跳），一个都不回。
#[tokio::test]
async fn discover_reports_the_whole_view_and_needs_a_paired_token() {
    let Some(lan_ip) = local_lan_ip() else {
        eprintln!("跳过：本机没有局域网地址，模拟不出「来自另一台设备」的请求");
        return;
    };
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_disc_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    // 凭据库是**全局共享**的（一个数据目录一份），而测试是并行跑的：id 必须带 uuid，
    // 否则和别的测试互相清键，谁的 token 都活不到被用上那一刻。
    let uniq = uuid::Uuid::new_v4().to_string();
    let tag = &uniq[..8];
    let b_id = format!("disc-b-{tag}");
    let a_id = format!("disc-a-{tag}");
    let c_id = format!("disc-c-{tag}");
    let far_id = format!("disc-far-{tag}");
    let sleep_id = format!("disc-sleep-{tag}");

    let state_b = make_state(4390, &b_id, "PC-B", save_dir.clone());
    let add = |id: &str, ip: String, port: u16, online: bool, paired: bool| {
        state_b.add_peer_book(PeerDevice {
            id: id.into(),
            name: id.to_uppercase(),
            ip,
            port,
            online,
            paired,
            auto_connect: false, paused: false, relay: None,
        });
    };
    // 本机这一段里在线的设备。
    add(&c_id, lan_ip.to_string(), 4391, true, false);
    // 另一段的设备（本机多网卡时另一个网段上看得见的）：**整份视野**里必须有它 ——
    // 按拨号地址划段的老写法会把它挡掉，那正是"一台锚点只换回一个网段"的由来。
    add(&far_id, "198.51.100.7".into(), 4392, true, false);
    // 呼叫方自己：表里不该回显它（A 已经知道自己是谁）。
    add(&a_id, lan_ip.to_string(), 4393, true, true);
    // 离线的：这一层只回答"此刻有谁在"。
    add(&sleep_id, lan_ip.to_string(), 4394, false, false);
    // 别台设备转述来的（`learn_peers` 走的就是这条）：**不进表** —— 发现只走单播、
    // 不做多跳，否则一台锚点会顺着链路把整张网蔓延出去。
    let learned_id = format!("disc-learned-{tag}");
    state_b.learn_peers(
        "disc-voucher",
        vec![PeerDevice {
            id: learned_id.clone(),
            name: "LEARNED".into(),
            ip: lan_ip.to_string(),
            port: 4395,
            online: true,
            paired: false,
            auto_connect: false, paused: false, relay: None,
        }],
    );
    assert!(
        state_b.get_peer(&learned_id).is_some(),
        "前提：转述来的行确实在本机视野（peers）里，只是不该被报出去"
    );

    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 4390)
        .await
        .expect("B server");

    let token = format!("tok-{}", uuid::Uuid::new_v4());
    // B 手里那条：键是 A 的 id —— 它据此才知道"带这个 token 的是 A"。
    credential::set_peer(&a_id, &token).unwrap();

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan_ip}:{port_b}/discover");

    // 没带 / 带错 token：拿不到表。这就是"匹配上之后才发现"那道门。
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "没配对不许看本机认识谁"
    );
    assert_eq!(
        client.get(&url).bearer_auth("not-a-token").send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let body: serde_json::Value = client
        .get(&url)
        .bearer_auth(&token)
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(body["id"], b_id.as_str());
    assert_eq!(
        body["port"].as_u64(),
        Some(u64::from(port_b)),
        "报的必须是实际监听的端口（配置端口被占时会顺延）"
    );

    let ids: Vec<&str> = body["peers"]
        .as_array()
        .expect("peers 数组")
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(ids.contains(&c_id.as_str()), "在线的设备要在表里: {ids:?}");
    assert!(
        ids.contains(&far_id.as_str()),
        "**整份视野**：别的网段上的设备也要回（多网卡时每一段都要交换）: {ids:?}"
    );
    assert!(!ids.contains(&a_id.as_str()), "呼叫方自己不许回: {ids:?}");
    assert!(!ids.contains(&sleep_id.as_str()), "离线的不回: {ids:?}");
    assert!(!ids.contains(&b_id.as_str()), "本机自己不许回: {ids:?}");
    assert!(
        !ids.contains(&learned_id.as_str()),
        "别台设备转述来的（learned）不许再往外报：发现不做多跳: {ids:?}"
    );

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&a_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 请求方那半边（F-106）：目标**直连探不通**时，向一台"已配对 + 自报可中转 + 在线"的设备
/// 请一条端点；拿到之后本机对目标的**拨号地址**就改走端点（`PeerDevice::dial`），直连恢复
/// 再收起。这条用例同时钉住"请求方不需要自己的开关"和"只挑自报愿意中转的那台"。
#[tokio::test]
async fn requester_opens_a_relay_only_for_an_unreachable_peer() {
    isolate_data_dir();
    let Some(lan_ip) = local_lan_ip() else {
        return; // 没有局域网地址的机器：这条路上"拨谁"本身就没有答案
    };
    let root = std::env::temp_dir().join(format!("orbdrop_it_relay_req_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let relay_id = format!("relay-node-{}", uuid::Uuid::new_v4());
    let requester_id = format!("relay-req-{}", uuid::Uuid::new_v4());
    let target_id = format!("relay-goal-{}", uuid::Uuid::new_v4());
    let silent_id = format!("relay-silent-{}", uuid::Uuid::new_v4());

    // 中转方 R：开着开关，地址簿里目标"在线"（= R 探活够得着它）。
    let relay_state = make_state(4493, &relay_id, "RELAY", root.clone());
    relay_state.config.lock().allow_relay = true;
    relay_state.add_peer_book(PeerDevice {
        id: target_id.clone(),
        name: "GOAL".into(),
        ip: "127.0.0.1".into(),
        port: 1, // 这条用例只验请求方的决策，不需要 R 真把字节送到目标
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    let relay_port = orbdrop_lib::server::start_server(relay_state.clone(), 4493)
        .await
        .expect("relay server");

    // 请求方 A：目标那个端口上没人（直连必然不通）；R 在线、已配对，并**自报可中转**。
    let requester = make_state(4494, &requester_id, "REQ", root.clone());
    let dead_port = free_port();
    requester.add_peer_book(PeerDevice {
        id: target_id.clone(),
        name: "GOAL".into(),
        ip: "127.0.0.1".into(),
        port: dead_port,
        online: false,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    requester.add_peer_book(PeerDevice {
        id: relay_id.clone(),
        name: "RELAY".into(),
        // **必须是局域网地址**：打 127.0.0.1 的话服务端会把调用方认成"本机 UI"
        // （回环 = `Caller::Local`，见 `auth_middleware`），而中转只对已配对对端开放。
        ip: lan_ip.to_string(),
        port: relay_port,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 配对关系的凭据面：A 手里"对 R"的 token，与 R 手里"对 A"的 token 是**同一枚**。
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&relay_id, &token).unwrap();
    credential::set_peer(&requester_id, &token).unwrap();
    // 能力位平时由 5s 探活学到（`probe_addr`），这里直接给上。
    requester.set_relay_capable(&relay_id, true);

    let relay = orbdrop_lib::relay::ensure_for(&requester, &target_id)
        .await
        .expect("直连不通时应当开出一条中转");
    assert_eq!(relay.via, relay_id, "中转方就是那台自报愿意中转的设备");
    let goal = requester.get_peer(&target_id).expect("目标行还在");
    assert_eq!(
        goal.dial(),
        (relay.host.as_str(), relay.port),
        "拨号地址必须改走端点（十几处数据面都从 dial() 取值）"
    );
    assert_eq!(goal.ip, "127.0.0.1", "它自己的地址不变 —— 地址簿记的是它长什么样");
    assert_eq!(goal.port, dead_port, "端口也不变");

    // 直连恢复 → 收起中转（`probe_peers_once` 在直连那一跳 Online 时就是这么做的）。
    orbdrop_lib::relay::clear_for(&requester, &target_id);
    assert_eq!(
        requester.get_peer(&target_id).unwrap().dial(),
        ("127.0.0.1", dead_port),
        "收起之后回到直连地址"
    );

    // 关着开关的中转方请不动：**请求方自己不需要开关**，靠的是对端自报的能力位。
    let silent_state = make_state(4495, &silent_id, "SILENT", root.clone());
    silent_state.add_peer_book(PeerDevice {
        id: target_id.clone(),
        name: "GOAL".into(),
        ip: "127.0.0.1".into(),
        port: 1,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    let silent_port = orbdrop_lib::server::start_server(silent_state.clone(), 4495)
        .await
        .expect("silent server");
    requester.upsert_peer(&PeerDevice {
        id: silent_id.clone(),
        name: "SILENT".into(),
        ip: lan_ip.to_string(),
        port: silent_port,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });
    credential::set_peer(&silent_id, &token).unwrap();
    credential::set_peer(&requester_id, &token).unwrap();
    requester.set_relay_capable(&silent_id, true);
    // 目标行刚被收起，可以再请一次；这一轮唯一的候选是"关着开关的那台" → 请不动。
    assert!(
        orbdrop_lib::relay::ensure_for(&requester, &target_id).await.is_none(),
        "关着开关的中转方必须请不动（403）"
    );
    assert_eq!(
        requester.get_peer(&target_id).unwrap().dial(),
        ("127.0.0.1", dead_port),
        "请不动就还是直连地址，不能凭空挂上一跳"
    );

    orbdrop_lib::server::stop_server(&requester).await;
    orbdrop_lib::server::stop_server(&relay_state).await;
    orbdrop_lib::server::stop_server(&silent_state).await;
    credential::remove_peer(&relay_id).ok();
    credential::remove_peer(&silent_id).ok();
    credential::remove_peer(&requester_id).ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// 中转方**不认识**目标时的那一条路（跨网段常见：mDNS 跨不过路由、手动加的还可能是旧 id，
/// 于是"够得着却从没认识过"）：请求方把目标的地址一起捎来，中转方到那个地址核实身份后照转。
/// 核实这一关不能省 —— 不然中转方就成了"给任意局域网地址开隧道"的工具。
#[tokio::test]
async fn relay_open_accepts_a_verified_address_for_an_unknown_target() {
    isolate_data_dir();
    let Some(lan_ip) = local_lan_ip() else {
        return;
    };
    let root = std::env::temp_dir().join(format!("orbdrop_it_relay_hint_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let relay_id = format!("relay-hint-{}", uuid::Uuid::new_v4());
    let caller_id = format!("relay-hint-caller-{}", uuid::Uuid::new_v4());
    let goal_id = format!("relay-hint-goal-{}", uuid::Uuid::new_v4());

    // "目标"是一台**真的 OrbDrop**（因为中转方要核验身份）。
    let goal_state = make_state(4496, &goal_id, "GOAL", root.clone());
    let goal_port = orbdrop_lib::server::start_server(goal_state.clone(), 4496)
        .await
        .expect("goal server");

    // 中转方：开着开关，地址簿里**没有**这台目标。
    let relay_state = make_state(4497, &relay_id, "RELAY", root.clone());
    relay_state.config.lock().allow_relay = true;
    let relay_port = orbdrop_lib::server::start_server(relay_state.clone(), 4497)
        .await
        .expect("relay server");
    relay_state.add_peer_book(PeerDevice {
        id: caller_id.clone(),
        name: "CALLER".into(),
        ip: lan_ip.to_string(),
        port: 4498,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&caller_id, &token).unwrap();

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan_ip}:{relay_port}/relay/open");
    let ask = |target: &str, addr: serde_json::Value| {
        client
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "target": target, "addr": addr }))
    };

    // 1) 不带地址、本机又不认识它 → 请不动（这是"不认识 + 给不出可核验地址"那一档）。
    assert_eq!(
        ask(&goal_id, serde_json::Value::Null).send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // 2) 给了地址、但那个地址上的设备**不是**它自称的那台（拿中转方自己的地址来试）→ 仍请不动。
    assert_eq!(
        ask(&goal_id, serde_json::json!(format!("{lan_ip}:{relay_port}")))
            .send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "地址上的身份与声明的 id 不符时必须拒绝"
    );

    // 3) 给了地址且身份核验通过 → 开出来，并且**真的把字节送到那台目标**。
    let opened: serde_json::Value = ask(&goal_id, serde_json::json!(format!("{lan_ip}:{goal_port}")))
        .send().await.unwrap().json().await.unwrap();
    let host = opened["host"].as_str().expect("host").to_string();
    let endpoint_port = u16::try_from(opened["port"].as_u64().expect("port")).expect("端口范围");

    let relay_ip = orbdrop_lib::net::parse_host(&host).expect("端点主机可解析");
    let dial_ip = match relay_ip {
        std::net::IpAddr::V6(v6) => std::net::IpAddr::V4(v6.to_ipv4_mapped().expect("映射地址")),
        std::net::IpAddr::V4(v4) => std::net::IpAddr::V4(v4),
    };
    // 经端点问身份：回话必须是**目标**那台（中转方自己也不是它）。
    let end_client = orbdrop_lib::net::client();
    let via_endpoint: serde_json::Value = end_client
        .get(&format!("https://{dial_ip}:{endpoint_port}/pair/info"))
        .send()
        .await
        .expect("经端点请求")
        .json()
        .await
        .expect("端点回话");
    assert_eq!(
        via_endpoint["id"].as_str(),
        Some(goal_id.as_str()),
        "端点后面站着的必须是目标那台设备"
    );

    orbdrop_lib::server::stop_server(&goal_state).await;
    orbdrop_lib::server::stop_server(&relay_state).await;
    credential::remove_peer(&caller_id).ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// 「看得见」与「连得上」是两件事（F-106）：前者只说明"对端的 mDNS 记录还在"，
/// 后者只由探活决定。列表把两档分开显示（灰行「看得见·连不上」/ 正常在线行）。
#[tokio::test]
async fn seen_is_independent_of_reachability() {
    isolate_data_dir();
    let root = std::env::temp_dir().join(format!("orbdrop_it_seen_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let state = make_state(4501, "seen-owner", "SEEN", root.clone());

    let peer_id = format!("seen-peer-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: peer_id.clone(),
        name: "GHOST".into(),
        ip: "127.0.0.1".into(),
        port: 1,
        online: false,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });

    assert!(!state.is_seen(&peer_id), "没见过的不算");
    state.note_seen("");
    assert!(!state.is_seen(""), "记录里没带 id 的（不是本项目/旧版本）不当「看得见」");

    state.note_seen(&peer_id);
    assert!(state.is_seen(&peer_id), "收到记录 = 看得见");
    assert!(
        !state.get_peer(&peer_id).expect("行还在").online,
        "「看得见」不等于「连得上」：`online` 只由探活决定"
    );

    state.clear_seen(&peer_id);
    assert!(!state.is_seen(&peer_id), "记录被撤（告别包 / TTL 过期）就不再「看得见」");

    let _ = std::fs::remove_dir_all(&root);
}

/// 消息够不着时**当场试一次中转**（F-106）：探活每 5s 才去开中转，等它的话消息会在眼前
/// 卡住。这条用例只钉"试了"：候选在（已配对 + 在线 + 自报可中转）、直连不通 →
/// 发送那一跳的限流表里应该出现这台目标。中转**开成之后的重发**由端到端那条路验
/// （`ensure_for` 与 `deliver` 各自的用例）。
#[tokio::test]
async fn an_unreachable_send_tries_a_relay_immediately() {
    isolate_data_dir();
    let root = std::env::temp_dir().join(format!("orbdrop_it_send_relay_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let state = make_state(4502, "send-me", "ME", root.clone());

    // 目标：地址上没人听（直连必然不通）。
    let goal_id = format!("send-goal-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: goal_id.clone(),
        name: "GOAL".into(),
        ip: "127.0.0.1".into(),
        port: free_port(),
        online: false,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    // 候选中转方：已配对、在线、自报愿意中转（地址上也没人听 —— 会失败，但"试过"要发生）。
    let via_id = format!("send-via-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: via_id.clone(),
        name: "VIA".into(),
        ip: "127.0.0.1".into(),
        port: free_port(),
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });
    state.set_relay_capable(&via_id, true);
    credential::set_peer(&via_id, "tok-via").ok();

    let _ = orbdrop_lib::chat::send_text(&state, &[goal_id.clone()], "够不着的一句", None);

    wait_until("发送那一跳去试了中转", || {
        state.relay_attempts.lock().contains_key(&goal_id)
    })
    .await;

    orbdrop_lib::server::stop_server(&state).await;
    credential::remove_peer(&via_id).ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// 探活那条 5 秒的循环顺手把「对端此刻看得见的设备」学过来：学来的行是临时的
/// （不写地址簿、不被再探活），而且**存活跟着担保人走** —— 担保人不再提到它，超时收走。
#[tokio::test]
async fn probe_learns_the_peers_view_and_sweeps_it_when_the_voucher_goes_quiet() {
    let Some(lan_ip) = local_lan_ip() else {
        eprintln!("跳过：本机没有局域网地址，模拟不出「来自另一台设备」的请求");
        return;
    };
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_learn_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    // 凭据库全局共享、测试并行跑：id 带 uuid，别和别的测试互相清键（同上）。
    let uniq = uuid::Uuid::new_v4().to_string();
    let tag = &uniq[..8];
    let b_id = format!("learn-b-{tag}");
    let a_id = format!("learn-a-{tag}");
    let c_id = format!("learn-c-{tag}");

    let state_b = make_state(4395, &b_id, "PC-B", save_dir.clone());
    state_b.add_peer_book(PeerDevice {
        id: c_id.clone(),
        name: "PC-C".into(),
        ip: lan_ip.to_string(),
        port: 4396,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 4395)
        .await
        .expect("B server");

    let state_a = make_state(4397, &a_id, "PC-A", save_dir.clone());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&a_id, &token).unwrap(); // B 认这个 token 是 A 的
    credential::set_peer(&b_id, &token).unwrap(); // A 手里有 B 的 token
    state_a.add_peer_book(PeerDevice {
        id: b_id.clone(),
        name: "PC-B".into(),
        ip: lan_ip.to_string(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    orbdrop_lib::state::probe_peers_once(&state_a).await;

    let learned = state_a.get_peer(&c_id).expect("A 学到了 B 那一段的设备");
    assert!(learned.online, "担保人说它在线，就是在线");
    assert!(!learned.paired, "学来的设备跟本机没有任何关系");
    assert!(state_a.is_learned(&c_id));

    assert!(
        !state_a.config.lock().peers.iter().any(|p| p.id == c_id),
        "学来的条目是临时的：不许写进地址簿"
    );

    // 担保人不再报它了。本机**不会**去拨它 —— 那不是本机发现的关系，多拨一次就是多余
    // 的一跳；所以它不该被探活判成离线，而是继续按"担保人最近说过它在"活着。
    state_b.config.lock().peers.retain(|p| p.id != c_id);
    state_b.peers.lock().retain(|p| p.id != c_id);
    orbdrop_lib::state::probe_peers_once(&state_a).await;
    assert!(
        state_a.get_peer(&c_id).map(|p| p.online).unwrap_or(false),
        "学来的行不参与探活：担保人没再提它也仍是在线（只有超时才收走）"
    );

    // 担保人不再提到它（超时）→ 收走。把时间戳拨回去模拟"20 秒没人再担保"。
    state_a
        .learned
        .lock()
        .insert(c_id.clone(), Instant::now() - Duration::from_secs(600));
    state_a.sweep_learned();
    assert!(
        state_a.get_peer(&c_id).is_none(),
        "没人再担保的行必须从列表里消失"
    );
    assert!(!state_a.is_learned(&c_id));

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&a_id).ok();
    credential::remove_peer(&b_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// **长连接活着也要交换视野**：这台对端就是本机跨段的锚点，而链路只证明"本机跟它这一台
/// 的关系"，不证明"它那一段有哪些设备已经同步过"。原先探活一看到长连接就 `continue`，
/// 于是用户点过「连接」之后锚点再也不报表：学来的条目 20s（`LEARNED_TTL`）内被收走，
/// 而且再也不会回来（"连上之后就不见了"）。
///
/// 学来的行是临时的，所以这件事必须跟着 5s 这一拍**持续**做 —— 不是"配对那一次做一次"。
#[tokio::test]
async fn the_view_exchange_survives_a_live_link() {
    let Some(lan_ip) = local_lan_ip() else {
        eprintln!("跳过：本机没有局域网地址，模拟不出「来自另一台设备」的请求");
        return;
    };
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_link_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    let uniq = uuid::Uuid::new_v4().to_string();
    let tag = &uniq[..8];
    let b_id = format!("link-b-{tag}");
    let a_id = format!("link-a-{tag}");
    let c_id = format!("link-c-{tag}");

    // 锚点 B：它这一段里有一台 C。
    let state_b = make_state(4398, &b_id, "PC-B", save_dir.clone());
    state_b.add_peer_book(PeerDevice {
        id: c_id.clone(),
        name: "PC-C".into(),
        ip: lan_ip.to_string(),
        port: 4399,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 4398)
        .await
        .expect("B server");

    let state_a = make_state(4400, &a_id, "PC-A", save_dir.clone());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&a_id, &token).unwrap(); // B 认这个 token 是 A 的
    credential::set_peer(&b_id, &token).unwrap(); // A 手里有 B 的 token
    state_a.add_peer_book(PeerDevice {
        id: b_id.clone(),
        name: "PC-B".into(),
        ip: lan_ip.to_string(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    // 造一条"活着"的长连接：探活那一趟必须仍然去要表。
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    state_a.ws.register(&b_id, tx);
    assert!(state_a.ws.has_connection(&b_id), "前提：链路活着");

    orbdrop_lib::state::probe_peers_once(&state_a).await;

    assert!(
        state_a.is_learned(&c_id),
        "链路活着不等于视野同步过：这一拍仍要把对端的表学过来"
    );
    assert!(state_a.get_peer(&c_id).is_some_and(|p| p.online));

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&a_id).ok();
    credential::remove_peer(&b_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

// ---------------------------------------------------------------------------
// 证书指纹（TLS 信任）：重新配对必须能换掉一条过时的钉
// ---------------------------------------------------------------------------

/// 用户点「配对 / 重新配对」= 明确重建关系：这个地址上钉着的旧指纹先让位
/// （内存表 + 凭据库都让），否则拨号会在 TLS 那一跳被"指纹不一致"直接拒掉 ——
/// 表现就是"点了重新配对还是连不上，重新配好也连不上"。
#[test]
fn forget_stale_pin_clears_both_the_memory_and_the_vault_copy() {
    credential::remove_peer("device-pin-a").ok();
    credential::remove_peer("device-pin-b").ok();
    let dir = std::env::temp_dir().join(format!("orbdrop_pin_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = make_state(3997, "device-pin-a", "PC-PIN-A", dir.clone());
    state.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-pin-b".into(), name: "PC-PIN-B".into(), ip: "10.9.9.9".into(), port: 3890,
        online: true, paired: true, auto_connect: false, paused: false, relay: None,
    });

    // 上一次配对留下的指纹（对端重装过 = 这枚已经过时）。
    credential::set_peer_credential("device-pin-b", "stale-token", &"ab".repeat(32)).unwrap();
    orbdrop_lib::state::refresh_trust(&state);
    let ip: std::net::IpAddr = "10.9.9.9".parse().unwrap();
    assert!(orbdrop_lib::net::trust().get(ip).is_some(), "前提：这个地址上钉着旧指纹");

    orbdrop_lib::pair::forget_stale_pin(&state, "device-pin-b");

    assert!(orbdrop_lib::net::trust().get(ip).is_none(), "内存那份让位了");
    assert_eq!(
        credential::get_peer_fingerprint("device-pin-b").unwrap(),
        None,
        "凭据库那份也清了（否则下一次 refresh_trust 又把它灌回来）"
    );
    credential::remove_peer("device-pin-b").ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 配对落定（`commit_pairing`）时**不再保留**那条过时指纹：这一跳真见过对端证书就钉这枚，
/// 没见过（邀请那条路本机是收方）就留空 —— 留着旧的正是"刚重新配好却依旧连不上"的成因。
#[test]
fn commit_pairing_replaces_a_stale_fingerprint_instead_of_keeping_it() {
    credential::remove_peer("device-cp-a").ok();
    credential::remove_peer("device-cp-b").ok();
    let dir = std::env::temp_dir().join(format!("orbdrop_cp_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = make_state(3998, "device-cp-a", "PC-CP-A", dir.clone());

    credential::set_peer_credential("device-cp-b", "stale-token", &"cd".repeat(32)).unwrap();
    orbdrop_lib::pair::commit_pairing(
        &state,
        orbdrop_lib::pair::PairedPeer {
            id: "device-cp-b".into(),
            name: "PC-CP-B".into(),
            ip: "10.9.9.10".into(),
            port: 3890,
        },
        "fresh-token",
        None,
    )
    .expect("配对落定");

    // 这一跳没走过 TLS（测试里没有真连），所以写下的应当是"空"（= 这个地址暂时没有钉住的
    // 指纹，交给下一次成功连线 TOFU 重新认），**绝不是**那条过时的。
    assert_eq!(
        credential::get_peer_fingerprint("device-cp-b").unwrap(),
        None,
        "过时的指纹必须让位"
    );
    assert_eq!(credential::get_peer_token("device-cp-b").unwrap().as_deref(), Some("fresh-token"));
    credential::remove_peer("device-cp-b").ok();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 会话（聊天）：文本消息与已读回执走的就是那条长连接
// ---------------------------------------------------------------------------

/// 一台设备的两个"家"：config.toml 与会话记录（chat.json）都落在它自己的目录里。
///
/// 会话记录跟 config.toml 走（见 `AppState::new`），所以两台设备必须各占一个目录 ——
/// 共用一个的话，两边会把同一份 chat.json 互相覆盖（真实部署里这就是两台机器）。
fn chat_state(
    port: u16,
    device_id: &str,
    device_name: &str,
    root: &std::path::Path,
) -> (Arc<AppState>, std::path::PathBuf) {
    let dir = root.join(device_id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    config.download_dir = dir.clone();
    config.bc_exe_path = Some(hermetic_bc_exe());
    let config_path = dir.join("config.toml");
    (Arc::new(AppState::new(config, config_path)), dir)
}

#[tokio::test]
async fn chat_messages_and_read_receipts_travel_over_the_long_connection() {
    use orbdrop_lib::chat::{self, Delivery};

    let root = std::env::temp_dir().join(format!("orbdrop_it_chat_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (port_a, port_b) = two_free_ports();
    let (state_a, dir_a) = chat_state(port_a, "device-chat-a", "PC-CA", &root);
    let (state_b, dir_b) = chat_state(port_b, "device-chat-b", "PC-CB", &root);

    let server_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let server_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");

    // ---- 配对结果（两侧各存一枚 token）+ 用户点「连接」 ----
    credential::remove_peer("device-chat-a").ok();
    credential::remove_peer("device-chat-b").ok();
    let token = format!("t-chat-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    credential::set_peer("device-chat-a", &token).unwrap();
    credential::set_peer("device-chat-b", &token).unwrap();
    state_a.mark_paired(&PeerDevice {
        id: "device-chat-b".into(), name: "PC-CB".into(), ip: "127.0.0.1".into(), port: port_b,
        online: true, paired: true, auto_connect: false, paused: false, relay: None,
    });
    state_b.mark_paired(&PeerDevice {
        id: "device-chat-a".into(), name: "PC-CA".into(), ip: "127.0.0.1".into(), port: port_a,
        online: true, paired: true, auto_connect: false, paused: false, relay: None,
    });
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-chat-b");
    wait_until("长连接建立", || {
        state_a.ws.has_connection("device-chat-b") && state_b.ws.has_connection("device-chat-a")
    })
    .await;

    // ---- A 发一条文本 ----
    let text = "在吗？帮我传一下「Q3 排期表」。";
    let res = chat::send_text(&state_a, &["device-chat-b".to_string()], text, None);
    assert!(!res.id.is_empty(), "发出去的消息带回它的 id（界面据此逐台读状态）");
    assert_eq!(res.batch_id, None, "一对一发的：不挂群发名单");
    let snap = res.snapshot;
    let convo = snap.convos.iter().find(|c| c.peer_id == "device-chat-b").expect("A 侧有一条会话");
    assert_eq!(convo.messages.len(), 1);
    let sent = &convo.messages[0];
    assert!(sent.from_me);
    assert_eq!(sent.text, text);
    assert_eq!(sent.targets, vec!["device-chat-b".to_string()]);
    assert_eq!(sent.delivery, Delivery::Sent, "写进长连接就算送达");
    assert_eq!(snap.unread, 0, "自己发的不算未读");

    // ---- B 收到（未读 +1），发件人名字取自**B 自己的设备表** ----
    wait_until("B 收到那条消息", || state_b.chat.inbound_watermark("device-chat-a") > 0).await;
    let snap_b = state_b.chat.snapshot();
    assert_eq!(snap_b.unread, 1);
    let convo_b = snap_b.convos.iter().find(|c| c.peer_id == "device-chat-a").expect("B 侧有一条会话");
    let got = &convo_b.messages[0];
    assert!(!got.from_me);
    assert_eq!(got.text, text);
    assert_eq!(got.from_name, "PC-CA", "发件人名字以本机设备表为准");
    assert_eq!(got.delivery, Delivery::Received);

    // ---- B 打开会话 → 本地未读清零 + 回执回去 → A 那条变「已读」 ----
    chat::mark_read(&state_b, &["device-chat-a".to_string()]);
    assert_eq!(state_b.chat.snapshot().unread, 0);
    wait_until("A 看到「已读」", || {
        state_a
            .chat
            .snapshot()
            .convos
            .iter()
            .find(|c| c.peer_id == "device-chat-b")
            .and_then(|c| c.messages.first())
            .is_some_and(|m| m.delivery == Delivery::Read)
    })
    .await;

    // ---- 长连接断了：**消息照样到**（HTTP 兜底那一跳；消息不该以"点过连接"为前提） ----
    state_b.ws.pause("device-chat-a");
    wait_until("B 侧链路断开", || !state_a.ws.has_connection("device-chat-b")).await;
    let before = state_b.chat.inbound_watermark("device-chat-a");
    let snap = chat::send_text(&state_a, &["device-chat-b".to_string()], "没有长连接也要到", None).snapshot;
    let convo = snap.convos.iter().find(|c| c.peer_id == "device-chat-b").unwrap();
    assert_eq!(
        convo.messages.last().unwrap().delivery,
        Delivery::Sending,
        "落账那一刻是「在途」：HTTP 那一跳还没回来"
    );
    wait_until("HTTP 兜底把这条送到（状态落定）", || {
        state_a
            .chat
            .snapshot()
            .convos
            .iter()
            .find(|c| c.peer_id == "device-chat-b")
            .and_then(|c| c.messages.last())
            .is_some_and(|m| m.delivery == Delivery::Sent)
    })
    .await;
    wait_until("B 真的收到了这条（HTTP 直达，没长连接）", || {
        state_b.chat.inbound_watermark("device-chat-a") > before
    })
    .await;

    // B 打开会话 → 已读回执也走 HTTP 兜底回来（没有长连接，"已读"照样得能传到）。
    let before_read = state_b.chat.read_watermark("device-chat-a");
    chat::mark_read(&state_b, &["device-chat-a".to_string()]);
    assert!(
        state_b.chat.read_watermark("device-chat-a") > before_read,
        "B 自己的水位先推进"
    );
    wait_until("A 收到 HTTP 回来的已读回执", || {
        state_a
            .chat
            .snapshot()
            .convos
            .iter()
            .find(|c| c.peer_id == "device-chat-b")
            .and_then(|c| c.messages.last())
            .is_some_and(|m| m.delivery == Delivery::Read)
    })
    .await;

    // ---- 群发：每台各一份（同一个 id），**没有群实体** —— 世界里只多出"与每台的两条会话" ----
    // 一台有链路、一台（没配对过的 id）根本没有会话：逐台成败各算各的。
    let fan = chat::send_text(
        &state_a,
        &["device-chat-b".to_string(), "device-chat-ghost".to_string()],
        "群发一句",
        None,
    );
    let fan_id = fan.id.clone();
    let batch_id = fan.batch_id.clone().expect("多台发送要挂上一份群发名单");
    let snap = fan.snapshot;
    assert_eq!(snap.batches.len(), 1, "这次群发产生了一份名单");
    assert_eq!(snap.batches[0].id, batch_id);
    assert_eq!(snap.batches[0].members.len(), 2);
    assert_eq!(snap.batches[0].name, "", "名字留空，等用户在列表里右键改名");
    let fan_b = snap.convos.iter().find(|c| c.peer_id == "device-chat-b").unwrap();
    let fan_ghost = snap.convos.iter().find(|c| c.peer_id == "device-chat-ghost").expect("另一台也有自己那条");
    assert_eq!(fan_b.messages.last().unwrap().id, fan_id);
    assert_eq!(fan_ghost.messages.last().unwrap().id, fan_id, "同一个 id：逐台状态靠它拼");
    assert_eq!(snap.convos.len(), 2, "群发不产生第三条「群会话」，就是两台各一条");
    // 逐台各算各的：B 走 HTTP 兜底能到；那个 id **地址簿里没有**（没有 ip/端口可打）→ 够不着，
    // 落「排队」等它上线（它哪天被发现、有了地址就会被补发出去，见 F-95）。
    wait_until("没有地址的那台排队等它上线", || {
        state_a
            .chat
            .snapshot()
            .convos
            .iter()
            .find(|c| c.peer_id == "device-chat-ghost")
            .and_then(|c| c.messages.last())
            .is_some_and(|m| m.delivery == Delivery::Queued)
    })
    .await;
    wait_until("有地址的那台（HTTP 兜底）落「已送达」", || {
        state_a
            .chat
            .snapshot()
            .convos
            .iter()
            .find(|c| c.peer_id == "device-chat-b")
            .and_then(|c| c.messages.last())
            .is_some_and(|m| m.delivery == Delivery::Sent)
    })
    .await;

    // ---- 两侧都落了盘：重开一份 store 还能读到（会话记录是用户数据） ----
    let reopened_a = chat::ChatStore::with_path(dir_a.join("chat.json"));
    assert_eq!(
        reopened_a.snapshot().convos[0].messages.len(),
        3,
        "第一句、HTTP 兜底那句、群发那句 —— 各一份"
    );
    let reopened_b = chat::ChatStore::with_path(dir_b.join("chat.json"));
    let kept = &reopened_b.snapshot().convos[0].messages[0];
    assert_eq!(kept.text, text);
    assert_eq!(
        reopened_b.read_watermark("device-chat-a"),
        state_b.chat.read_watermark("device-chat-a"),
        "已读水位也落盘"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-chat-a").ok();
    credential::remove_peer("device-chat-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = (server_a, server_b);   // 端口号不是句柄：服务器由 `stop_server` 收
}

/// 对端离线时发出去的消息**不丢**：落「等待发送」排着队，对端一上线就按原顺序自动发出去
/// （F-95）。顺带钉住三件事：
///
/// - **顺序**：欠着消息的时候新消息跟着排（不许后发先至）；
/// - **时刻**：对端显示的是**发送时刻**（发送方的钟），不是它收到的时刻 —— 顺序与已读水位
///   都按它比（这一条决定了两端看到的时刻一致、会话位置也一致）；
/// - **幂等**：同一个 id 再投一次不会在对端写成两条（补发是"至少一次"）。
///
/// "离线"是用**没人监听的地址**造的（见测试里的说明）：与"对端不在网上"在代码里是同一条路
/// —— 连不上 → 够不着 → 排队；而 `stop_server` 造不出这一段（keep-alive 连接还在被服务）。
#[tokio::test]
async fn messages_sent_while_the_peer_is_offline_arrive_when_it_comes_back() {
    use orbdrop_lib::chat::{self, Delivery};
    use orbdrop_lib::ws::events::WsEvent;

    let root = std::env::temp_dir().join(format!("orbdrop_it_chat_off_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let (port_a, port_b) = two_free_ports();
    let (state_a, _dir_a) = chat_state(port_a, "device-off-a", "PC-OFF-A", &root);
    let (state_b, _dir_b) = chat_state(port_b, "device-off-b", "PC-OFF-B", &root);

    let server_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let server_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");

    credential::remove_peer("device-off-a").ok();
    credential::remove_peer("device-off-b").ok();
    let token = format!("t-off-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    credential::set_peer("device-off-a", &token).unwrap();
    credential::set_peer("device-off-b", &token).unwrap();
    let addr = |id: &str, name: &str, ip: &str, port: u16| PeerDevice {
        id: id.into(), name: name.into(), ip: ip.into(), port,
        online: true, paired: true, auto_connect: false, paused: false, relay: None,
    };
    state_a.mark_paired(&addr("device-off-b", "PC-OFF-B", "127.0.0.1", port_b));
    state_b.mark_paired(&addr("device-off-a", "PC-OFF-A", "127.0.0.1", port_a));

    // 先正常说一句：这一步把对端证书按 TOFU 认下来（之后 B 换了进程也还是同一份身份文件）。
    chat::send_text(&state_a, &["device-off-b".to_string()], "在线时说的", None);
    wait_until("在线时那句到了", || state_b.chat.inbound_watermark("device-off-a") > 0).await;
    let convo = |state: &Arc<AppState>, peer: &str| {
        state.chat.snapshot().convos.into_iter().find(|c| c.peer_id == peer)
    };
    assert_eq!(convo(&state_a, "device-off-b").unwrap().messages[0].delivery, Delivery::Sent);

    // ---- B 离线：把地址指到一个**没人监听**的端口（连接被拒 = 够不着） ----
    //
    // 这里不用 `stop_server` 模拟"对端关机"：监听停了，但已经建立的 keep-alive 连接
    // 仍由它那条连接任务服务着，下一跳照样会"成功"（测出来的是"消息直接送达"，不是排队）。
    // 地址指到死端口与"对端不在网上"在代码里是同一条路：连不上 → 够不着。
    const DEAD: u16 = 1; // 需要管理员权限才绑得上，测试进程不可能占用它
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", DEAD)).is_err(),
        "死地址上不该有人监听"
    );
    state_a.mark_paired(&addr("device-off-b", "PC-OFF-B", "127.0.0.1", DEAD));
    state_a.set_peer_online("device-off-b", false);

    // ---- 这个窗口里 A 说了三句 ----
    let first = chat::send_text(&state_a, &["device-off-b".to_string()], "你不在的时候说的", None);
    let first_at = convo(&state_a, "device-off-b").unwrap().messages.last().unwrap().at;
    chat::send_text(&state_a, &["device-off-b".to_string()], "还有这一句", None);
    wait_until("前两句都排上队（HTTP 那一跳没打通）", || {
        // 会话里第一条是"在线时说的"（已送达），后面两条才是这一个窗口里的。
        convo(&state_a, "device-off-b").is_some_and(|c| {
            c.messages.len() == 3 && c.messages[1..].iter().all(|m| m.delivery == Delivery::Queued)
        })
    })
    .await;
    // 欠着消息的时候新消息**跟着排**（不许后发先至：对端那边顺序乱了，已读水位就跟着错）。
    let third = chat::send_text(&state_a, &["device-off-b".to_string()], "第三句", None);
    let third_copy = third
        .snapshot
        .convos
        .iter()
        .find(|c| c.peer_id == "device-off-b")
        .and_then(|c| c.messages.last())
        .expect("第三句也有一份副本");
    assert_eq!(third_copy.delivery, Delivery::Queued, "欠着消息时新消息跟着排队");
    assert!(third_copy.at > first_at, "时刻仍是发送那一刻（递增）");

    // ---- B 上线：地址指回去 + 一次"重新出现在网上"的证据 ----
    let before_back = chat::now_ms();
    state_a.mark_paired(&addr("device-off-b", "PC-OFF-B", "127.0.0.1", port_b));
    // 真实部署里这个证据来自探活/mDNS（`set_peer_online` 是它们的汇合点），补发的触发点
    // 就挂在它上面。
    state_a.set_peer_online("device-off-b", true);

    wait_until("B 收到补发的三句", || {
        convo(&state_b, "device-off-a").is_some_and(|c| c.messages.len() == 4)
    })
    .await;
    wait_until("A 侧三句都落「已送达」", || {
        convo(&state_a, "device-off-b").is_some_and(|c| c.messages.iter().all(|m| m.delivery == Delivery::Sent))
    })
    .await;

    let on_b = convo(&state_b, "device-off-a").unwrap();
    let texts: Vec<&str> = on_b.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["在线时说的", "你不在的时候说的", "还有这一句", "第三句"],
        "补发的按原顺序接在后面"
    );
    let resent = &on_b.messages[1];
    assert_eq!(resent.id, first.id, "补发的就是当初那一条（id 不变）");
    // **发送时刻**，不是收到的时刻：B 是三句都发完之后才开机的（`before_back` 之后）。
    assert_eq!(resent.at, first_at, "对端看到的时刻 = A 发出去那一刻");
    assert!(
        on_b.messages.windows(2).all(|w| w[0].at < w[1].at),
        "四条的时刻跟着会话顺序递增"
    );
    assert!(
        on_b.messages[1].at < before_back,
        "补发进来的时刻早于对端开机那一刻（不是它收到的时刻）"
    );
    assert_eq!(on_b.unread, 4, "四条都没打开过 —— 补发进来的照样是未读（收到 ≠ 读过）");

    // ---- 重复投递由接收方按 id 幂等吸收（补发是"至少一次"：HTTP 到了、回话丢了） ----
    let dup = WsEvent::ChatText { id: first.id.clone(), text: "你不在的时候说的".into(), at: first_at };
    assert_eq!(chat::deliver(&state_a, "device-off-b", &dup).await, chat::Outcome::Delivered, "这一跳是通的");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        convo(&state_b, "device-off-a").unwrap().messages.len(),
        4,
        "同一个 id 再投一次不会写成两条"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-off-a").ok();
    credential::remove_peer("device-off-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = (server_a, server_b);
}

// ---------------------------------------------------------------------------
// 群聊（共享群）：名册 + 群消息，成员**不需要配对**（D6=b）
// ---------------------------------------------------------------------------

/// 三台设备互不认识（各占一个数据目录），但在地址簿里互相有地址 —— 这就是"局域网里看得见"
/// 的最小形态。**刻意不配对**：群成员不需要配对（D6=b），这一条必须被真实覆盖。
fn address_book(state: &Arc<AppState>, id: &str, name: &str, port: u16) {
    state.upsert_peer(&PeerDevice {
        id: id.into(),
        name: name.into(),
        ip: "127.0.0.1".into(),
        port,
        online: true,
        paired: false,
        auto_connect: false,
        paused: false, relay: None,
    });
}

/// 群聊的主线：A 建群 → B/C 收到的是**邀请卡**（要人确认）→ 加入 → A 发一句三端都能看到 →
/// B 回一句 A/C 也能看到（成员互见，这是"群"与"群发"的分水岭）。
#[tokio::test]
async fn group_roster_confirmation_and_messages_reach_every_member() {
    use orbdrop_lib::group;

    let root = std::env::temp_dir().join(format!("orbdrop_it_group_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (port_a, port_b) = two_free_ports();
    let port_c = free_port();
    let (state_a, _) = chat_state(port_a, "device-grp-a", "PC-GA", &root);
    let (state_b, _) = chat_state(port_b, "device-grp-b", "PC-GB", &root);
    let (state_c, _) = chat_state(port_c, "device-grp-c", "PC-GC", &root);
    let server_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let server_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    let server_c = orbdrop_lib::server::start_server(state_c.clone(), port_c).await.expect("C server");

    address_book(&state_a, "device-grp-b", "PC-GB", port_b);
    address_book(&state_a, "device-grp-c", "PC-GC", port_c);
    address_book(&state_b, "device-grp-a", "PC-GA", port_a);
    address_book(&state_b, "device-grp-c", "PC-GC", port_c);
    address_book(&state_c, "device-grp-a", "PC-GA", port_a);
    address_book(&state_c, "device-grp-b", "PC-GB", port_b);

    // ---- A 建群：名册当场广播出去 ----
    let gid = group::create(&state_a, &["device-grp-b".into(), "device-grp-c".into()], "排期组")
        .expect("至少要有一台设备");
    assert!(state_a.groups.is_member(&gid, "device-grp-a"), "本机在建的群里");
    assert_eq!(state_a.groups.members(&gid).len(), 3);

    // ---- B、C 收到的是**邀请卡**：没点「加入」之前不算群 ----
    wait_until("B 收到邀请卡", || !state_b.groups.pending_views("device-grp-b").is_empty()).await;
    wait_until("C 收到邀请卡", || !state_c.groups.pending_views("device-grp-c").is_empty()).await;
    let invite = &state_b.groups.pending_views("device-grp-b")[0];
    assert_eq!(invite.group_id, gid);
    assert_eq!(invite.name, "排期组");
    assert_eq!(invite.from, "device-grp-a");
    assert_eq!(
        invite.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        vec!["device-grp-a".to_string(), "device-grp-c".to_string()],
        "邀请卡里列出的是别人（不含本机）"
    );
    assert!(state_b.groups.view(&gid, "device-grp-b").is_none(), "没确认之前不进会话列表");

    // 摆出界面那份快照：邀请在 `pendingGroups` 里，还没进 `groups`。
    let snap_b = group::snapshot(&state_b);
    assert_eq!(snap_b.pending_groups.len(), 1);
    assert!(snap_b.groups.is_empty());

    // ---- 点「加入」 ----
    assert!(group::accept(&state_b, &gid));
    assert!(group::accept(&state_c, &gid));
    assert_eq!(state_b.groups.view(&gid, "device-grp-b").unwrap().members.len(), 2, "视图里不含本机");
    assert!(group::snapshot(&state_b).pending_groups.is_empty(), "卡收走了");

    // ---- A 发一句：B、C 都要收到（未配对 → 只能走 HTTP 兜底那一跳） ----
    let text = "周三十点开个短会";
    let msg_id = group::send_text(&state_a, &gid, text).expect("本机在群里，发得出去");
    wait_until("B 收到群消息", || {
        state_b.groups.view(&gid, "device-grp-b").is_some_and(|v| v.messages.len() == 1)
    })
    .await;
    wait_until("C 收到群消息", || {
        state_c.groups.view(&gid, "device-grp-c").is_some_and(|v| v.messages.len() == 1)
    })
    .await;
    let seen = &state_b.groups.view(&gid, "device-grp-b").unwrap().messages[0];
    assert_eq!(seen.id, msg_id);
    assert_eq!(seen.text, text);
    assert_eq!(seen.from, "device-grp-a");
    assert_eq!(seen.from_name, "PC-GA", "发言人名字按**收方的设备表**填");
    assert!(!seen.from_me);
    assert_eq!(state_b.groups.unread_total("device-grp-b"), 1);
    assert!(state_a.groups.view(&gid, "device-grp-a").unwrap().messages[0].from_me, "自己的那条");

    // ---- B 回一句：**A 与 C 都该看到**（成员互见 —— 群发做不到这件事） ----
    group::send_text(&state_b, &gid, "收到，我把参会人拉齐").expect("B 发得出去");
    wait_until("A 看到 B 的那句", || {
        state_a.groups.view(&gid, "device-grp-a").is_some_and(|v| v.messages.len() == 2)
    })
    .await;
    wait_until("C 也看到 B 的那句", || {
        state_c.groups.view(&gid, "device-grp-c").is_some_and(|v| v.messages.len() == 2)
    })
    .await;
    let c_view = state_c.groups.view(&gid, "device-grp-c").unwrap();
    let speakers: Vec<&str> = c_view.messages.iter().map(|m| m.from.as_str()).collect();
    assert_eq!(speakers, vec!["device-grp-a", "device-grp-b"], "两句、两个发言人，按时间排");

    // ---- B 打开会话 → 已读回执逐发送者回去 → A 那条算「已读 1/2」 ----
    group::mark_read(&state_b, &gid);
    wait_until("A 看到 B 读了", || {
        state_a
            .groups
            .view(&gid, "device-grp-a")
            .is_some_and(|v| v.messages[0].read_count == 1)
    })
    .await;
    let a_view = state_a.groups.view(&gid, "device-grp-a").unwrap();
    assert_eq!(a_view.messages[0].read_count, 1, "B 读了、C 还没读");
    assert_eq!(a_view.messages[0].delivery.get("device-grp-b"), Some(&orbdrop_lib::chat::Delivery::Sent));

    // ---- 改群名：名册 +1 广播，另两端跟着改 ----
    assert!(group::edit(&state_a, &gid, &["device-grp-b".into(), "device-grp-c".into()], "排期组（周三）"));
    wait_until("B 那边群名也变了", || {
        state_b.groups.view(&gid, "device-grp-b").is_some_and(|v| v.name == "排期组（周三）")
    })
    .await;

    // ---- 退群：本地没了，另两端的名册里也少一个人 ----
    assert!(group::leave(&state_c, &gid));
    assert!(state_c.groups.view(&gid, "device-grp-c").is_none(), "C 这边删干净了");
    wait_until("A、B 的名册里 C 走了", || {
        state_a
            .groups
            .view(&gid, "device-grp-a")
            .is_some_and(|v| v.members.iter().all(|m| m.id != "device-grp-c"))
    })
    .await;

    // 群数据落在自己的文件里（与 `chat.json` 分开）。
    let reopened = orbdrop_lib::group::GroupStore::with_path(root.join("device-grp-a").join("groups.json"));
    assert_eq!(reopened.views("device-grp-a").len(), 1, "重启后群还在");
    assert_eq!(reopened.views("device-grp-a")[0].messages.len(), 2, "消息也还在");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    orbdrop_lib::server::stop_server(&state_c).await;
    let _ = std::fs::remove_dir_all(&root);
    let _ = (server_a, server_b, server_c);
}

/// 群消息的离线补发：对端**没开机**时那一份落「排队」，等它上线由补发那一跳按原顺序送出去。
///
/// 顺带覆盖"名册先于消息"：名册当初广播时 C 不在网上，所以补发那一步必须先补名册、
/// 再发消息（否则 C 不认识这个群，消息无处可放）。
#[tokio::test]
async fn group_messages_wait_for_an_offline_member_and_the_roster_goes_first() {
    use orbdrop_lib::chat::{self, Delivery};
    use orbdrop_lib::group;

    let root = std::env::temp_dir().join(format!("orbdrop_it_groupq_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (port_a, port_b) = two_free_ports();
    let (state_a, _) = chat_state(port_a, "device-gq-a", "PC-QA", &root);
    let (state_b, _) = chat_state(port_b, "device-gq-b", "PC-QB", &root);
    let server_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");

    address_book(&state_a, "device-gq-b", "PC-QB", port_b);

    let gid = group::create(&state_a, &["device-gq-b".into()], "临时群").expect("建群");
    // B 还没开机：名册投不出去（`told` 停在"没说过"）。
    assert!(state_a.groups.needs_roster(&gid, "device-gq-b"));
    let msg_id = group::send_text(&state_a, &gid, "你上线再看这句").expect("发得出去");
    let view = state_a.groups.view(&gid, "device-gq-a").unwrap();
    assert_eq!(
        view.messages[0].delivery.get("device-gq-b"),
        Some(&Delivery::Queued),
        "够不着 = 排队等它上线（不是「未送达」）"
    );
    assert_eq!(view.messages[0].id, msg_id);
    assert!(state_a.groups.has_backlog_for("device-gq-b"));

    // ---- B 上线（起服务器 + 判在线）→ 补发那一跳：先名册、再消息 ----
    let server_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    chat::flush_queued(&state_a, "device-gq-b");
    // 名册先到：B 那边先挂一张邀请卡（它还没点「加入」）。
    wait_until("B 收到邀请卡", || state_b.groups.has_pending(&gid)).await;
    assert!(state_b.groups.view(&gid, "device-gq-b").is_none(), "没确认前不算群");
    // 紧跟其后的那条消息**不能丢**：它还挂在卡上，先攒着。
    wait_until("那条消息被攒下来了", || state_b.groups.buffered_count(&gid) == 1).await;

    // 点「加入」→ 攒着的那条并进会话。
    assert!(group::accept(&state_b, &gid));
    let view_b = state_b.groups.view(&gid, "device-gq-b").expect("加入后有群了");
    assert_eq!(view_b.messages.len(), 1);
    assert_eq!(view_b.messages[0].text, "你上线再看这句");
    assert_eq!(view_b.unread, 1, "攒进来的照样是未读（收到 ≠ 读过）");
    assert!(!state_a.groups.needs_roster(&gid, "device-gq-b"), "名册补过了");
    wait_until("A 那一份落成已送达", || {
        state_a
            .groups
            .view(&gid, "device-gq-a")
            .is_some_and(|v| v.messages[0].delivery.get("device-gq-b") == Some(&Delivery::Sent))
    })
    .await;

    // ---- 再发一条：B 现在可达，直接就送到（不用再排队） ----
    group::send_text(&state_a, &gid, "这句是它上线之后发的").expect("发得出去");
    wait_until("B 收到第二条", || {
        state_b.groups.view(&gid, "device-gq-b").is_some_and(|v| v.messages.len() == 2)
    })
    .await;

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&root);
    let _ = (server_a, server_b);
}

/// 跨网段（链式：A—B—C，A 与 C 互相够不着）时，群消息靠**两条路**到得了对面：
/// ① 接收方主动**补拉**（向自己够得着的邻居要）；② 链路上的成员**中转**（`groupRelay` 开关）。
///
/// 拓扑用地址簿造出来：A 只认识 B、C 只认识 B —— 就是"两台机器各在一个网段、中间那台双网卡"
/// 的最小替身（`learned` 那一层的跨段发现在 `routes_discover` 里就写明"拿到表之后本机直连"，
/// 所以这里的"够不着"正是真实情形）。
#[tokio::test]
async fn a_chain_group_catches_up_by_pull_and_by_relay() {
    use orbdrop_lib::chat::Delivery;
    use orbdrop_lib::group;

    let root = std::env::temp_dir().join(format!("orbdrop_it_chain_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (port_a, port_b) = two_free_ports();
    let port_c = free_port();
    let (state_a, _) = chat_state(port_a, "device-ch-a", "PC-CHA", &root);
    let (state_b, _) = chat_state(port_b, "device-ch-b", "PC-CHB", &root);
    let (state_c, _) = chat_state(port_c, "device-ch-c", "PC-CHC", &root);
    let server_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let server_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    let server_c = orbdrop_lib::server::start_server(state_c.clone(), port_c).await.expect("C server");

    // A ↔ B、B ↔ C 有地址；A ↔ C **没有**（两个网段之间没有路径）。
    address_book(&state_a, "device-ch-b", "PC-CHB", port_b);
    address_book(&state_b, "device-ch-a", "PC-CHA", port_a);
    address_book(&state_b, "device-ch-c", "PC-CHC", port_c);
    address_book(&state_c, "device-ch-b", "PC-CHB", port_b);

    // 群由**中间那台**建：能同时看见两端的人建群，两端才都收得到名册（真事也是这样 ——
    // A 那边连 C 都列不出来，选不中它）。
    let gid = group::create(&state_b, &["device-ch-a".into(), "device-ch-c".into()], "跨段组").expect("建群");
    wait_until("A、C 都收到邀请卡", || {
        state_a.groups.has_pending(&gid) && state_c.groups.has_pending(&gid)
    })
    .await;
    assert!(group::accept(&state_a, &gid));
    assert!(group::accept(&state_c, &gid));

    // ---- ① 补拉：A 发一句（只能到 B），C 向 B 要来 ----
    group::send_text(&state_a, &gid, "排期表我放共享里了").expect("A 发得出去");
    wait_until("B 收到", || {
        state_b.groups.view(&gid, "device-ch-b").is_some_and(|v| v.messages.len() == 1)
    })
    .await;
    assert!(
        state_c.groups.view(&gid, "device-ch-c").is_some_and(|v| v.messages.is_empty()),
        "C 收不到：A 到不了 C（那一份在 A 那边排着队）"
    );
    assert_eq!(
        state_a.groups.view(&gid, "device-ch-a").unwrap().messages[0].delivery.get("device-ch-c"),
        Some(&Delivery::Queued),
        "A 对 C 的那一份：够不着 = 排队"
    );

    // C 打开群（或定时扫）→ 向够得着的 B 要一遍。
    group::sync_with(&state_c, &gid, "device-ch-b");
    wait_until("C 补到了 A 的那句", || {
        state_c
            .groups
            .view(&gid, "device-ch-c")
            .is_some_and(|v| v.messages.iter().any(|m| m.text == "排期表我放共享里了"))
    })
    .await;
    let seen = &state_c.groups.view(&gid, "device-ch-c").unwrap().messages[0];
    assert_eq!(seen.from, "device-ch-a", "作者是 A，不是搬运工 B");
    assert_eq!(seen.from_name, "PC-CHA");

    // ---- ② 中转：B 打开开关后，A 新发的那句由 B 转给 C（不必等补拉） ----
    state_b.config.lock().group_relay = true;
    group::send_text(&state_a, &gid, "十点开会").expect("A 发得出去");
    wait_until("C 从中转那条路收到", || {
        state_c
            .groups
            .view(&gid, "device-ch-c")
            .is_some_and(|v| v.messages.iter().any(|m| m.text == "十点开会"))
    })
    .await;
    let c_view = state_c.groups.view(&gid, "device-ch-c").unwrap();
    let c_msgs: Vec<(String, String, u64)> = c_view
        .messages
        .iter()
        .map(|m| (m.from.clone(), m.text.clone(), m.seq))
        .collect();
    let b_msgs: Vec<(String, String, u64)> = state_b
        .groups
        .view(&gid, "device-ch-b")
        .unwrap()
        .messages
        .iter()
        .map(|m| (m.from.clone(), m.text.clone(), m.seq))
        .collect();
    assert_eq!(c_view.messages.len(), 2, "两句都在，没有重复：C={c_msgs:?} B={b_msgs:?}");
    assert!(
        c_view.messages.iter().all(|m| m.from == "device-ch-a"),
        "两句都是 A 说的：C={c_msgs:?} B={b_msgs:?}"
    );
    // A 那边也不会因为中转多出副本（作者始终是 A；中转不产生新消息）。
    let a_view = state_a.groups.view(&gid, "device-ch-a").unwrap();
    assert_eq!(a_view.messages.len(), 2, "A 只看到自己发的那两条");
    assert!(
        a_view.messages.iter().all(|m| m.from_me),
        "中转不会把消息转回给作者"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    orbdrop_lib::server::stop_server(&state_c).await;
    let _ = std::fs::remove_dir_all(&root);
    let _ = (server_a, server_b, server_c);
}

/// 中转（F-106）：已配对对端请本机替它连一台设备（AP 把客户端之间的单播隔开时，这是
/// 唯一还走得通的那条路）。这一条同时钉住三件事：
///
/// 1. **规则**：开关默认关 → 403；没凭据 → 401；目标不认识 → 404；请求方把自己当目标 → 404；
/// 2. **端点真的搬字节**：开出来的端口后面是一条活的字节通道（"目标"是一台真的回声服务器
///    —— 中转方只搬字节，它看不懂协议，也不该懂）；
/// 3. **地址字面量**：回话里的 host 是 IPv4 映射的 IPv6，与直连键位不同 —— 指纹表靠这一点
///    把"中转端点上的那台设备"与"中转方自己的服务"分成两个键位（见 `net::relay_host`）。
#[tokio::test]
async fn relay_open_obeys_the_rules_and_blind_forwards_bytes() {
    isolate_data_dir();
    let Some(lan_ip) = local_lan_ip() else {
        return; // 没有局域网地址的机器（纯离线 CI）跳过：这条路上"拨谁"本身就没有答案
    };

    let root = std::env::temp_dir().join(format!("orbdrop_it_relay_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let relay_id = format!("relay-{}", uuid::Uuid::new_v4());
    let caller_id = format!("relay-caller-{}", uuid::Uuid::new_v4());
    let target_id = format!("relay-target-{}", uuid::Uuid::new_v4());
    let state = make_state(4490, &relay_id, "RELAY", root.clone());

    // "目标"是一台真的回声服务器。
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("回声服务器");
    let target_port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tokio::io::AsyncWriteExt::write_all(&mut sock, &buf[..n])
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    // 目标**在线**（那是中转的前提）；呼叫方已配对（控制面要凭据）。
    state.add_peer_book(PeerDevice {
        id: target_id.clone(),
        name: "TARGET".into(),
        ip: "127.0.0.1".into(),
        port: target_port,
        online: true,
        paired: false,
        auto_connect: false, paused: false, relay: None,
    });
    state.add_peer_book(PeerDevice {
        id: caller_id.clone(),
        name: "CALLER".into(),
        ip: lan_ip.to_string(),
        port: 4491,
        online: true,
        paired: true,
        auto_connect: false, paused: false, relay: None,
    });

    let port = orbdrop_lib::server::start_server(state.clone(), 4490)
        .await
        .expect("relay server");
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&caller_id, &token).unwrap();

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan_ip}:{port}/relay/open");
    let ask = |target: &str| {
        client
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "target": target }))
    };

    // 1) 开关默认关：本机不替任何人搬字节。
    assert_eq!(
        ask(&target_id).send().await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN,
        "「允许为其它设备中转」默认关"
    );
    // 2) 没凭据的连接在 auth 中间件就被拦（连门都进不去）。
    assert_eq!(
        client
            .post(&url)
            .json(&serde_json::json!({ "target": target_id }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "中转只对已配对对端开放"
    );

    state.config.lock().allow_relay = true;

    // 3) 不认识的目标 / 请求方把自己当目标：都不合格 —— 而且**不该吃掉限流额度**
    //    （限流是最后一道闸，见 `relay::open`）。
    assert_eq!(
        ask("who-is-this").send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(
        ask(&caller_id).send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // 4) 正常一遍：开出来的端点必须真的把字节搬到目标、再原样搬回来。
    let opened: serde_json::Value = ask(&target_id).send().await.unwrap().json().await.unwrap();
    let host = opened["host"].as_str().expect("host").to_string();
    let relay_port = u16::try_from(opened["port"].as_u64().expect("port")).expect("端口范围");
    assert_eq!(opened["target"], target_id.as_str());
    assert!(
        host.starts_with("[::ffff:"),
        "中转端点必须用 IPv4 映射的 IPv6 字面量（指纹表独立键位）: {host}"
    );
    let relay_ip = orbdrop_lib::net::parse_host(&host).expect("端点主机可解析");
    assert_ne!(
        orbdrop_lib::net::parse_host(&lan_ip.to_string()).unwrap(),
        relay_ip,
        "中转键位与直连键位必须不同（否则同地址两枚指纹 → 不钉 → TOFU）"
    );

    // 拨这个端点用**它映射到的那个 IPv4**：同一个监听、同一个端口，字面量只影响指纹表的
    // 键位与客户端的拨号写法。这样写这条断言在任何平台都成立 —— Windows 的 socket 默认
    // v6-only，把映射地址 connect 出去需要 `IPV6_V6ONLY=0`（reqwest 自己建的 socket 不带
    // 这个标志，见 `relay` 模块头的已知限制）。
    let dial_ip = match relay_ip {
        std::net::IpAddr::V6(v6) => std::net::IpAddr::V4(v6.to_ipv4_mapped().expect("映射地址")),
        std::net::IpAddr::V4(v4) => std::net::IpAddr::V4(v4),
    };
    let mut sock = tokio::net::TcpStream::connect(std::net::SocketAddr::new(dial_ip, relay_port))
        .await
        .expect("连中转端点");
    tokio::io::AsyncWriteExt::write_all(&mut sock, b"through-the-relay")
        .await
        .expect("写入");
    let mut buf = vec![0u8; 32];
    let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await.expect("读回");
    assert_eq!(
        &buf[..n],
        b"through-the-relay",
        "端点必须原样搬字节（不解析、不改写）"
    );

    // 关掉开关（设置里的 S-09）→ **立刻**收掉已经开出去的端点，不是等它空闲回收。
    orbdrop_lib::relay::close_all(&state);
    assert_eq!(state.relays.active(), 0, "关掉开关必须当场把端点收掉");
    let mut closed = false;
    for _ in 0..40 {
        match tokio::net::TcpStream::connect(std::net::SocketAddr::new(dial_ip, relay_port)).await {
            // 监听随监听任务一起 drop，可能晚一拍：给几次机会。
            Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "关掉开关之后端点不该再接受连接");

    orbdrop_lib::server::stop_server(&state).await;
    echo_task.abort();
    credential::remove_peer(&caller_id).ok();
    let _ = std::fs::remove_dir_all(&root);
}
