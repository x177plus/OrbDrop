//! 拖拽下载（F-46）的对端准备：先只走元数据（`/tree` + `/stat`）；**小到兜得住**的项
//! （单文件 ≤ 100 MB、目录总量 ≤ 500 MB）顺手下到暂存目录，按 `CF_HDROP` **真实路径**
//! 拖出去（只认路径的应用——微信这类从不问虚拟文件的——只有这条路能收到）；
//! 超过上限的退回虚拟文件（秒起拖、不落暂存盘），内容等落下那一刻才按需拉。
//!
//! 走真 HTTP：本机起一个实例、地址簿里把自己当成对端，于是 `/tree` 递归、名字清洗、
//! 单文件走 `/stat`、越界拒绝全都被真的走一遍。
//!
//! 注：keyring（系统凭据库）在本机会话下可用；测试用临时 device id，结束清理，
//! 不污染真实配对。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbdrop_lib::config::{Config, PeerDevice, ShareItem};
use orbdrop_lib::credential;
use orbdrop_lib::drag::{DragOutRequest, Payload};
use orbdrop_lib::state::AppState;

fn make_state(port: u16) -> Arc<AppState> {
    let mut config = Config::default();
    config.port = port;
    config.device_id = "device-drag-self".into();
    config.device_name = "拖拽测试".into();
    config.bc_exe_path = None;
    let config_path = std::env::temp_dir().join(format!("orbdrop_drag_cfg_{}.toml", uuid::Uuid::new_v4()));
    Arc::new(AppState::new(config, config_path))
}

/// 对端共享项：`顶层.txt` + `子目录/深层.txt`。
fn remote_share_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_drag_src_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("子目录")).unwrap();
    std::fs::write(dir.join("顶层.txt"), b"top").unwrap();
    std::fs::write(dir.join("子目录").join("深层.txt"), b"deep").unwrap();
    dir
}

async fn wait_payload(state: &Arc<AppState>, ticket: &str) -> Payload {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match state.drag.payload(ticket) {
            Ok(Some(payload)) => return payload,
            Ok(None) => {}
            Err(e) => panic!("准备失败: {e}"),
        }
        assert!(Instant::now() < deadline, "等元数据超时");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn peer_small_tree_is_staged_to_real_paths() {
    let root = remote_share_dir();
    let state = make_state(3997);
    let share = ShareItem::new(root.clone());
    let share_id = share.id.clone();
    state.config.lock().shares.push(share);
    let port = orbdrop_lib::server::start_server(state.clone(), 3997)
        .await
        .expect("起服务");

    // 把自己当对端：地址簿一行 + 凭据库里它的 token，于是带 Bearer 的调用被认成它。
    let peer = format!("device-drag-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: peer.clone(),
        name: "对端".into(),
        ip: "127.0.0.1".into(),
        port,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
    });
    credential::set_peer(&peer, "drag-test-token").unwrap();

    // ---- 拖小目录：阈值以内 → **提前转存**，交出去的是真实路径 ----
    let req = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id: share_id.clone(),
        path: String::new(),
        name: "远端目录".into(),
        is_dir: true,
    };
    let ticket = state.drag.prepare(&state, &req).expect("登记拖拽");
    assert!(!ticket.ready, "对端项要先走一遍目录（外加可能的转存）");
    let Payload::RealPaths(paths) = wait_payload(&state, &ticket.ticket).await else {
        panic!("阈值以内的小目录应该提前转存成真实路径");
    };
    // 一整棵树交给资源管理器的只有**根那一条**：`CF_HDROP` 由它自己去递归
    assert_eq!(paths.len(), 1, "拖一棵树只交根路径: {paths:?}");
    let staged = &paths[0];
    assert_eq!(staged.file_name().unwrap(), "远端目录", "落点里那一项还是原来的名字");
    assert!(staged.is_dir(), "树根还原成目录: {staged:?}");
    assert_eq!(std::fs::read(staged.join("顶层.txt")).unwrap(), b"top");
    assert_eq!(std::fs::read(staged.join("子目录").join("深层.txt")).unwrap(), b"deep");

    // ---- 越界路径下不来 ----
    let escape = DragOutRequest { path: "../outside.txt".into(), ..req.clone() };
    let escape_ticket = state.drag.prepare(&state, &escape).expect("登记");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match state.drag.payload(&escape_ticket.ticket) {
            Err(e) => {
                assert!(!e.is_empty(), "越界要带一句能看懂的原因");
                break;
            }
            Ok(None) => {}
            Ok(Some(payload)) => panic!("越界路径不该备好: {payload:?}"),
        }
        assert!(Instant::now() < deadline, "等越界路径失败超时");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(staged.parent().unwrap_or(staged)); // 本次的暂存目录
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&state.config_path);
    orbdrop_lib::server::stop_server(&state).await;
}

