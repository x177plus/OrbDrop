//! 对端元数据这条共用面（`peer_fs`）：`/shares` 的 isDir、`/stat` 优先、老版本退回列父目录。
//!
//! 拖放（`drag`）与镜像（`mirror`）都吃这一层，所以它单独盯着测 —— 尤其是**老版本对端**
//! 那条路：用户的另一台机器可能还跑着旧版本，那时 `/stat` 不存在，退回"列父目录"能不能
//! 拿到大小/时间/是否目录，直接决定拖出去的东西是不是完整的。
//!
//! 走真 HTTP：本机起一个实例、地址簿里把自己当成对端；到对端那一跳是 TLS，所以那个
//! "老版本对端"桩也必须说 TLS（用应用自己那份身份起服务端）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use orbdrop_lib::config::{Config, PeerDevice, ShareItem};
use orbdrop_lib::credential;
use orbdrop_lib::peer_fs;
use orbdrop_lib::state::AppState;

fn make_state(port: u16) -> Arc<AppState> {
    let mut config = Config::default();
    config.port = port;
    config.device_id = "device-metadata-self".into();
    config.device_name = "元数据测试".into();
    config.bc_exe_path = None;
    let config_path = std::env::temp_dir().join(format!("orbdrop_meta_cfg_{}.toml", uuid::Uuid::new_v4()));
    Arc::new(AppState::new(config, config_path))
}

/// 搭一个只有共享项、没有内容的实例，并把地址簿里那一行指向 `peer_port`。
async fn state_with_share(port: u16, root: &std::path::Path, peer_port: u16) -> (Arc<AppState>, String, String, std::path::PathBuf) {
    let state = make_state(port);
    let share = ShareItem::new(root.to_path_buf());
    let share_id = share.id.clone();
    state.config.lock().shares.push(share);
    let peer = format!("device-meta-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: peer.clone(),
        name: "对端".into(),
        ip: "127.0.0.1".into(),
        port: peer_port,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false, relay: None,
    });
    credential::set_peer(&peer, "t").unwrap();
    let cfg = state.config_path.clone();
    (state, share_id, peer, cfg)
}

/// **老版本对端**：只有 `/tree`，问 `/stat` 一律 404。
struct OldPeer {
    port: u16,
}

