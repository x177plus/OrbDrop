//! F-107 集成测试：跨网段中转记忆（真 HTTP + 真 TLS + 真中转端点，不是 mock）。
//!
//! 三块都是"真的跑起来"的：
//!
//! 1. **`/relay/open` 的放宽**（S-09 §12.1）：未配对设备必须**自报身份**并被核实，才能借中转；
//!    冒名、端口对不上、不带身份的一律拒。
//! 2. **`/relay/route`**（§10）：反向路由记录由**已配对**对端或**自报且被核实**的对端写；
//!    目标段必须含本机地址，其余丢弃。
//! 3. **选路与失效转移**（§8/§9）：池 → 记住的池 → 广扫的候选顺序；C 掉线后自动改走**未配对**
//!    的 D；端点真的把字节搬到目标；以及"收到反向记录之后"的那一侧按记录里的池借到中转。
//!
//! ⚠️ **平台限制（与 F-106 同一条，见 `relay` 模块头）**：Windows 上 reqwest 连不上
//! "IPv4 映射的 IPv6"字面量（实测 `AddrNotAvailable`），所以**本机作为请求方**时，"经端点再探
//! 一次目标"这一步在 Windows 上跑不通（手机/桌面端自己那台做请求方时不受影响 —— 它们是双栈）。
//! 因此这里：控制面与选路走真的 reqwest；**端点的数据面用裸 `TcpStream` 拨**（拨它映射到的那个
//! IPv4，同一个监听、同一个端口），与 `p1_integration` 里那条中转用例同一个写法。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbdrop_lib::config::{Config, PeerDevice};
use orbdrop_lib::state::{AppState, DiscoveredPeer};
use orbdrop_lib::{credential, server};

// ---------------------------------------------------------------------------
// 夹具（与 `p1_integration` 同源；那个文件的夹具是私有的）
// ---------------------------------------------------------------------------

fn isolate_data_dir() {
    static INIT: std::sync::LazyLock<()> = std::sync::LazyLock::new(|| {
        let dir = std::env::temp_dir().join(format!("orbdrop_it_relaymem_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时数据目录");
        std::env::set_var("ORBDROP_DATA_DIR", &dir);
        // 这组用例把多个 `AppState` 跑在同一进程里，目标只能写 127.0.0.1。
        // `relay::open` 会拒绝回环目标（P0：中转回环会拿到本机 UI 的免认证权限）；
        // 这个测试开关只在 debug 构建生效，release 永远拒绝。
        std::env::set_var("ORBDROP_TEST_ALLOW_LOOPBACK_RELAY", "1");
    });
    std::sync::LazyLock::force(&INIT);
}

fn make_state(port: u16, device_id: &str, device_name: &str) -> Arc<AppState> {
    isolate_data_dir();
    let dir = std::env::temp_dir().join(format!("orbdrop_rm_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    config.download_dir = dir.clone();
    config.bc_exe_path = None;
    Arc::new(AppState::new(config, dir.join("config.toml")))
}

/// 本机在局域网上的出口地址（纯离线 CI 上拿不到就跳过整条用例）。
fn local_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip)
    }
}

/// 同一个网段里**不会有人**的那个地址（用来扮演"看得见却连不通"的同段设备）。
fn unused_in_subnet(lan: std::net::IpAddr) -> std::net::IpAddr {
    let std::net::IpAddr::V4(v4) = lan else {
        return lan;
    };
    let mut octets = v4.octets();
    // 与自己的地址不同、且落在同一段（用 .253 那种"几乎不会有人"的位置）。
    octets[3] = if octets[3] == 253 { 252 } else { 253 };
    std::net::IpAddr::V4(std::net::Ipv4Addr::from(octets))
}

async fn wait_until<F: Fn() -> bool>(what: &str, ok: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等超时：{what}");
}

fn peer(id: &str, name: &str, ip: &str, port: u16) -> PeerDevice {
    PeerDevice {
        id: id.to_string(),
        name: name.to_string(),
        ip: ip.to_string(),
        port,
        online: true,
        paired: false,
        auto_connect: false,
        paused: false,
        relay: None,
    }
}

/// 一个回显服务器（当"目标设备"用）：中转端点连上它、字节原样回来。
async fn echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
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
    (addr, task)
}

/// 拨一条中转端点，把字节日志送过去再读回来（走**它映射到的那个 IPv4**，见文件头）。
async fn probe_endpoint_bytes(host: &str, port: u16, payload: &[u8]) -> Vec<u8> {
    let ip = orbdrop_lib::net::parse_host(host).expect("端点主机可解析");
    let ip = match ip {
        std::net::IpAddr::V6(v6) => std::net::IpAddr::V4(v6.to_ipv4_mapped().expect("映射地址")),
        other => other,
    };
    let mut sock = tokio::net::TcpStream::connect(std::net::SocketAddr::new(ip, port))
        .await
        .expect("连中转端点");
    tokio::io::AsyncWriteExt::write_all(&mut sock, payload)
        .await
        .unwrap();
    let mut buf = vec![0u8; payload.len()];
    let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
        .await
        .unwrap();
    buf.truncate(n);
    buf
}