/// **共享项根就是一个文件**（界面拖整行 → `path` 为空）——对端拖拽曾经栽在这一档：
/// `/file` 少了空路径判断，`share_root.join("")` 带上结尾分隔符，`canonicalize` 直接失败
/// （`ERROR_INVALID_NAME`），于是回 403、内容一个字节都下不来。
#[tokio::test]
async fn peer_file_root_share_is_staged_with_contents() {
    let dir = std::env::temp_dir().join(format!("orbdrop_fileroot_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("单文件.txt");
    std::fs::write(&file, b"file root content").unwrap();

    // 共享项根指向那个文件本身
    let (state, share_id, peer, cfg) = state_with_share(3986, &file, 3986).await;
    orbdrop_lib::server::start_server(state.clone(), 3986).await.expect("起服务");

    let req = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id,
        path: String::new(), // 共享项根：界面拖的就是整行
        name: "单文件.txt".into(),
        is_dir: false,
    };
    let ticket = state.drag.prepare(&state, &req).expect("登记拖拽");
    let Payload::RealPaths(paths) = wait_payload(&state, &ticket.ticket).await else {
        panic!("单个小文件应该提前转存成真实路径");
    };
    assert_eq!(paths[0].file_name().unwrap(), "单文件.txt");
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"file root content");

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(paths[0].parent().unwrap_or(&paths[0]));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}

/// 超过上限的项**不转存**：退回虚拟文件（秒起拖、不落暂存盘），代价是只有支持虚拟文件的
/// 目标能收。这里用 `set_len` 造一个 101 MB 的文件（只改元数据、不写内容）。
#[tokio::test]
async fn peer_oversized_item_stays_virtual() {
    let dir = std::env::temp_dir().join(format!("orbdrop_bigitem_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("大文件.bin");
    std::fs::File::create(&big).unwrap().set_len(101 * 1024 * 1024).unwrap();

    let (state, share_id, peer, cfg) = state_with_share(3987, &big, 3987).await;
    orbdrop_lib::server::start_server(state.clone(), 3987).await.expect("起服务");

    let req = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id,
        path: String::new(),
        name: "大文件.bin".into(),
        is_dir: false,
    };
    let ticket = state.drag.prepare(&state, &req).expect("登记拖拽");
    let Payload::Virtual(items) = wait_payload(&state, &ticket.ticket).await else {
        panic!("超过单文件上限就不该转存");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].size, 101 * 1024 * 1024, "大小来自 /stat（描述符拿它显示进度）");
    assert!(items[0].size_known, "大小是问到的，不是猜的");

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}

// ---------------------------------------------------------------------------
// 报过的两个坑：isDir 猜错（文件被当成目录）、对端老版本没有 `/stat`（404）
// ---------------------------------------------------------------------------

/// 一个"老版本对端"：**只有 `/tree`**（没有 `/stat`），用来验证退回"列父目录"那条路。
///
/// 只认 ASCII 名字（直接比对原始查询串，不解码），所以喂给它的目录名用英文。
struct OldPeer {
    port: u16,
}

impl OldPeer {
    fn start(root: PathBuf, port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("绑端口");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
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
        // 等它起来
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        OldPeer { port }
    }
}

/// 搭一个只有共享项、没有内容的实例，并把地址簿里那一行指向 `peer_port`。
async fn state_with_share(port: u16, root: &std::path::Path, peer_port: u16) -> (Arc<AppState>, String, String, PathBuf) {
    let state = make_state(port);
    let share = ShareItem::new(root.to_path_buf());
    let share_id = share.id.clone();
    state.config.lock().shares.push(share);
    let peer = format!("device-drag-{}", uuid::Uuid::new_v4());
    state.add_peer_book(PeerDevice {
        id: peer.clone(),
        name: "对端".into(),
        ip: "127.0.0.1".into(),
        port: peer_port,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
    });
    credential::set_peer(&peer, "t").unwrap();
    let cfg = state.config_path.clone();
    (state, share_id, peer, cfg)
}