impl OldPeer {
    fn start(root: std::path::PathBuf, port: u16) -> Self {
        let identity = orbdrop_lib::net::load_or_create_identity().expect("测试身份");
        let tls = orbdrop_lib::net::server_config(&identity);
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("绑端口");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let Ok(conn) = rustls::ServerConnection::new(tls.clone()) else {
                    continue;
                };
                let mut stream = rustls::StreamOwned::new(conn, stream);
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path_line = request.lines().next().unwrap_or_default().to_string();
                let query = path_line.split(' ').nth(1).unwrap_or_default().to_string();
                let body = if path_line.starts_with("GET /tree") {
                    let rel = query
                        .split(['?', '&'])
                        .find_map(|kv| kv.strip_prefix("path="))
                        .unwrap_or("");
                    let dir = if rel.is_empty() { root.clone() } else { root.join(rel) };
                    let mut entries = Vec::new();
                    if let Ok(read) = std::fs::read_dir(&dir) {
                        for entry in read.flatten() {
                            let meta = entry.metadata().expect("meta");
                            entries.push(format!(
                                "{{\"name\":\"{}\",\"isDir\":{},\"size\":{},\"modified\":\"2026-09-16T12:00:00+00:00\"}}",
                                entry.file_name().to_string_lossy(),
                                meta.is_dir(),
                                if meta.is_dir() { 0 } else { meta.len() }
                            ));
                        }
                    }
                    format!("{{\"entries\":[{}]}}", entries.join(","))
                } else {
                    // 老版本没有 /stat：路由不认这个路径，就是 404
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        // 等它能把端口接起来（后面那次裸 connect 会让桩在握手上失败一次，无所谓）
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { port }
    }
}

#[tokio::test]
async fn shares_reports_is_dir_from_real_metadata() {
    let root = std::env::temp_dir().join(format!("orbdrop_isdir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README"), b"no extension").unwrap(); // 文件：没有点
    std::fs::create_dir_all(root.join("带点.的目录")).unwrap(); // 目录：名字里有"扩展名"

    let (state, _share_id, peer, cfg) = state_with_share(3993, &root, 3993).await;
    let file_share = ShareItem::new(root.join("README"));
    let dir_share = ShareItem::new(root.join("带点.的目录"));
    let (file_id, dir_id) = (file_share.id.clone(), dir_share.id.clone());
    state.config.lock().shares.push(file_share);
    state.config.lock().shares.push(dir_share);
    let port = orbdrop_lib::server::start_server(state.clone(), 3993).await.expect("起服务");
    credential::set_peer(&peer, "t").unwrap();

    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/shares"))
        .bearer_auth("t")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let find = |id: &str| {
        body["shares"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()["isDir"]
            .as_bool()
            .unwrap()
    };
    assert!(!find(&file_id), "没有扩展名的文件不是目录");
    assert!(find(&dir_id), "名字里有点的目录仍是目录");

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}

/// 真去问一次对端时，`is_dir`/大小/时间**以对端为准**（拖放那侧就是靠它纠正界面的过时判断）。
#[tokio::test]
async fn probe_entry_reports_what_the_peer_actually_has() {
    let root = std::env::temp_dir().join(format!("orbdrop_probe_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("子目录")).unwrap();
    std::fs::write(root.join("子目录").join("a.txt"), b"data").unwrap();
    std::fs::write(root.join("一个文件"), b"file-content").unwrap();

    let (state, share_id, peer, cfg) = state_with_share(3994, &root, 3994).await;
    let port = orbdrop_lib::server::start_server(state.clone(), 3994).await.expect("起服务");
    credential::set_peer(&peer, "t").unwrap();

    let client = peer_fs::client();
    let base = orbdrop_lib::net::peer_base("127.0.0.1", port);

    let dir = peer_fs::probe_entry(&client, &base, "t", &share_id, "子目录")
        .await
        .expect("问得到")
        .expect("有元数据");
    assert!(dir.is_dir, "对端说它是目录");

    let file = peer_fs::probe_entry(&client, &base, "t", &share_id, "一个文件")
        .await
        .expect("问得到")
        .expect("有元数据");
    assert!(!file.is_dir, "对端说它是文件");
    assert_eq!(file.size, b"file-content".len() as u64, "大小要是真的");
    assert!(file.mtime.is_some(), "修改时间也要带上");

    // 越界路径必须报错，不能"看着成功了"到最后复制才失败
    assert!(peer_fs::probe_entry(&client, &base, "t", &share_id, "../x").await.is_err());

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}

/// 对端是**老版本**（没有 `/stat`，回 404）时：退回"列父目录"拿大小/时间/是否目录，
/// 而不是把整件事判失败。
#[tokio::test]
async fn old_peer_without_stat_falls_back_to_parent_listing() {
    let root = std::env::temp_dir().join(format!("orbdrop_oldpeer_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("deep.txt"), b"deep").unwrap();

    let old = OldPeer::start(root.clone(), 3995);
    let (state, share_id, peer, cfg) = state_with_share(3992, &root, old.port).await;

    let meta = peer_fs::probe_entry(
        &peer_fs::client(),
        &orbdrop_lib::net::peer_base("127.0.0.1", old.port),
        "t",
        &share_id,
        "sub/deep.txt",
    )
    .await
    .expect("退回列父目录也要能拿到")
    .expect("父目录列表里有这一条");
    assert!(!meta.is_dir, "从父目录列表里读到的是文件");
    assert_eq!(meta.size, 4, "父目录列表里的 size 就是它的大小");
    assert!(meta.mtime.is_some(), "父目录列表里也有修改时间");

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}
/// 对端文件**流式**落盘（`orbdrop_lib::stream_peer_file`）：分段要、写 `.part`、写完原子改名。
///
/// 钉住三件事：
/// 1. 大于一个 `READ_CHUNK`（4MB）的文件能被**分段**拉全、逐字节一致（分段循环真的在转）；
/// 2. 成功之后不留 `.part`；
/// 3. 中途失败（对端说没有这个文件）**不留残骸** —— 下载目录里既没有目标文件、也没有 `.part`。
#[tokio::test]
async fn peer_file_streams_to_disk_in_chunks_and_leaves_no_litter() {
    let root = std::env::temp_dir().join(format!("orbdrop_stream_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // 9MB：跨 3 个 4MB 分片，正好把"短读 = 到末尾"这条判据走到。
    let payload: Vec<u8> = (0..9 * 1024 * 1024).map(|b| (b % 251) as u8).collect();
    std::fs::write(root.join("big.bin"), &payload).unwrap();

    let (state, share_id, peer, _cfg) = state_with_share(3997, &root, 3997).await;
    let port = orbdrop_lib::server::start_server(state.clone(), 3997)
        .await
        .expect("起服务");
    credential::set_peer(&peer, "t").unwrap();

    let out_dir = std::env::temp_dir().join(format!("orbdrop_stream_out_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&out_dir).unwrap();
    let target = out_dir.join("copy.bin");
    let file = peer_fs::PeerFile {
        base: format!("http://127.0.0.1:{port}"),
        token: "t".into(),
        share_id: share_id.clone(),
        path: "big.bin".into(),
    };

    let written = orbdrop_lib::stream_peer_file(&reqwest::Client::new(), &file, &target)
        .await
        .expect("流式下载应当成功");
    assert_eq!(written, payload.len() as u64, "返回的字节数应当等于文件大小");
    assert_eq!(std::fs::read(&target).unwrap(), payload, "内容必须逐字节一致");
    assert!(
        !out_dir.join("copy.bin.orbdrop-part").exists(),
        "成功之后不该留 .part"
    );

    // 失败一次：对端没有这条路径 → 既不该有目标文件，也不该有 `.part`。
    let missing = peer_fs::PeerFile {
        path: "不存在.bin".into(),
        ..file
    };
    let target2 = out_dir.join("missing.bin");
    let err = orbdrop_lib::stream_peer_file(&reqwest::Client::new(), &missing, &target2)
        .await
        .expect_err("对端没有这个文件时应当报错");
    assert!(err.contains("404") || err.contains("读取对端文件失败"), "错误信息要说清: {err}");
    assert!(!target2.exists(), "失败不该留下目标文件");
    assert!(
        !out_dir.join("missing.bin.orbdrop-part").exists(),
        "失败不该留下 .part"
    );

    orbdrop_lib::server::stop_server(&state).await;
    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&out_dir);
}
