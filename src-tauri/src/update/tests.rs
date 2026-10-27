//! 更新链路上最容易静默失效的几处的单元测试。
//!
//! 钉的不是"代码长什么样"，而是三件一旦错了就**只在出事那天才发现**的事：
//!
//! 1. **签名格式**：`tauri signer sign` 的产物是"base64 包着 minisign 文本"，校验那侧要
//!    先解 base64 再 `Signature::decode`。谁把这一步改顺手了（比如直接当裸 minisign 文本读），
//!    表现是**所有更新一律装不上**，而这只有真发版时才知道。夹具
//!    `tests/fixtures/update-payload.json(.sig)` 是用真正的私钥签的，钉住这条链路。
//! 2. **篡改必拒**：同一份字节改一个字符就得校验失败（这是"清单被改也换不掉包"的全部依据）。
//! 3. **地址策略**：只放行 https 与环回 http。清单里的地址是远端可控的字符串，这一关松了
//!    就等于允许别人把用户导到任意 http 源上。
//!
//! 第 4 条（[`manifest_endpoints_really_load_over_https`]）默认不跑：它要联网，而 CI / 离线机器
//! 上必然红。它钉的是**只在真机上出事、本地全部绿**的那一格 —— 证书信任库。

use super::*;

/// 夹具所在的目录（`src-tauri/tests/fixtures/`）。
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/update-payload.json"
);

fn fixture_bytes() -> Vec<u8> {
    std::fs::read(FIXTURE).expect("夹具文件不在")
}

fn fixture_sig() -> String {
    std::fs::read_to_string(format!("{FIXTURE}.sig"))
        .expect("夹具签名不在")
        .trim()
        .to_string()
}

#[test]
fn signer_output_verifies() {
    verify_signature(&fixture_bytes(), &fixture_sig()).expect("真实的 signer 产物必须能过");
}

#[test]
fn tampered_payload_is_rejected() {
    let mut bytes = fixture_bytes();
    bytes.push(b' ');
    let err = verify_signature(&bytes, &fixture_sig()).expect_err("改过的字节必须被判死");
    assert!(err.contains("签名"), "报错要指到签名上，实际：{err}");
}

#[test]
fn signature_of_another_file_is_rejected() {
    let other = "{\"note\":\"这不是夹具那份\"}".as_bytes();
    assert!(verify_signature(other, &fixture_sig()).is_err());
}

/// `verify_file` 同时把关 sha256、体积与签名：清单里那三项任一不符都不许装。
#[test]
fn verify_file_checks_hash_size_and_signature() {
    let bytes = fixture_bytes();
    let dir = std::env::temp_dir().join("orbdrop-update-verify-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("payload.json");
    std::fs::write(&path, &bytes).unwrap();

    let good = Artifact {
        size: bytes.len() as u64,
        sha256: hex_lower(&Sha256::digest(&bytes)),
        sig: Some(fixture_sig()),
        mirrors: vec![],
    };
    verify_file(&path, &good, true).expect("完全对得上就得过");

    let wrong_hash = Artifact {
        sha256: "00".repeat(32),
        ..good.clone()
    };
    assert!(
        verify_file(&path, &wrong_hash, true).is_err(),
        "sha256 不符要拒"
    );

    let wrong_size = Artifact {
        size: good.size + 1,
        ..good.clone()
    };
    assert!(
        verify_file(&path, &wrong_size, true).is_err(),
        "体积不符要拒"
    );

    let no_sig = Artifact {
        sig: None,
        ..good.clone()
    };
    assert!(verify_file(&path, &no_sig, true).is_err(), "缺签名要拒");
    // 安卓端不要求签名（安装时的把关在系统的证书校验上），缺签名照样能过。
    verify_file(&path, &no_sig, false).expect("不要求签名时不该因此失败");

    let _ = std::fs::remove_file(&path);
}

/// **发布脚本与客户端之间的接缝**：`scripts/release.mjs` 写出来的清单必须能被客户端解析。
///
/// 这两半是分别写的（脚本在 Node 侧、解析在 Rust 侧），夹具是从发布仓 `update.json` **原样拷来**的
/// 那一份（0.0.2 远端发布真实产出）—— 也就是**发出去的那份清单**（GitHub Release 附件与 Gitee 仓库根
/// 是同一份文件）必须能被客户端解析、地址策略必须放行。
/// 谁把字段名写歪了（`pubDate` ↔ `pub_date`、`windows-x86_64` ↔ `windows-x86_64-nsis`），
/// 这里先红；不然要等到用户点「检查更新」才知道，而那时的报错只会说"清单格式不对"。
#[test]
fn release_script_manifest_parses() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/release-update.json"
    ))
    .expect("夹具不在");
    let m: Manifest = serde_json::from_str(&raw).expect("发布脚本产出的清单必须能被客户端解析");
    parse_version(&m.version).expect("版本号要能被客户端解析");
    assert!(m.version_code > 0, "安卓的安装门槛必须在清单里");

    let art = m
        .artifact_for_this_platform()
        .expect("清单里要有本平台的产物");
    assert!(art.size > 0, "产物体积要写上");
    assert_eq!(art.sha256.len(), 64, "sha256 要写全");
    assert!(
        art.sig.is_some(),
        "桌面端产物必须带签名（没有签名客户端会拒装）"
    );
    assert_eq!(art.mirrors.len(), 2, "两条镜像：GitHub 在前、Gitee 在后");
    for u in &art.mirrors {
        ensure_url_ok(u).expect("清单里的地址必须过地址策略（https 或环回 http）");
    }

    assert!(
        m.android.contains_key("universal") && m.android.contains_key("arm64-v8a"),
        "安卓两个包都要在清单里（通用包 + arm64）"
    );
    assert!(
        m.android.values().all(|a| a.sig.is_none()),
        "安卓不写签名字段：那边的把关在系统安装时的证书校验上"
    );
}