/// `/shares` 报的 isDir 必须是**真元数据**：没有扩展名的文件不能被当成目录。
#[tokio::test]
async fn shares_reports_is_dir_from_real_metadata() {
    let root = std::env::temp_dir().join(format!("orbdrop_isdir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README"), b"no extension").unwrap();       // 文件：没有点
    std::fs::create_dir_all(root.join("带点.的目录")).unwrap();           // 目录：名字里有"扩展名"

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

/// 前端带来的 `is_dir` 可能过时：真去问一次对端，**以对端为准**。
#[tokio::test]
async fn peer_metadata_overrides_a_stale_is_dir_flag() {
    let root = std::env::temp_dir().join(format!("orbdrop_stale_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("子目录")).unwrap();
    std::fs::write(root.join("子目录").join("a.txt"), b"data").unwrap();
    std::fs::write(root.join("一个文件"), b"file-content").unwrap();

    let (state, share_id, peer, cfg) = state_with_share(3994, &root, 3994).await;
    let port = orbdrop_lib::server::start_server(state.clone(), 3994).await.expect("起服务");

    // (1) 目录被说成文件：必须还是按目录走 —— 转存出来的是一个**目录**，里面有那个子文件
    let as_file = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id: share_id.clone(),
        path: "子目录".into(),
        name: "子目录".into(),
        is_dir: false, // 过时的判断
    };
    let ticket = state.drag.prepare(&state, &as_file).unwrap();
    let Payload::RealPaths(paths) = wait_payload(&state, &ticket.ticket).await else {
        panic!("应该是提前转存的真实路径");
    };
    assert!(paths[0].is_dir(), "对端说它是目录，转存出来就必须是目录: {paths:?}");
    assert_eq!(std::fs::read(paths[0].join("a.txt")).unwrap(), b"data");
    let staged_1 = paths[0].parent().unwrap().to_path_buf();

    // (2) 文件被说成目录：退化成单文件，而不是报一句 "Path is not a directory"
    let as_dir = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id,
        path: "一个文件".into(),
        name: "一个文件".into(),
        is_dir: true, // 过时的判断（老版本 /shares 的扩展名猜测就是这样）
    };
    let ticket = state.drag.prepare(&state, &as_dir).unwrap();
    let Payload::RealPaths(paths) = wait_payload(&state, &ticket.ticket).await else {
        panic!("应该是提前转存的真实路径");
    };
    assert!(paths[0].is_file(), "对端说它是文件: {paths:?}");
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"file-content");
    let staged_2 = paths[0].parent().unwrap().to_path_buf();

    let _ = port;
    let _ = PathBuf::new();
    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&staged_1);
    let _ = std::fs::remove_dir_all(&staged_2);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}

/// 对端是**老版本**（没有 `/stat`，回 404）时：退回"列父目录"拿大小/时间/是否目录，
/// 而不是把拖拽整个判失败。
#[tokio::test]
async fn old_peer_without_stat_falls_back_to_parent_listing() {
    let root = std::env::temp_dir().join(format!("orbdrop_oldpeer_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("deep.txt"), b"deep").unwrap();

    let old = OldPeer::start(root.clone(), 3995);
    let (state, share_id, peer, cfg) = state_with_share(3992, &root, old.port).await;

    let req = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id,
        path: "sub/deep.txt".into(),
        name: "deep.txt".into(),
        is_dir: false,
    };
    let ticket = state.drag.prepare(&state, &req).unwrap();
    let Payload::Virtual(items) = wait_payload(&state, &ticket.ticket).await else {
        panic!("应该是虚拟文件清单");
    };
    assert_eq!(items.len(), 1);
    assert!(!items[0].is_dir, "从父目录列表里读到的是文件");
    assert_eq!(items[0].size, 4, "父目录列表里的 size 就是它的大小");
    assert!(items[0].mtime.is_some(), "父目录列表里也有修改时间");

    credential::remove_peer(&peer).ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&cfg);
    orbdrop_lib::server::stop_server(&state).await;
}
