//! 拖拽下载（F-46）的对端准备：**只走元数据**（`/tree` + `/stat`），内容等到落下那一刻才拉。
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
async fn peer_directory_walk_yields_virtual_items_with_metadata() {
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
        auto_connect: false,
    });
    credential::set_peer(&peer, "drag-test-token").unwrap();

    // ---- 拖目录：清单里要有目录本身、子目录、两个文件（名字按相对落点的 `\` 路径）----
    let req = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id: share_id.clone(),
        path: String::new(),
        name: "远端目录".into(),
        is_dir: true,
    };
    let ticket = state.drag.prepare(&state, &req).expect("登记拖拽");
    assert!(!ticket.ready, "对端项要先走一遍目录");
    let Payload::Virtual(items) = wait_payload(&state, &ticket.ticket).await else {
        panic!("对端项应该是虚拟文件");
    };
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names.len(),
        4,
        "目录本身 + 子目录 + 两个文件，清单: {names:?}"
    );
    assert!(names.contains(&"远端目录"), "目录自己也是一条: {names:?}");
    assert!(names.contains(&"远端目录\\子目录"), "清单: {names:?}");
    assert!(names.contains(&"远端目录\\顶层.txt"), "清单: {names:?}");
    assert!(names.contains(&"远端目录\\子目录\\深层.txt"), "清单: {names:?}");
    // 不变式：任何一条的父目录都要排在它前面（资源管理器照这个顺序建目录）
    for (index, item) in items.iter().enumerate() {
        if let Some((parent, _)) = item.name.rsplit_once('\\') {
            let parent_at = names.iter().position(|n| *n == parent).expect("父目录必须在清单里");
            assert!(parent_at < index, "父目录 {parent} 必须排在自己前面");
        }
    }

    let by_name = |name: &str| items.iter().find(|i| i.name == name).expect("清单里有这条");
    assert!(by_name("远端目录").is_dir && by_name("远端目录").source.is_none(), "目录项没有内容来源");
    assert_eq!(by_name("远端目录\\顶层.txt").size, 3, "文件大小来自 /tree，落下时用于显示进度");
    assert!(by_name("远端目录\\顶层.txt").mtime.is_some(), "修改时间要带上（才知道该不该写 FD_WRITESTIME）");
    let src = by_name("远端目录\\子目录\\深层.txt").source.as_ref().expect("文件要有来源");
    assert_eq!(src.share_id, share_id);
    assert_eq!(src.path, "子目录/深层.txt", "来源路径用 `/` 分隔");

    // ---- 拖单个文件：走 /stat（`/tree` 只列目录，这条路没有别的尺寸来源）----
    let file_req = DragOutRequest {
        path: "子目录/深层.txt".into(),
        name: "深层.txt".into(),
        is_dir: false,
        ..req.clone()
    };
    let file_ticket = state.drag.prepare(&state, &file_req).expect("登记文件拖拽");
    let Payload::Virtual(files) = wait_payload(&state, &file_ticket.ticket).await else {
        panic!("对端项应该是虚拟文件");
    };
    assert_eq!(files.len(), 1, "单个文件就是清单里的一条");
    assert_eq!(files[0].name, "深层.txt");
    assert_eq!(files[0].size, 4, "大小来自 /stat");
    assert!(!files[0].is_dir);

    // ---- 落下时拉内容：直接从服务端取这一段字节，内容就是源文件 ----
    let fetched = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/file"))
        .query(&[("share", share_id.as_str()), ("path", "子目录/深层.txt"), ("offset", "0"), ("len", "4")])
        .bearer_auth("drag-test-token")
        .send()
        .await
        .expect("取内容");
    assert_eq!(fetched.bytes().await.unwrap().as_ref(), b"deep");
    // 只要一半时服务端按 len 截断：短读 = 到末尾（流的结束判据）
    let half = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/file"))
        .query(&[("share", share_id.as_str()), ("path", "子目录/深层.txt"), ("offset", "2"), ("len", "99")])
        .bearer_auth("drag-test-token")
        .send()
        .await
        .expect("取后半段");
    assert_eq!(half.bytes().await.unwrap().as_ref(), b"ep");

    // ---- 越界路径下不来 ----
    let escape = DragOutRequest { path: "../outside.txt".into(), ..file_req.clone() };
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
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&state.config_path);
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
        auto_connect: false,
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

    // (1) 目录被说成文件：必须还是按目录走（清单里要有子目录那一条）
    let as_file = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id: share_id.clone(),
        path: "子目录".into(),
        name: "子目录".into(),
        is_dir: false, // 过时的判断
    };
    let ticket = state.drag.prepare(&state, &as_file).unwrap();
    let Payload::Virtual(items) = wait_payload(&state, &ticket.ticket).await else {
        panic!("应该是虚拟文件清单");
    };
    assert!(items[0].is_dir, "对端说它是目录");
    assert!(items.iter().any(|i| i.name.ends_with(r"子目录\a.txt")), "目录要展开: {items:?}");

    // (2) 文件被说成目录：退化成单文件，而不是报一句 "Path is not a directory"
    let as_dir = DragOutRequest {
        peer_id: Some(peer.clone()),
        share_id,
        path: "一个文件".into(),
        name: "一个文件".into(),
        is_dir: true, // 过时的判断（老版本 /shares 的扩展名猜测就是这样）
    };
    let ticket = state.drag.prepare(&state, &as_dir).unwrap();
    let Payload::Virtual(items) = wait_payload(&state, &ticket.ticket).await else {
        panic!("应该是虚拟文件清单");
    };
    assert_eq!(items.len(), 1, "一个文件就一条: {items:?}");
    assert!(!items[0].is_dir);
    assert_eq!(items[0].size, 12, "大小来自 /stat");
    assert!(items[0].source.is_some());

    let _ = port;
    let _ = PathBuf::new();
    credential::remove_peer(&peer).ok();
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