#[test]
fn url_policy_only_allows_https_and_loopback_http() {
    assert!(ensure_url_ok("https://gitee.com/o/r/releases/download/v1/a.exe").is_ok());
    // 本机演练要 http（`scripts/dev-mirror.mjs`），只放行环回。
    assert!(ensure_url_ok("http://127.0.0.1:8788/update.json").is_ok());
    assert!(ensure_url_ok("http://localhost:8788/update.json").is_ok());
    // 其余一律拒：http 之上连"这份清单还是那个清单"都保证不了。
    assert!(ensure_url_ok("http://example.com/a.exe").is_err());
    assert!(ensure_url_ok("ftp://example.com/a.exe").is_err());
    assert!(ensure_url_ok("file:///C:/a.exe").is_err());
    assert!(ensure_url_ok("不是地址").is_err());
}

#[test]
fn version_parsing_tolerates_leading_v() {
    assert_eq!(
        parse_version("0.0.2").unwrap(),
        parse_version("v0.0.2").unwrap()
    );
    assert_eq!(parse_version(" v0.0.2 ").unwrap().to_string(), "0.0.2");
    assert!(parse_version("最新版").is_err());
}

#[test]
fn host_extraction() {
    assert_eq!(host_of("https://gitee.com/a/b"), "gitee.com");
    assert_eq!(host_of("http://127.0.0.1:8788/x"), "127.0.0.1");
    assert_eq!(host_of("不是一个地址"), "");
}

/// **联网的**回归：编译期那两条清单地址，用**客户端自己的** reqwest 拿一遍。
///
/// 为什么值得留着一条要联网的测试：其余测试全在离线夹具与明文回环上跑，而 2026-10-10 真机上
/// 出事的那一格恰恰是"**https 能不能过证书校验**" —— `reqwest` 当时是按 `rustls-tls-no-provider`
/// （= 空信任库）编的，每条 https 都在握手时 `invalid peer certificate: UnknownIssuer`，
/// 而本机演练走明文、发布侧回读走 `curl`（系统信任库），**两边都看不见**。改回那个 feature
/// 的话，只有这一条会红。
///
/// `#[ignore]`：它要真的连公网，离线/CI 环境必然失败，所以不进默认那一轮。
/// 跑法：`cargo test --lib update:: -- --ignored`（真机上验证自更新时按 `docs/update.md` §11 走）。
///
/// **判据与客户端一致：两条里至少一条能拿到**（客户端自己就是"第一个能解析成清单的赢"）。
/// 不要求两条都通 —— 国内直连 github.com 时通时不通是常态，那是网络的事，不该让这条测试变红；
/// 而信任库坏掉时**两条会一起死**，所以它照样抓得住。
#[tokio::test]
#[ignore = "要联网：离线环境必然失败，只在真的验证自更新链路时跑"]
async fn manifest_endpoints_really_load_over_https() {
    crate::net::install_crypto_provider();
    let mut ok = 0;
    let mut failures = Vec::new();
    for url in MANIFEST_ENDPOINTS {
        match fetch_manifest_at(url).await {
            Ok(manifest) => {
                // 拿到的不只是"有响应"：还得是一份能解析、版本号读得出来的清单。
                parse_version(&manifest.version)
                    .unwrap_or_else(|e| panic!("{url} 的版本号读不出来：{e}"));
                ok += 1;
            }
            Err(e) => failures.push(format!("{url}：{e}")),
        }
    }
    assert!(
        ok > 0,
        "两条清单地址一条都没拿到（信任库/网络？）：\n  {}",
        failures.join("\n  ")
    );
    if !failures.is_empty() {
        // 只剩一条通不算失败（就是 GitHub 那种时通时不通），但要知道是哪条、为什么。
        println!("这一条没能拿到（不判失败）：\n  {}", failures.join("\n  "));
    }
}