// ---------------------------------------------------------------------------
// 1. `/relay/open`：未配对请求方要自报身份并被核实（S-09 的放宽）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relay_open_verifies_a_non_paired_requester() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let provider_id = format!("provider-{tag}");
    let requester_id = format!("requester-{tag}");
    let target_id = format!("target-{tag}");

    // 中转方（开着 S-09）+ 一个真的"请求方"（它的 /pair/info 会答出自己的 id）。
    let provider = make_state(4700, &provider_id, "PROVIDER");
    provider.config.lock().allow_relay = true;
    let requester = make_state(4710, &requester_id, "REQUESTER");
    let provider_port = server::start_server(provider.clone(), 4700).await.unwrap();
    let requester_port = server::start_server(requester.clone(), 4710).await.unwrap();

    // 目标：中转方**认识**它（在自己的地址簿里且在线）→ 直接用自己那份地址，不必核实请求方捎来的。
    let (echo, echo_task) = echo_server().await;
    provider.add_peer_book(peer(&target_id, "TARGET", "127.0.0.1", echo.port()));

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan}:{provider_port}/relay/open");
    let ask = |body: serde_json::Value| client.post(&url).json(&body).send();

    // ① 什么都不带（没有凭据、也没有自报身份）→ 401：连"谁在请求"都不知道，没得谈。
    assert_eq!(
        ask(serde_json::json!({ "target": target_id }))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "未配对设备必须自报身份"
    );

    // ② 自报身份但**冒名**（说自己是另一个 id）→ 核实不过 → 403。
    assert_eq!(
        ask(serde_json::json!({
            "target": target_id,
            "requester": { "id": "not-that-device", "port": requester_port },
        }))
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::FORBIDDEN,
        "自报的 id 必须与「来源地址 + 自报端口」上问出来的 id 一致"
    );

    // ③ 自报的端口上没人答话（例如报了个别人家的端口）→ 核实不过 → 403。
    assert_eq!(
        ask(serde_json::json!({
            "target": target_id,
            "requester": { "id": requester_id, "port": 1 },
        }))
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::FORBIDDEN,
        "核实不了身份就不给转"
    );

    // ④ 身份完整且对得上 → 200，端点可以借。
    let opened: serde_json::Value = ask(serde_json::json!({
        "target": target_id,
        "requester": { "id": requester_id, "port": requester_port },
    }))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let host = opened["host"].as_str().expect("host").to_string();
    let relay_port = u16::try_from(opened["port"].as_u64().expect("port")).unwrap();
    assert!(
        host.starts_with("[::ffff:"),
        "端点字面量必须与直连键位不同: {host}"
    );
    assert_eq!(
        opened["targetAddr"].as_str(),
        Some(format!("127.0.0.1:{}", echo.port()).as_str()),
        "回话里要带上**目标此刻的地址**（请求方靠它补上自己那一行：只经中转见过一面的那侧从来不知道这个）"
    );
    assert_eq!(
        probe_endpoint_bytes(&host, relay_port, b"non-paired-requester").await,
        b"non-paired-requester",
        "端点要原样搬字节（未配对那一档也一样）"
    );
    // 中转流量记账（F-106/S-09）：这条连接真的搬过字节，而且**收发两个方向都算** ——
    // 端点上的 `pump` 边搬边计（`relay::CountingStream`），连接结束那一刻落进 `state.relay_bytes`。
    let counted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if provider.relay_conns.load(Ordering::Relaxed) == 1 {
                return provider.relay_bytes.load(Ordering::Relaxed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("中转连接结束时要记账");
    assert_eq!(
        counted,
        2 * b"non-paired-requester".len() as u64,
        "收发两个方向都要计（这里是请求方→目标、目标回显→请求方各一趟）"
    );
    // 刚才那一问证明了"来源地址 + 自报端口"上答得出这个 id：中转方要把地址记下来
    // （只补空行）—— 之后它自己来请中转、或它成为别人的目标时，都不必再靠 hint。
    assert_eq!(
        provider.get_peer(&requester_id).map(|p| (p.ip, p.port)),
        Some((lan.clone(), requester_port)),
        "核实过的那一台要留下地址"
    );

    // ⑤ 开关关掉 → 403（放宽的是"认识不认识"，不是"愿不愿意"）。
    provider.config.lock().allow_relay = false;
    assert_eq!(
        ask(serde_json::json!({
            "target": target_id,
            "requester": { "id": requester_id, "port": requester_port },
        }))
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::FORBIDDEN
    );

    server::stop_server(&provider).await;
    server::stop_server(&requester).await;
    echo_task.abort();
}

// ---------------------------------------------------------------------------
// 2. `/relay/route`：已配对对端或**自报且被核实**的对端；目标段必须含本机地址
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relay_route_accepts_only_paired_peers_and_local_targets() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let receiver_id = format!("receiver-{tag}");
    let sender_id = format!("sender-{tag}");

    let receiver = make_state(4720, &receiver_id, "RECEIVER");
    let receiver_port = server::start_server(receiver.clone(), 4720).await.unwrap();

    let Some(own_net) = orbdrop_lib::subnet::host_nets()
        .first()
        .map(|c| c.to_cidr_string())
    else {
        return; // 没有非回环网卡的机器：这条判断无从谈起
    };

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan}:{receiver_port}/relay/route");
    let device =
        |id: &str, ip: &str| serde_json::json!({ "id": id, "name": id, "ip": ip, "port": 3890 });
    let body = serde_json::json!({ "routes": [
        {
            "targetCidr": own_net,
            "viaCidr": "10.44.0.0/16",
            "viaDevices": [device("relay-c", "10.44.0.7"), device("relay-d", "10.44.0.8")],
            "viaPaired": ["relay-c"],
            "confirmedAt": "2026-09-30T12:00:00Z",
        },
        {
            // 与接收方无关的一段：必须丢弃（防投毒的第一关）。
            "targetCidr": "203.0.113.0/24",
            "viaCidr": "10.44.0.0/16",
            "viaDevices": [device("relay-e", "10.44.0.9")],
            "viaPaired": [],
        },
    ]});

    // 没有凭据、也没有自报身份 → 401。
    assert_eq!(
        client.post(&url).json(&body).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "要么带凭据，要么自报身份"
    );

    // 未配对的一侧（被中转的 B 常常从来没跟锚点配过）也要收得到：自报身份 + 被核实。
    // 少了这一条，B 就永远不知道"A 是怎么过来的"。
    let (sender_id2, sender_port2) = {
        let id = format!("sender2-{tag}");
        let st = make_state(4725, &id, "SENDER2");
        let port = server::start_server(st.clone(), 4725).await.unwrap();
        (id, port)
    };
    let reply: serde_json::Value = client
        .post(&url)
        .json(&serde_json::json!({
            "routes": body["routes"],
            "requester": { "id": sender_id2, "port": sender_port2 },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reply["accepted"].as_array().unwrap().len(),
        1,
        "自报身份核实通过就该收下（记录只是提示，用之前仍由中转方核实目标）"
    );
    assert!(
        receiver.is_ephemeral_route(&own_net),
        "未认证来源的记录只做本进程内的临时提示（不落盘）"
    );

    // 已配对对端（Bearer）写的同一份记录：升格为可落盘的记忆。
    let token = format!("tok-{tag}");
    credential::set_peer(&sender_id, &token).unwrap();
    let reply: serde_json::Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reply["accepted"].as_array().unwrap().len(),
        1,
        "只有含本机地址的那一条被采纳"
    );
    assert_eq!(reply["ignored"].as_array().unwrap().len(), 1);
    assert!(
        !receiver.is_ephemeral_route(&own_net),
        "Bearer 来源的记录可信，升格为可落盘的策略"
    );

    // 自报身份对不上 → 拒。
    assert_eq!(
        client
            .post(&url)
            .json(&serde_json::json!({
                "routes": body["routes"],
                "requester": { "id": "not-that-one", "port": sender_port2 },
            }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );

    {
        let config = receiver.config.lock();
        let route = config.relay_route(&own_net).expect("策略已落库");
        assert_eq!(route.via_cidr, "10.44.0.0/16");
        assert_eq!(
            route.via_devices,
            vec!["relay-c".to_string(), "relay-d".to_string()]
        );
        assert!(
            route.via_paired.is_empty(),
            "本机与它们都没配对：这里记的是**本机**的配对状态"
        );
        assert!(route.seen_at.is_some(), "记下池的刷新时刻（TTL 靠它）");
        // 池里并进来的设备**不写地址簿** —— 那是别人的段。
        assert!(
            !config.peers.iter().any(|p| p.id == "relay-c"),
            "反向记录不该往地址簿里塞设备"
        );
    }
    assert!(
        receiver.get_peer("relay-c").is_some(),
        "池里的设备要能被拨到"
    );

    credential::remove_peer(&sender_id).ok();
    server::stop_server(&receiver).await;
}

/// `/relay/route` 的自报核实只能证明"这个地址上答得出这个 id"，不能改写**已配对设备**的
/// 名字/地址/在线状态（与 `/relay/open` 同一道闸）。否则局域网里任何一台机器都能把自己
/// 伪装成那台设备，把它的名字写进地址簿并把它标成在线。
#[tokio::test]
async fn relay_route_does_not_let_a_self_report_rewrite_a_paired_peer() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let receiver_id = format!("name-receiver-{tag}");
    let paired_id = format!("paired-{tag}");

    let receiver = make_state(4820, &receiver_id, "RECEIVER");
    let receiver_port = server::start_server(receiver.clone(), 4820).await.unwrap();
    receiver.add_peer_book(PeerDevice {
        online: false,
        paired: true,
        ..peer(&paired_id, "ORIGINAL", "10.9.9.9", 3890)
    });

    // 冒充者把自己伪装成 paired_id：它的 `/pair/info` 会回这个 id 和另一个名字，
    // 于是「自报 + 核实」这一关会通过 —— 但已配对设备的行不许被它改写。
    let impostor = make_state(4821, &paired_id, "SPOOF");
    let impostor_port = server::start_server(impostor.clone(), 4821).await.unwrap();

    let url = format!("https://{lan}:{receiver_port}/relay/route");
    let status = orbdrop_lib::net::client()
        .post(&url)
        .json(&serde_json::json!({
            "routes": [],
            "requester": { "id": paired_id, "port": impostor_port, "name": "SPOOF" },
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "核实通过，请求本身要收下: {status}");

    let row = receiver.get_peer(&paired_id).expect("已配对行还在");
    assert_eq!(row.name, "ORIGINAL", "自报不能改写已配对设备的名字");
    assert_eq!(row.ip, "10.9.9.9", "自报不能改写已配对设备的地址");
    assert!(!row.online, "自报不能把已配对设备标成在线");
    assert_eq!(
        receiver
            .config
            .lock()
            .peers
            .iter()
            .find(|p| p.id == paired_id)
            .map(|p| p.name.clone()),
        Some("ORIGINAL".to_string()),
        "地址簿里的名字也不能被改写"
    );

    server::stop_server(&impostor).await;
    server::stop_server(&receiver).await;
}

// ---------------------------------------------------------------------------
// 3. 选路：先借池里的，C 掉线后自动改走**未配对**的 D（不配对也能拿到池）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relay_pool_directs_and_fails_over_to_an_unpaired_relay() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let unreachable = unused_in_subnet(lan.parse().unwrap()).to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let (a_id, c_id, d_id) = (
        format!("phone-a-{tag}"),
        format!("pc-c-{tag}"),
        format!("pc-d-{tag}"),
    );
    let (t1_id, t2_id) = (format!("phone-b1-{tag}"), format!("phone-b2-{tag}"));

    // 两台中转方（都开着 S-09），以及两台"目标"回显服务器。
    let a = make_state(4730, &a_id, "PHONE-A");
    let c = make_state(4740, &c_id, "PC-C");
    let d = make_state(4750, &d_id, "PC-D");
    c.config.lock().allow_relay = true;
    d.config.lock().allow_relay = true;
    let (echo1, echo1_task) = echo_server().await;
    let (echo2, echo2_task) = echo_server().await;

    let port_a = server::start_server(a.clone(), 4730).await.unwrap();
    let port_c = server::start_server(c.clone(), 4740).await.unwrap();
    let port_d = server::start_server(d.clone(), 4750).await.unwrap();

    // C、D 都认识两台目标（"够得着它们"）。目标在 A 眼里是**同一个网段里够不着的地址**。
    for st in [&c, &d] {
        st.add_peer_book(peer(&t1_id, "B1", "127.0.0.1", echo1.port()));
        st.add_peer_book(peer(&t2_id, "B2", "127.0.0.1", echo2.port()));
    }
    // C 还认识同段的 D —— A 的"中转池"就是从 C 的回话里来的。
    // 这里直接把 D 的能力位摆好（等于 C 已经探过它）：**不跑 C 的探活循环** —— 那样会把
    // 两个回显目标探成"离线"，中转方就不会再用地址簿里的那一条了。
    c.add_peer_book(peer(&d_id, "PC-D", &lan, port_d));
    c.set_relay_capable(&d_id, true);

    // A 与 C 的关系：**没有配对、也没有 token** —— 于是两条路都要走放宽后的那一档
    // （自报身份 + 被核实）。这比"假装已配对"更贴近"换一台没配过的设备也能借"的真实用法。
    //
    // 不要在这里标 `paired: true`：已配对却没有指纹的出站拨号现在会在 TLS 阶段被拒
    // （`credential::dial_pin` 的 `Deny`，那条收口是为了不把 token 发到未钉住的通道上），
    // 与这条用例要走的"未配对"路径正好相反。
    a.add_peer_book(peer(&c_id, "PC-C", &lan, port_c));
    // 等于"A 的 5s 探活问过 C 的 `/pair/info`"（本机首跳就是靠这一位把 C 挑出来的）：
    // 这一位不看配对，只看对方**开没开** S-09。
    a.set_relay_capable(&c_id, true);
    assert_eq!(
        a.listening_port(),
        port_a,
        "请求方自报的端口就是它实际监听的端口"
    );

    // B1 在 A 眼里是"同一个网段里、但是够不着的那个地址"（AP 把同段单播隔开就是这个样子）。
    a.add_peer_book(peer(&t1_id, "B1", &unreachable, echo1.port()));

    // 直连够不着 B1 → 请池里的中转方开端点（真的 HTTP + TLS）。
    let opened = orbdrop_lib::relay::ensure_for(&a, &t1_id).await;
    let relay = opened.expect("池里要有可用的中转方");
    assert_eq!(relay.via, c_id, "此时唯一可用的中转方是 C");
    assert_eq!(
        probe_endpoint_bytes(&relay.host, relay.port, b"hello-through-c").await,
        b"hello-through-c",
        "端点是活的：字节经 C 到 B1 又原样回来"
    );
    assert_eq!(
        a.get_peer(&t1_id)
            .unwrap()
            .relay
            .as_ref()
            .map(|r| r.via.clone()),
        Some(c_id.clone())
    );
    assert!(c.relays.active() >= 1, "中转方那边确实开出去了一条");

    // bug 3 的回归：A **从来没跟 C 配对**、也没换过 C 的 `/discover` 视野（没有 token），
    // 池是 **C 在 `/relay/open` 回话里自报**的 —— 也正是"C 掉线之后要能换 D"的依据。
    let t1_cidr = a.cidr_of(&t1_id, &unreachable).expect("B1 所在的段");
    let pool_ids: Vec<String> = a.pool_for(&t1_cidr).into_iter().map(|p| p.id).collect();
    assert!(
        pool_ids.contains(&c_id) && pool_ids.contains(&d_id),
        "池里要有 C 也要有 D（C 报来的）：{pool_ids:?}"
    );
    assert!(
        a.get_peer(&d_id).is_some(),
        "报来的 D 要并进设备表（否则拨不出去）"
    );

    // B2 晚一步进列表：这一条要走的正是"C 掉线之后换 D"（而不是探活那一拍顺手开的端点）。
    a.add_peer_book(peer(&t2_id, "B2", &unreachable, echo2.port()));

    // C 掉线 → 下一台：**未配对**的 D 也必须能借（S-09 的放宽）。
    server::stop_server(&c).await;
    assert!(
        credential::get_peer_token(&d_id).unwrap().is_none(),
        "A 手里没有 D 的凭据 —— 走的是「自报身份 + 被核实」那一档"
    );
    let opened = orbdrop_lib::relay::ensure_for(&a, &t2_id).await;
    let relay = opened.expect("C 掉线后要能借到 D");
    assert_eq!(relay.via, d_id, "池里剩下的那台接手");
    assert_eq!(
        probe_endpoint_bytes(&relay.host, relay.port, b"hello-through-d").await,
        b"hello-through-d",
        "未配对的中转方同样要把字节搬到位"
    );
    wait_until("D 那边开出了端点", || d.relays.active() >= 1).await;

    // 真机反馈第七轮（回归）：**来回切换 C/D**（"C 离线 → D 接手 → D 离线一会儿 → C 离线
    // 又轮到 D"）之后，D 必须还在记忆里、还借得动。以前池是**整份替换**的：D 离线那一拍的
    // 活视野（锚点只报自己）就把 D 的最后已知地址抹掉，等 C 也离线、D 再上线时谁也想不起
    // 它 —— 用户看到的是"AB 都看不到 D"，消息发不出去（只能等 C 回来把视野补上）。
    //
    // 这里按真机顺序摆：① 经 D 真的到过目标（写策略与池，见 `note_relay_success`）；
    // ② 锚点停服**前**那一拍的回话里只有它自己（D 那一拍不在线）→ 吸收它；
    // ③ D 那行（锚点带来的）到期被回收、本机这边那条端点也收了；
    // ④ 再发消息：必须还借得到 D。
    a.note_relay_success(&t2_id, &d_id);
    a.learned
        .lock()
        .insert(d_id.clone(), Instant::now() - Duration::from_secs(60));
    a.sweep_learned();
    a.sweep_relay_memory();
    assert!(a.get_peer(&d_id).is_none(), "前提：D 那行已经被回收");
    let voucher_c = orbdrop_lib::state::DiscoveredPeer {
        id: c_id.clone(),
        name: "PC-C".into(),
        ip: lan.clone(),
        port: port_c,
        cidr: None,
        relay: true,
    };
    a.absorb_view(&c_id, &[t1_cidr.clone()], true, &voucher_c, &[]);
    assert!(
        a.pool_for(&t1_cidr)
            .iter()
            .any(|e| e.id == d_id && !e.ip.is_empty() && e.port != 0),
        "一次快照里没有它 ≠ 它没了：D 的最后已知地址要留在记忆里"
    );
    a.clear_relay(&t2_id);
    a.relay_attempts.lock().remove(&t2_id);
    // C 也关掉中转开关：`stop_server` 是优雅关闭，同进程里已建立的 keep-alive 连接仍会被
    // 服务（夹具产物），关掉开关才有一个确定的 403 —— 与上面那条"锚点不再供表"同一个做法。
    c.config.lock().allow_relay = false;
    // 中转方那边同一位请求方有 3 秒冷却（`relay::CALLER_COOLDOWN`）：上面刚借过一次，
    // 这里等一下，让这一次真的走到"重新借"那一步（这一步要验的正是它）。
    tokio::time::sleep(Duration::from_secs(4)).await;
    let opened = orbdrop_lib::relay::ensure_for(&a, &t2_id)
        .await
        .expect("来回切换之后，记忆里那台还要借得到");
    assert_eq!(opened.via, d_id, "接手的还是 D（C 已经离线）");
    assert_eq!(
        probe_endpoint_bytes(&opened.host, opened.port, b"still-through-d").await,
        b"still-through-d",
        "再切一次之后字节照样搬到位"
    );

    credential::remove_peer(&c_id).ok();
    server::stop_server(&a).await;
    server::stop_server(&d).await;
    echo1_task.abort();
    echo2_task.abort();
    let _ = port_c;
}

// ---------------------------------------------------------------------------
// 5. 前置：三处报文自报网段与中转能力（§5）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_reports_nets_and_per_peer_cidr() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let server_id = format!("disco-{tag}");
    let member_id = format!("member-{tag}");
    let far_id = format!("far-{tag}");

    let st = make_state(4780, &server_id, "DISCO");
    st.config.lock().allow_relay = true;
    let port = server::start_server(st.clone(), 4780).await.unwrap();

    // 一台同段的设备（自报允许中转：本机探到过）+ 一台"跨段、算不出它属于哪一段"的设备。
    st.add_peer_book(peer(&member_id, "MEMBER", &lan, 3890));
    st.set_relay_capable(&member_id, true);
    st.add_peer_book(peer(&far_id, "FAR", "127.0.0.1", 3890));

    let client = orbdrop_lib::net::client();

    // `/pair/info`：公开、零请求就报自己的段与中转能力（跨段那一侧没有 mDNS，只能靠这一问）。
    let info: serde_json::Value = client
        .get(&format!("https://{lan}:{port}/pair/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["relay"], true, "中转能力位");
    let nets = orbdrop_lib::subnet::parse_list(info["nets"].as_str().expect("nets"));
    assert!(!nets.is_empty(), "本机要自报网段: {info}");
    assert!(
        nets.iter().any(|c| c.contains(lan.parse().unwrap())),
        "自报的段必须覆盖自己的地址: {nets:?}"
    );

    // `/discover`：回话带 `nets` 与 `ver`，表里每条带 `cidr` 与 `relay`。
    let probe_id = format!("probe-{tag}");
    let token = format!("tok-{tag}");
    credential::set_peer(&probe_id, &token).unwrap();
    let disc: serde_json::Value = client
        .get(&format!("https://{lan}:{port}/discover"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(disc["ver"], 2, "F-107 之后这张表的版本");
    assert!(
        !orbdrop_lib::subnet::parse_list(disc["nets"].as_str().unwrap_or_default()).is_empty(),
        "回话要带自己的段"
    );
    let expected_cidr = orbdrop_lib::subnet::local_net_of(lan.parse().unwrap())
        .map(|c| c.to_cidr_string())
        .expect("本机网段");
    let rows = disc["peers"].as_array().unwrap();
    let member = rows
        .iter()
        .find(|p| p["id"] == member_id.as_str())
        .expect("表里有那台设备");
    assert_eq!(member["relay"], true, "它允不允许中转要在表里（池靠它筛）");
    assert_eq!(
        member["cidr"], expected_cidr,
        "这一段是哪一段由**报出方**填"
    );
    let far = rows
        .iter()
        .find(|p| p["id"] == far_id.as_str())
        .expect("表里也有那台");
    assert!(
        far["cidr"].is_null(),
        "算不出归属段时省略这一格（老对端读成未知，本机退回按自己的掩码算）"
    );

    credential::remove_peer(&probe_id).ok();
    server::stop_server(&st).await;
}

// ---------------------------------------------------------------------------
// 6. 收到反向记录之后：按记录里的池借到中转（接收方对那一段没有 mDNS 视野）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remembered_pool_from_a_route_record_opens_a_relay() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let (b_id, c_id) = (format!("phone-b-{tag}"), format!("pc-c-{tag}"));
    let t_id = format!("phone-a-{tag}");

    // B：收到记录的那一侧；C：记录里那个"中转方"，B **从没跟它配对过**、也不和它同 L2。
    let b = make_state(4760, &b_id, "PHONE-B");
    let c = make_state(4770, &c_id, "PC-C");
    c.config.lock().allow_relay = true;
    let (echo, echo_task) = echo_server().await;
    let Some(own_net) = orbdrop_lib::subnet::host_nets()
        .first()
        .map(|cidr| cidr.to_cidr_string())
    else {
        return;
    };
    let port_b = server::start_server(b.clone(), 4760).await.unwrap();
    let port_c = server::start_server(c.clone(), 4770).await.unwrap();

    // 中转方认识目标（够得着它）；B 只认识"同段但够不着"的那个地址。
    c.add_peer_book(peer(&t_id, "A", "127.0.0.1", echo.port()));
    let unreachable = unused_in_subnet(lan.parse().unwrap()).to_string();
    b.add_peer_book(peer(&t_id, "A", &unreachable, echo.port()));

    // B 收到 A 推来的记录（这里直接应用；跨机那条路的鉴权与校验见用例 2）。
    let records = vec![orbdrop_lib::relay::RouteRecord {
        target_cidr: own_net.clone(),
        via_cidr: "10.44.0.0/16".into(),
        via_devices: vec![orbdrop_lib::relay::RouteDevice {
            id: c_id.clone(),
            name: "PC-C".into(),
            ip: lan.clone(),
            port: port_c,
        }],
        via_paired: vec![],
        confirmed_at: None,
    }];
    let (accepted, ignored) = b.apply_relay_routes("phone-a", &records);
    assert_eq!(accepted, vec![own_net.clone()]);
    assert!(ignored.is_empty());

    // B 借记录里的 C（没有凭据、没有 mDNS 视野）→ 端点活着、字节到位。
    let relay = orbdrop_lib::relay::ensure_for(&b, &t_id)
        .await
        .expect("按记住的池要能借到中转");
    assert_eq!(relay.via, c_id);
    assert_eq!(
        probe_endpoint_bytes(&relay.host, relay.port, b"recorded-pool").await,
        b"recorded-pool"
    );
    assert_eq!(
        b.get_peer(&t_id)
            .unwrap()
            .relay
            .as_ref()
            .map(|r| r.via.clone()),
        Some(c_id.clone()),
        "端点写进了那台设备的运行时段（`dial()` 的唯一来源）"
    );
    assert_eq!(
        b.listening_port(),
        port_b,
        "B 自报的端口就是它实际监听的端口"
    );

    server::stop_server(&b).await;
    server::stop_server(&c).await;
    echo_task.abort();
}

// ---------------------------------------------------------------------------
// 7. 切到「离线」= 隐身：替别人开着的中转必须当场收掉
// ---------------------------------------------------------------------------

#[tokio::test]
async fn going_offline_stops_relaying() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let provider_id = format!("off-provider-{tag}");
    let (target_id, requester_id) = (format!("off-target-{tag}"), format!("off-requester-{tag}"));

    let provider = make_state(4790, &provider_id, "PROVIDER");
    provider.config.lock().allow_relay = true;
    let requester = make_state(4795, &requester_id, "REQUESTER");
    let provider_port = server::start_server(provider.clone(), 4790).await.unwrap();
    let requester_port = server::start_server(requester.clone(), 4795).await.unwrap();
    let (echo, echo_task) = echo_server().await;
    provider.add_peer_book(peer(&target_id, "TARGET", "127.0.0.1", echo.port()));

    // 先替别人开一条（未配对请求方：自报身份 + 被核实）。
    let client = orbdrop_lib::net::client();
    let opened: serde_json::Value = client
        .post(&format!("https://{lan}:{provider_port}/relay/open"))
        .json(&serde_json::json!({
            "target": target_id,
            "requester": { "id": requester_id, "port": requester_port },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let host = opened["host"].as_str().expect("host").to_string();
    let relay_port = u16::try_from(opened["port"].as_u64().expect("port")).unwrap();
    assert_eq!(
        probe_endpoint_bytes(&host, relay_port, b"before-offline").await,
        b"before-offline",
        "离线之前这条中转是通的"
    );
    assert!(provider.relays.active() >= 1);

    // 切到「离线」：`apply_online(false)` 做的就是这两步（`set_local_online` + `apply_offline`）。
    provider.set_local_online(false);
    orbdrop_lib::apply_offline(&provider);

    assert_eq!(
        provider.relays.active(),
        0,
        "离线要**当场**收掉全部中转端点"
    );
    let dial_ip = match orbdrop_lib::net::parse_host(&host).expect("端点主机") {
        std::net::IpAddr::V6(v6) => std::net::IpAddr::V4(v6.to_ipv4_mapped().expect("映射地址")),
        other => other,
    };
    let mut closed = false;
    for _ in 0..40 {
        match tokio::net::TcpStream::connect(std::net::SocketAddr::new(dial_ip, relay_port)).await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(
        closed,
        "离线之后端点不该再接受连接（监听随监听任务一起 drop）"
    );

    // 控制面也进不来：隐身由 `presence_middleware` 统一回 503。
    assert_eq!(
        client
            .post(&format!("https://{lan}:{provider_port}/relay/open"))
            .json(&serde_json::json!({
                "target": target_id,
                "requester": { "id": requester_id, "port": requester_port },
            }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "离线 = 隐身：入站请求一律 503"
    );

    server::stop_server(&provider).await;
    server::stop_server(&requester).await;
    echo_task.abort();
}
// ---------------------------------------------------------------------------
// 8. 单向通 → 双向通：被中转的那一侧（地址未知）也要能借回去
// ---------------------------------------------------------------------------

/// 一条中转 TCP 管道（当"中转端点"用：拨它就等于拨 `target`）—— 端点那条路真的把字节搬过去。
async fn tcp_pipe(
    target: std::net::SocketAddr,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    (addr, task)
}

/// `POST /relay/route`：**经它自己点名的中转方**送达的那一档。
///
/// 这是真机上报的那条路：A 经 C 到 B 之后，A 要把"我这一段怎么够得着"告诉 B —— 而这一推
/// 只能经 C（A 够不着 B）。以前自报那一关问的是「TCP 来源地址 + 自报端口」= **C 的地址**，
/// 必然问不出 A 的身份 → 403 → B 永远不知道该经谁回去（"单向通、反向不通"）。
#[tokio::test]
async fn relay_route_accepts_a_record_carried_by_the_relay_it_names() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip() else {
        return;
    };
    let lan = lan.to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let receiver_id = format!("rev-receiver-{tag}");

    let receiver = make_state(4810, &receiver_id, "RECEIVER");
    let receiver_port = server::start_server(receiver.clone(), 4810).await.unwrap();
    let Some(own_net) = orbdrop_lib::subnet::host_nets()
        .first()
        .map(|c| c.to_cidr_string())
    else {
        return;
    };

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan}:{receiver_port}/relay/route");
    let device =
        |id: &str, ip: &str| serde_json::json!({ "id": id, "name": id, "ip": ip, "port": 3890 });
    // 记录里点名的中转方就是**这条连接的来源地址**（TCP 源地址伪造不了）。
    let via_lan = serde_json::json!([device("relay-c", &lan)]);
    let via_other = serde_json::json!([device("relay-c", "10.44.0.7")]);
    let claim = |id: &str| serde_json::json!({ "id": id, "port": receiver_port });

    // 自报的身份**故意问不出来**（`lan:receiver_port` 上答话的是接收方自己）：
    // 来源地址不在记录点名的中转方里 → 照旧 403。
    assert_eq!(
        client
            .post(&url)
            .json(&serde_json::json!({
                "routes": [{
                    "targetCidr": own_net,
                    "viaCidr": "10.44.0.0/16",
                    "viaDevices": via_other,
                    "viaPaired": [],
                }],
                "requester": claim(&format!("impostor-{tag}")),
            }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN,
        "来源地址不是记录点名的中转方时，自报那一关问不出来就该拒"
    );

    // 来源地址 == 记录点名的中转方 → 收下（被中转的那一侧本来就只能经它送过来）。
    let reply: serde_json::Value = client
        .post(&url)
        .json(&serde_json::json!({
            "routes": [{
                "targetCidr": own_net,
                "viaCidr": "10.44.0.0/16",
                "viaDevices": via_lan,
                "viaPaired": [],
            }],
            "requester": claim(&format!("phone-a-{tag}")),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reply["accepted"].as_array().unwrap().len(),
        1,
        "经它自己点名的中转方送来的记录要收下: {reply}"
    );
    assert!(
        receiver.is_ephemeral_route(&own_net),
        "没有 Bearer 的来源只做本进程内的临时提示（不落盘）"
    );
    let route = receiver
        .config
        .lock()
        .relay_route(&own_net)
        .cloned()
        .expect("策略已在内存里（临时提示）");
    assert_eq!(route.via_devices, vec!["relay-c".to_string()]);
    assert_eq!(
        receiver.get_peer("relay-c").map(|p| p.ip),
        Some(lan.clone()),
        "被中转的那一侧要靠它拨得动中转方，所以地址也得并进来"
    );
    assert!(
        receiver
            .pool_for(&own_net)
            .iter()
            .any(|d| d.id == "relay-c"),
        "临时提示也要能用于选路：池必须已经并进来"
    );

    server::stop_server(&receiver).await;
}

/// 目标**没有直连地址**（只经中转见过一面）时：按记住的池借到中转，消息真的送到。
///
/// 两条都是真机上报的那条路的关键一步：
/// - `ensure_for` 以前在"目标没有可拨地址"时当场返回 → 永远停「排队」；
/// - `deliver_http` 以前也要求"它自己的地址"非空 → 借到了端点也送不出去。
#[tokio::test]
async fn addressless_peer_borrows_the_recorded_relay_and_gets_the_message() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip().map(|ip| ip.to_string()) else {
        return;
    };
    let tag = uuid::Uuid::new_v4().to_string();
    let (a_id, b_id, c_id) = (
        format!("phone-a-{tag}"),
        format!("phone-b-{tag}"),
        format!("pc-c-{tag}"),
    );

    // A：目标（真的服务端，收得到会话消息）；B：被中转的那一侧（**只知道 A 的名字**）。
    let a = make_state(4820, &a_id, "PHONE-A");
    let b = make_state(4830, &b_id, "PHONE-B");
    let a_port = server::start_server(a.clone(), 4820).await.unwrap();
    let b_port = server::start_server(b.clone(), 4830).await.unwrap();
    b.add_peer_book(PeerDevice {
        id: a_id.clone(),
        name: "PHONE-A".into(),
        ip: String::new(),
        port: 0,
        online: false,
        ..Default::default()
    });

    // ── 第一段：借到中转（目标地址未知，靠记录里那一池）──────────────────────
    let Some(own_net) = orbdrop_lib::subnet::host_nets()
        .first()
        .map(|c| c.to_cidr_string())
    else {
        return;
    };
    let (echo, echo_task) = echo_server().await;
    let c = make_state(4840, &c_id, "PC-C");
    c.config.lock().allow_relay = true;
    let c_port = server::start_server(c.clone(), 4840).await.unwrap();
    c.add_peer_book(peer(&a_id, "PHONE-A", "127.0.0.1", echo.port()));

    // 还没有任何记录、也没有直连地址：借不到（**同一台目标的 30s 限流**会把这个判定
    // 缓存住，所以这条控制用另一台"不存在"的设备来试，别污染下面那次真借）。
    let phantom = format!("phantom-{tag}");
    b.add_peer_book(PeerDevice {
        id: phantom.clone(),
        name: "PHANTOM".into(),
        ip: String::new(),
        port: 0,
        online: false,
        ..Default::default()
    });
    assert!(
        orbdrop_lib::relay::ensure_for(&b, &phantom).await.is_none(),
        "没有候选时借不到（这条以前也是 None，但那时是「地址空就当场返回」那条捷径）"
    );

    // A 经 C 到过 B 之后推来的记录：B 对那一段没有 mDNS 视野，只认这一份提示。
    let (accepted, _) = b.apply_relay_routes(
        &a_id,
        &[orbdrop_lib::relay::RouteRecord {
            target_cidr: own_net.clone(),
            via_cidr: "10.44.0.0/16".into(),
            via_devices: vec![orbdrop_lib::relay::RouteDevice {
                id: c_id.clone(),
                name: "PC-C".into(),
                ip: lan.clone(),
                port: c_port,
            }],
            via_paired: vec![],
            confirmed_at: None,
        }],
    );
    assert_eq!(accepted, vec![own_net.clone()], "记录含本机地址 → 采纳");

    let relay = orbdrop_lib::relay::ensure_for(&b, &a_id)
        .await
        .expect("目标没有直连地址，也要借得到记录里的中转");
    assert_eq!(relay.via, c_id);
    assert_eq!(
        probe_endpoint_bytes(&relay.host, relay.port, b"addrless").await,
        b"addrless",
        "端点真的把字节搬到目标"
    );

    // ── 第二段：借到的这条路上，会话消息真的送得到（B 从不知道 A 的地址）──────
    let (pipe, pipe_task) = tcp_pipe(format!("{lan}:{a_port}").parse().unwrap()).await;
    b.set_relay(
        &a_id,
        orbdrop_lib::config::RelayAddr {
            host: "127.0.0.1".into(),
            port: pipe.port(),
            via: c_id.clone(),
            via_name: "PC-C".into(),
        },
    );
    let event = orbdrop_lib::ws::events::WsEvent::ChatText {
        id: format!("m-{tag}"),
        text: "回去".into(),
        at: 42,
        from_name: String::new(),
    };
    assert_eq!(
        orbdrop_lib::chat::deliver(&b, &a_id, &event).await,
        orbdrop_lib::chat::Outcome::Delivered,
        "有一条借来的路就该送得到（`dial()` 认的是路，不是它自己的地址）"
    );
    wait_until("A 收到 B 的消息", || {
        a.chat
            .snapshot()
            .convos
            .iter()
            .any(|c| c.peer_id == b_id && !c.messages.is_empty())
    })
    .await;
    assert_eq!(b.listening_port(), b_port, "自报的端口就是实际监听的那个");

    // 路收掉之后又够不着了 —— 这就是"地址未知"那一类对端的真实处境。
    assert!(b.clear_relay(&a_id), "把借来的那条收掉");
    assert_eq!(
        orbdrop_lib::chat::deliver(&b, &a_id, &event).await,
        orbdrop_lib::chat::Outcome::Unreachable,
        "两条路都没有时照旧「够不着」（排队等它上线）"
    );

    server::stop_server(&a).await;
    server::stop_server(&b).await;
    server::stop_server(&c).await;
    echo_task.abort();
    pipe_task.abort();
}

/// 记录一到，"出路"就刚刚出现 —— 排队等它的消息应当**当场**动起来（不必再有人点发送）。
#[tokio::test]
async fn an_arriving_route_record_kicks_the_queued_message_into_borrowing_a_relay() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip().map(|ip| ip.to_string()) else {
        return;
    };
    let tag = uuid::Uuid::new_v4().to_string();
    let (a_id, b_id, c_id) = (
        format!("kick-a-{tag}"),
        format!("kick-b-{tag}"),
        format!("kick-c-{tag}"),
    );

    let b = make_state(4850, &b_id, "PHONE-B");
    let c = make_state(4860, &c_id, "PC-C");
    c.config.lock().allow_relay = true;
    let (echo, echo_task) = echo_server().await;
    // 请求方得有真实监听端口：借中转时**自报**的就是它，中转方会到「来源地址 + 这个端口」
    // 上去核实（`server_port = 0` 会被 401 掉 —— 这也是这条用例第一版失败的原因）。
    let b_port = server::start_server(b.clone(), 4850).await.unwrap();
    let c_port = server::start_server(c.clone(), 4860).await.unwrap();
    c.add_peer_book(peer(&a_id, "PHONE-A", "127.0.0.1", echo.port()));
    b.add_peer_book(PeerDevice {
        id: a_id.clone(),
        name: "PHONE-A".into(),
        ip: String::new(),
        port: 0,
        online: false,
        ..Default::default()
    });

    // 排队一条给它的消息（地址未知、也没有路 → 就是真机上"永远送不到"的那个状态）。
    b.chat.record_outbound(
        &format!("m-{tag}"),
        "救命",
        1,
        &[(a_id.clone(), orbdrop_lib::chat::Delivery::Queued)],
        None,
    );
    assert!(b.chat.has_backlog(&a_id), "这一台欠着一条");

    // 此刻借不到（还没认识任何中转方）—— 这一下**不许**吃掉那 30s 的额度。
    assert!(orbdrop_lib::relay::ensure_for(&b, &a_id).await.is_none());

    let Some(own_net) = orbdrop_lib::subnet::host_nets()
        .first()
        .map(|c| c.to_cidr_string())
    else {
        return;
    };
    let (accepted, _) = b.apply_relay_routes(
        &a_id,
        &[orbdrop_lib::relay::RouteRecord {
            target_cidr: own_net.clone(),
            via_cidr: "10.44.0.0/16".into(),
            via_devices: vec![orbdrop_lib::relay::RouteDevice {
                id: c_id.clone(),
                name: "PC-C".into(),
                ip: lan.clone(),
                port: c_port,
            }],
            via_paired: vec![],
            confirmed_at: None,
        }],
    );
    assert_eq!(accepted, vec![own_net.clone()]);

    // 记录到达就该自己动：`apply_relay_routes` → `flush_all_queued` → `flush_queued`
    // → 够不着 → 当场借一条中转。10s 内借不到就说明"上一发把额度吃掉了"或这条链断了。
    wait_until("记录一到，排队的那台就借到了路", || {
        b.get_peer(&a_id)
            .and_then(|p| p.relay.map(|r| r.via))
            .as_deref()
            == Some(c_id.as_str())
    })
    .await;

    assert_eq!(b.listening_port(), b_port, "自报的端口就是实际监听的那个");
    server::stop_server(&b).await;
    server::stop_server(&c).await;
    echo_task.abort();
}

// ---------------------------------------------------------------------------
// 9. 锚点离线之后，池里那台还得拨得动（真机反馈第五轮）
// ---------------------------------------------------------------------------

/// **池必须自己带地址** —— 锚点不再供表之后，`relayRoutes` 里那台还得借得到。
///
/// 真机现象：与 D1159 配对、经它中转发过消息之后，D1159 离线 → 同段的 D0622（开着 S-09）
/// 从不接手。根因是**池只存 id**（`viaDevices`），拨号时按 id 去设备表里找地址 —— 而那一行
/// 是**锚点带来的**（`learned` 20s / `adopted` 15min）：锚点一离散，行被 `sweep_learned`
/// 收走、活池又被 `sweep_relay_memory` 按"本机认识的设备"清掉，剩下的 id 谁也拨不动。
/// 于是 §8.2 的"C 离线自动换 D"只在锚点掉线的头 20 秒内偶然成立，之后就永远没有候选。
///
/// 这条用例守住修好之后的行为：池与它一起落盘的地址（`RelayRoute.viaAddrs`）来自池自己，
/// 与设备表无关 —— 锚点不再供表、D 那一行已经被回收之后，D 仍然借得到且字节真的到目标。
#[tokio::test]
async fn a_pooled_relay_is_still_dialable_after_its_row_is_reaped() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip().map(|ip| ip.to_string()) else {
        return;
    };
    let unreachable = unused_in_subnet(lan.parse().unwrap()).to_string();
    let tag = uuid::Uuid::new_v4().to_string();
    let (a_id, b_id, c_id, d_id) = (
        format!("phone-a-{tag}"),
        format!("phone-b-{tag}"),
        format!("pc-c-{tag}"),
        format!("pc-d-{tag}"),
    );

    // A：请求方（与锚点 C 配对的那台手机）；C：锚点（D1159 那种）；D：同段的中转方（D0622 那种）。
    let a = make_state(4970, &a_id, "PHONE-A");
    let c = make_state(4972, &c_id, "PC-C");
    let d = make_state(4973, &d_id, "PC-D");
    c.config.lock().allow_relay = true;
    d.config.lock().allow_relay = true;
    let port_a = server::start_server(a.clone(), 4970).await.unwrap();
    let port_c = server::start_server(c.clone(), 4972).await.unwrap();
    let port_d = server::start_server(d.clone(), 4973).await.unwrap();
    assert_eq!(a.listening_port(), port_a);

    // 两台中转方都够得着目标（目标在它们的地址簿里）—— 端点连上它，字节原样搬回。
    let (echo, echo_task) = echo_server().await;
    for st in [&c, &d] {
        st.add_peer_book(peer(&b_id, "PHONE-B", "127.0.0.1", echo.port()));
    }
    // C 的视野里还有同段的 D（允许中转）—— A 的池就是从这份表来的。
    c.add_peer_book(peer(&d_id, "PC-D", &lan, port_d));
    c.set_relay_capable(&d_id, true);

    // A ↔ C 配对（凭据库两边都存）：A 用它做 Bearer，C 用它认人。
    let tok = format!("tok-ac-{tag}");
    credential::set_peer(&c_id, &tok).unwrap();
    credential::set_peer(&a_id, &tok).unwrap();
    a.add_peer_book(PeerDevice {
        paired: true,
        ..peer(&c_id, "PC-C", &lan, port_c)
    });
    // 目标在 A 眼里是"同段、但够不着的地址"（AP 把同段单播隔开就是这样）。
    a.add_peer_book(peer(&b_id, "PHONE-B", &unreachable, echo.port()));

    // A 的中转记忆：经中转成功过一次之后就是这样（目标段 → 经 C 的那一段）。
    let target_cidr = a.cidr_of(&b_id, &unreachable).expect("目标所在的段");
    {
        let mut cfg = a.config.lock();
        cfg.relay_route_mut(&target_cidr).via_cidr = target_cidr.clone();
    }

    // ① 锚点的活视野：C 的 `/discover` 回话里报着同段的 D（允许中转）—— 吸收它就是
    //    `learn_from_peer` 对真回话做的那两件事（`absorb_view` + `learn_peers`）。
    //
    //    这里**故意不真去拨 C**：一个进程里同时跑着好几个 state（同一个 IP、各自一把
    //    证书），而 `net::Trust` 是**进程级、按 IP 索引**的钉表 —— 本测试一旦跟 C 握手，
    //    就会把"C 的指纹"钉到本机这个 IP 上，随后 D 回调核对请求方身份（A，同一个 IP）
    //    必然被指纹校验挡下。真刀真枪的那一跳留给 ④（借 D 并把字节搬到目标）。
    let nets = vec![target_cidr.clone()];
    a.note_peer_nets(&c_id, &nets);
    let voucher = DiscoveredPeer {
        id: c_id.clone(),
        name: "PC-C".into(),
        ip: lan.clone(),
        port: port_c,
        cidr: None,
        relay: true,
    };
    let entries = vec![DiscoveredPeer {
        id: d_id.clone(),
        name: "PC-D".into(),
        ip: lan.clone(),
        port: port_d,
        cidr: Some(target_cidr.clone()),
        relay: true,
    }];
    a.absorb_view(&c_id, &nets, true, &voucher, &entries);
    a.learn_peers(
        &c_id,
        entries
            .iter()
            .map(|e| PeerDevice {
                id: e.id.clone(),
                name: e.name.clone(),
                ip: e.ip.clone(),
                port: e.port,
                online: true,
                ..Default::default()
            })
            .collect(),
    );
    let pool_ids: Vec<String> = a.pool_for(&target_cidr).into_iter().map(|e| e.id).collect();
    assert!(
        pool_ids.contains(&d_id),
        "池里要有 C 报来的 D：{pool_ids:?}"
    );
    assert!(a.get_peer(&d_id).is_some(), "D 这行要并进设备表");

    // ② 经 C 中转发过消息之后，A 的中转记忆就是这样：设备级链路 + 网段级策略，池与地址一起落盘
    //    （`note_relay_success` 就是"经中转端点真的探到目标"那一刻的**唯一写入点**）。
    a.note_relay_success(&b_id, &c_id);
    let route = a
        .config
        .lock()
        .relay_route(&target_cidr)
        .cloned()
        .expect("策略要落库");
    assert!(
        route
            .via_addrs
            .iter()
            .any(|dev| dev.id == d_id && !dev.ip.is_empty() && dev.port != 0),
        "池里 D 的地址要跟着池一起记下来：{:?}",
        route.via_addrs
    );

    // ③ 锚点不再供表：C 关掉中转并停服，D 那行（锚点带来的）到期被回收。
    //    `stop_server` 是**优雅关闭**：同进程里已建立的 keep-alive 连接仍会被服务（夹具产物），
    //    所以先把开关关掉 —— 那一档得到一个明确的 403，选路必须换下一台，断言才确定。
    c.config.lock().allow_relay = false;
    server::stop_server(&c).await;
    a.learned
        .lock()
        .insert(d_id.clone(), Instant::now() - Duration::from_secs(60));
    a.sweep_learned();
    a.sweep_relay_memory();
    assert!(a.get_peer(&d_id).is_none(), "前提：D 这行已经被回收");

    // 真机反馈第七轮的回归：C 停服**前**那一拍的回话里只有它自己（D 当时离线了一会儿）。
    // 记忆必须把 D 留着 —— 池里那台与策略里那份地址都要在（旧实现是整份替换：这一拍就把
    // D 的最后已知地址抹掉，于是"C 离线 D 在线"却谁也拨不动，消息发不出去）。
    a.absorb_view(&c_id, &nets, true, &voucher, &[]);
    assert!(
        a.pool_for(&target_cidr)
            .iter()
            .any(|e| e.id == d_id && !e.ip.is_empty() && e.port != 0),
        "一次快照里没有它 ≠ 它没了：池里要留着 D 的最后已知地址"
    );
    assert!(
        a.config
            .lock()
            .relay_route(&target_cidr)
            .and_then(|r| r.pool_addr(&d_id))
            .is_some(),
        "策略里也要留一份：锚点离散之后拨得动它就靠这个"
    );

    // ④ 这以后还是要借到 D：地址只能来自池自己带的那一份（设备表里已经查不到它）。
    //    **并且先解除与 C 的配对**（真机反馈第七轮追问：解绑之后也要能直接用 D 发消息）——
    //    解绑只清 token 与配对标记，不碰中转记忆。
    a.unpair(&c_id);
    assert!(
        !a.get_peer(&c_id).map(|p| p.paired).unwrap_or(true),
        "前提：C 已经解绑"
    );
    a.relay_attempts.lock().remove(&b_id);
    let opened = orbdrop_lib::relay::ensure_for(&a, &b_id)
        .await
        .expect("锚点不再供表之后，池里那台要还能借");
    assert_eq!(opened.via, d_id, "接手的是池里的 D");
    assert_eq!(
        probe_endpoint_bytes(&opened.host, opened.port, b"through-d").await,
        b"through-d",
        "换的那台同样要把字节搬到目标"
    );

    credential::remove_peer(&c_id).ok();
    credential::remove_peer(&a_id).ok();
    server::stop_server(&a).await;
    server::stop_server(&d).await;
    echo_task.abort();
}

/// 中转方**记住它核实过的目标地址**：下一次同目标的请求不必再带 hint。
///
/// 真机上反方向的第一次借中转就是这么卡住的：请求方手里没有目标的地址（那一侧从来没 mDNS
/// 见过它），中转方也从没"发现"过目标 → 404「本机不认识设备 X」→ 消息永远排队。
#[tokio::test]
async fn a_hint_verified_target_needs_no_hint_next_time() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip().map(|ip| ip.to_string()) else {
        return;
    };
    let tag = uuid::Uuid::new_v4().to_string();
    let provider_id = format!("nh-provider-{tag}");
    let (r1_id, r2_id, t_id) = (
        format!("nh-r1-{tag}"),
        format!("nh-r2-{tag}"),
        format!("nh-t-{tag}"),
    );

    // 目标是一台**真的服务端**：hint 上那一次 `/pair/info` 只有它答得出来。
    let target = make_state(4870, &t_id, "PHONE-T");
    let t_port = server::start_server(target.clone(), 4870).await.unwrap();
    // 中转方**从来没认识过**这台目标（跨段的锚点就是这种：够得着目标，但 mDNS 跨不过路由）。
    let provider = make_state(4880, &provider_id, "PROVIDER");
    provider.config.lock().allow_relay = true;
    let provider_port = server::start_server(provider.clone(), 4880).await.unwrap();
    let r1 = make_state(4885, &r1_id, "R1");
    let r1_port = server::start_server(r1.clone(), 4885).await.unwrap();
    let r2 = make_state(4886, &r2_id, "R2");
    let r2_port = server::start_server(r2.clone(), 4886).await.unwrap();

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan}:{provider_port}/relay/open");

    // ① 带 hint：核实通过 → 开出来，并把目标的地址**记下**（只补空行）。
    let opened: serde_json::Value = client
        .post(&url)
        .json(&serde_json::json!({
            "target": t_id,
            "addr": format!("{lan}:{t_port}"),
            "requester": { "id": r1_id, "port": r1_port },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        opened["targetAddr"].as_str(),
        Some(format!("{lan}:{t_port}").as_str()),
        "回话里要带上目标此刻的地址（请求方靠它补上自己那一行）"
    );
    assert_eq!(
        provider.get_peer(&t_id).map(|p| (p.ip, p.port)),
        Some((lan.clone(), t_port)),
        "核实过的目标地址要留在中转方这边"
    );

    // ② 再来一台请求方，**不带 hint**：中转方自己就该知道目标在哪（以前 404）。
    // 等一下控制面的限流窗口（同一来源地址 3s 内只放一次），别把它误判成"目标解析不了"。
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert_eq!(
        client
            .post(&url)
            .json(&serde_json::json!({
                "target": t_id,
                "requester": { "id": r2_id, "port": r2_port },
            }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK,
        "核实过一次之后，「本机不认识设备 X」这条死路就不该再出现"
    );

    server::stop_server(&target).await;
    server::stop_server(&provider).await;
    server::stop_server(&r1).await;
    server::stop_server(&r2).await;
}

/// 一拍探活就该把"没有任何一条路"的那台救回来：不必等用户先发一条消息。
///
/// 重启之后设备行显示的 `:0` + 离线就是这一档：地址是临时的（mDNS 那条路在这台机器上不通），
/// 路由记忆里却有"这一段该借谁"——探活那一拍试着借一次，借到了地址、端点、状态就都回来了。
#[tokio::test]
async fn a_probe_tick_borrows_a_relay_for_a_peer_with_no_path_at_all() {
    isolate_data_dir();
    let Some(lan) = local_lan_ip().map(|ip| ip.to_string()) else {
        return;
    };
    let tag = uuid::Uuid::new_v4().to_string();
    let (a_id, b_id, c_id) = (
        format!("warm-a-{tag}"),
        format!("warm-b-{tag}"),
        format!("warm-c-{tag}"),
    );

    let b = make_state(4890, &b_id, "PHONE-B");
    let b_port = server::start_server(b.clone(), 4890).await.unwrap();
    let c = make_state(4900, &c_id, "PC-C");
    c.config.lock().allow_relay = true;
    let c_port = server::start_server(c.clone(), 4900).await.unwrap();
    let (echo, echo_task) = echo_server().await;
    c.add_peer_book(peer(&a_id, "PHONE-A", "127.0.0.1", echo.port()));
    // B 那一行：只知道名字（会话信封带来的），没有地址、没有路。
    b.add_peer_book(PeerDevice {
        id: a_id.clone(),
        name: "PHONE-A".into(),
        ip: String::new(),
        port: 0,
        online: false,
        ..Default::default()
    });
    assert!(
        !b.get_peer(&a_id).unwrap().dialable(),
        "起手就是「什么路都没有」"
    );

    let Some(own_net) = orbdrop_lib::subnet::host_nets()
        .first()
        .map(|c| c.to_cidr_string())
    else {
        return;
    };
    let (accepted, _) = b.apply_relay_routes(
        &a_id,
        &[orbdrop_lib::relay::RouteRecord {
            target_cidr: own_net.clone(),
            via_cidr: "10.44.0.0/16".into(),
            via_devices: vec![orbdrop_lib::relay::RouteDevice {
                id: c_id.clone(),
                name: "PC-C".into(),
                ip: lan.clone(),
                port: c_port,
            }],
            via_paired: vec![],
            confirmed_at: None,
        }],
    );
    assert_eq!(accepted, vec![own_net.clone()]);

    // 一拍探活（没有排队消息、也没人点发送）：那一台就该被借回来。
    orbdrop_lib::state::probe_peers_once(&b).await;
    wait_until("探活那一拍自己借到了路", || {
        b.get_peer(&a_id)
            .and_then(|p| p.relay.map(|r| r.via))
            .as_deref()
            == Some(c_id.as_str())
    })
    .await;
    assert_eq!(
        b.get_peer(&a_id).map(|p| p.ip),
        Some("127.0.0.1".to_string()),
        "借到路的同时把目标地址也补上了（设备行不再显示 `:0`）"
    );
    assert_eq!(b.listening_port(), b_port, "自报的端口就是实际监听的那个");

    server::stop_server(&b).await;
    server::stop_server(&c).await;
    echo_task.abort();
}
