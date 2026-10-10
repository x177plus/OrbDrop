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

use super::*;

/// 夹具所在的目录（`src-tauri/tests/fixtures/`）。
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/update-payload.json");

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

    let wrong_hash = Artifact { sha256: "00".repeat(32), ..good.clone() };
    assert!(verify_file(&path, &wrong_hash, true).is_err(), "sha256 不符要拒");

    let wrong_size = Artifact { size: good.size + 1, ..good.clone() };
    assert!(verify_file(&path, &wrong_size, true).is_err(), "体积不符要拒");

    let no_sig = Artifact { sig: None, ..good.clone() };
    assert!(verify_file(&path, &no_sig, true).is_err(), "缺签名要拒");
    // 安卓端不要求签名（安装时的把关在系统的证书校验上），缺签名照样能过。
    verify_file(&path, &no_sig, false).expect("不要求签名时不该因此失败");

    let _ = std::fs::remove_file(&path);
}

/// **发布脚本与客户端之间的接缝**：`scripts/release.mjs` 写出来的清单必须能被客户端解析。
///
/// 这两半是分别写的（脚本在 Node 侧、解析在 Rust 侧），夹具是发布脚本在
/// `--local --skip-build --version 0.0.1` 下**真实产出**的那一份，拷进来没改过。
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

    let art = m.artifact_for_this_platform().expect("清单里要有本平台的产物");
    assert!(art.size > 0, "产物体积要写上");
    assert_eq!(art.sha256.len(), 64, "sha256 要写全");
    assert!(art.sig.is_some(), "桌面端产物必须带签名（没有签名客户端会拒装）");
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
    assert_eq!(parse_version("0.0.2").unwrap(), parse_version("v0.0.2").unwrap());
    assert_eq!(parse_version(" v0.0.2 ").unwrap().to_string(), "0.0.2");
    assert!(parse_version("最新版").is_err());
}

#[test]
fn host_extraction() {
    assert_eq!(host_of("https://gitee.com/a/b"), "gitee.com");
    assert_eq!(host_of("http://127.0.0.1:8788/x"), "127.0.0.1");
    assert_eq!(host_of("不是一个地址"), "");
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