/// 镜像偏好只影响**顺序**，不会把候选丢掉或凭空加一个。
#[test]
fn pref_reorder_is_a_stable_partition() {
    let urls = vec![
        "https://github.com/o/r/a.exe".to_string(),
        "https://gitee.com/o/r/a.exe".to_string(),
    ];
    let mut first = urls.clone();
    first.retain(|u| host_of(u) == "gitee.com");
    first.extend(urls.iter().filter(|u| host_of(u) != "gitee.com").cloned());
    assert_eq!(first[0], "https://gitee.com/o/r/a.exe");
    assert_eq!(first.len(), urls.len());
}

// ---------------------------------------------------------------------------
// 上限：清单与安装包都不能让"对面随便说个大数"变成进程内存
// ---------------------------------------------------------------------------

/// 一个只答一次的裸 HTTP 服务端。
async fn one_shot_http(response: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf).await;
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    });
    format!("http://{addr}/update.json")
}

/// 清单回话超过 `MANIFEST_MAX_BYTES`：在 `Content-Length` 这一关就拒，不读身体。
#[tokio::test]
async fn manifest_response_is_capped() {
    crate::net::install_crypto_provider();
    let url = one_shot_http(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2097152\r\n\r\n"
            .to_string(),
    )
    .await;
    let err = fetch_manifest_at(&url)
        .await
        .expect_err("超过上限的清单必须拒绝");
    assert!(err.contains("太大"), "报错要指到体积上，实际：{err}");
}

/// 镜像声明 `Content-Length` 大于清单里写的 `size`：在落盘前就换下一条。
#[tokio::test]
async fn download_rejects_a_declared_oversize_body() {
    crate::net::install_crypto_provider();
    let url = one_shot_http(
        "HTTP/1.1 200 OK\r\nContent-Length: 2097152\r\n\r\n".to_string(),
    )
    .await;
    let res = http_client().unwrap().get(&url).send().await.unwrap();
    let dest = std::env::temp_dir().join("orbdrop-download-declared-cap-test.bin");
    let err = stream_to_file(res, &dest, 1024, |_, _| {})
        .await
        .expect_err("比清单大的响应必须中止");
    assert!(err.contains("比清单说的大"), "实际：{err}");
    let _ = std::fs::remove_file(&dest);
}

/// 镜像撒谎（不给 `Content-Length`、用 chunked 发更大的身体）：写盘同样按 `size` 截断。
#[tokio::test]
async fn download_stops_when_the_body_exceeds_the_declared_size() {
    crate::net::install_crypto_provider();
    let chunk = "x".repeat(4096);
    let url = one_shot_http(format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        chunk.len()
    ))
    .await;
    let res = http_client().unwrap().get(&url).send().await.unwrap();
    let dest = std::env::temp_dir().join("orbdrop-download-stream-cap-test.bin");
    let err = stream_to_file(res, &dest, 1024, |_, _| {})
        .await
        .expect_err("超过清单声明的身体必须中止");
    assert!(err.contains("超过清单"), "实际：{err}");
    let len = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    assert!(len <= 1024, "落盘不能超过清单声明的大小（实际 {len}）");
    let _ = std::fs::remove_file(&dest);
}

// ---------------------------------------------------------------------------
// 调度：退避、抖动、跨进程状态
// ---------------------------------------------------------------------------

/// 连续失败按 5m → 30m → 2h 退避，之后回到常态间隔（6 小时封顶）。
#[test]
fn retry_backoff_grows_then_falls_back_to_the_periodic_interval() {
    assert_eq!(retry_delay(1), Duration::from_secs(5 * 60));
    assert_eq!(retry_delay(2), Duration::from_secs(30 * 60));
    assert_eq!(retry_delay(3), Duration::from_secs(2 * 3600));
    assert_eq!(retry_delay(4), CHECK_INTERVAL);
    assert_eq!(retry_delay(99), CHECK_INTERVAL);
}

/// 成功后下一次以**上次成功时刻**为基准：启动时命中缓存不会把周期往后推 6 小时。
#[test]
fn due_after_success_is_anchored_to_the_last_success() {
    let now = 1_700_000_000_000i64;
    let jitter = Duration::from_secs(60);
    let interval = CHECK_INTERVAL.as_millis() as i64;

    let last = now - 20 * 60 * 1000;
    assert_eq!(
        due_after_success(now, Some(last), CHECK_INTERVAL, jitter),
        last + interval + 60_000,
        "20 分钟前成功过，下一次应当仍是那次 + 6h"
    );

    assert_eq!(
        due_after_success(now, None, CHECK_INTERVAL, jitter),
        now + interval + 60_000,
        "从没成功过就从现在起算"
    );

    let future = now + 10 * 24 * 3600 * 1000;
    assert_eq!(
        due_after_success(now, Some(future), CHECK_INTERVAL, jitter),
        now + interval + 60_000,
        "时钟往回跳（上次成功在未来）不能把检查推到更远"
    );

    // 演练间隔（15 秒）不能被 60 秒下限压住：从没成功过 → 约 15 秒后就该查。
    let short = Duration::from_secs(15);
    assert_eq!(
        due_after_success(now, None, short, Duration::from_secs(1)),
        now + 16_000,
        "短间隔演练要真的按短间隔排"
    );
}

/// 抖动落在 `[0, 2 × CHECK_JITTER)` 里。
#[test]
fn jitter_stays_within_the_declared_window() {
    let span = CHECK_JITTER * 2;
    for _ in 0..64 {
        let j = random_jitter(span);
        assert!(j < span, "抖动越界：{j:?}");
    }
}

/// 落盘状态要能原样读回，也要能容忍缺字段（老文件）与多字段（新版留下的）。
#[test]
fn persisted_state_roundtrips_and_tolerates_unknown_fields() {
    let p = Persisted {
        last_success_ms: Some(123),
        latest_version: Some("0.0.3".into()),
        available: Some(PersistedAvailable {
            version: "0.0.3".into(),
            notes: Some("修了几个问题".into()),
            pub_date: Some("2026-10-01T00:00:00Z".into()),
            size: 13_000_000,
        }),
        dismissed_version: Some("0.0.3".into()),
    };
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<Persisted>(&json).unwrap(), p);

    let old: Persisted = serde_json::from_str(r#"{"lastSuccessMs":123}"#).unwrap();
    assert_eq!(old.last_success_ms, Some(123));
    assert!(old.available.is_none() && old.dismissed_version.is_none());

    let newer: Persisted =
        serde_json::from_str(r#"{"lastSuccessMs":1,"somethingNew":true}"#).unwrap();
    assert_eq!(newer.last_success_ms, Some(1));
}

/// 只有远端确实更高时才恢复"待安装"：装上了（或升过头）就把旧记录丢掉。
#[test]
fn restored_available_only_when_it_is_newer_than_the_installed_version() {
    let p = Persisted {
        available: Some(PersistedAvailable {
            version: "0.0.3".into(),
            notes: None,
            pub_date: None,
            size: 1,
        }),
        ..Persisted::default()
    };
    assert!(restored_available(&p, "0.0.2").is_some());
    assert!(restored_available(&p, "0.0.3").is_none());
    assert!(restored_available(&p, "0.1.0").is_none());
}

/// 「稍后」同时改快照与落盘那份，且字段名是前端认的 `dismissedVersion`。
#[test]
fn mark_dismissed_updates_both_copies() {
    let mut c = Ctl::default();
    mark_dismissed(&mut c, "0.2.0".into());
    assert_eq!(c.state.dismissed_version.as_deref(), Some("0.2.0"));
    assert_eq!(c.persisted.dismissed_version.as_deref(), Some("0.2.0"));
    let json = serde_json::to_string(&c.persisted).unwrap();
    assert!(
        json.contains("\"dismissedVersion\":\"0.2.0\""),
        "落盘字段名必须与前端/清单一致：{json}"
    );
}
