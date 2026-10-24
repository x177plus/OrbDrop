//! 凭据文件（vault）的关键路径：**一键清空**（F-15）要一次清掉三处 —— 凭据文件、
//! 地址簿里的配对行、本机写进 Beyond Compare 的 WebDAV profile。
//!
//! 跑在隔离环境里，**不碰用户真实数据**：vault 路径靠 `ORBDROP_DATA_DIR` 指到临时目录
//! （`config::data_dir` 的唯一出口），BC 用一个"便携版假 exe"把 profile 写进临时目录。
//!
//! 老版本那条系统凭据库的迁移路径已经删掉了（调试机全部迁移完毕），所以这里不再碰
//! 系统凭据库；仍在兼容范围内的只有**文件格式**的升级（v1/v2 → v3），那部分由
//! `credential.rs` 的单元测试覆盖。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use orbdrop_lib::bc;
use orbdrop_lib::config::{Config, PeerDevice};
use orbdrop_lib::credential::{self, PeerCredential};
use orbdrop_lib::state::AppState;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_vault_{tag}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 便携版"假 BeyondCompare"：预写 profile 只写进这个临时目录，不碰用户真实的 BC 配置。
fn hermetic_bc_exe() -> PathBuf {
    let dir = temp_dir("bc");
    std::fs::write(dir.join("BCompare.exe"), b"stub").unwrap();
    std::fs::write(dir.join("BCState.xml"), b"<BCState/>").unwrap();
    dir.join("BCompare.exe")
}

fn make_state(device_id: &str, bc_exe: PathBuf) -> Arc<AppState> {
    let mut config = Config::default();
    config.port = 0;
    config.device_id = device_id.to_string();
    config.device_name = "vault-test".to_string();
    config.download_dir = temp_dir("dl");
    config.bc_exe_path = Some(bc_exe);
    let config_path = temp_dir("cfg").join("config.toml");
    Arc::new(AppState::new(config, config_path))
}

fn peer(id: &str, ip: &str, port: u16) -> PeerDevice {
    PeerDevice {
        id: id.to_string(),
        name: id.to_string(),
        ip: ip.to_string(),
        port,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false,
        relay: None,
    }
}

/// 一键清空：vault 文件、地址簿里的配对行、以及本机写进 BC 的 WebDAV profile 一起清；
/// 用户**自己**在 BC 里建的 profile 一个都不能动（判定依据只有 `Username`）。
///
/// 前半段还走一遍**启动时的脏记录清理**（`credential::drop_own_fingerprint`）：钉在本机
/// 自己那张证书上的对端指纹是错记录（来源见那个函数的说明），要抹掉，其余记录一个字不动。
#[test]
fn wipe_clears_vault_address_book_and_our_bc_profiles_only() {
    let data_dir = temp_dir("data");
    // 必须在任何 `load_peers` 之前把 `ORBDROP_DATA_DIR` 指到临时目录：本机真实那份凭据
    // 就在用户的数据目录里，测试碰它一次就可能把人家的配对清掉。
    std::env::set_var("ORBDROP_DATA_DIR", &data_dir);

    let seeded: HashMap<String, PeerCredential> = [
        ("device-a".to_string(), "tok-a".to_string()),
        ("device-b".to_string(), "tok-b".to_string()),
    ]
    .into_iter()
    .map(|(id, token)| {
        (
            id,
            PeerCredential {
                token,
                fingerprint: "a".repeat(64),
            },
        )
    })
    .collect();
    credential::save_credentials(&seeded).unwrap();
    assert!(credential::vault_path().is_file());

    // —— 启动时的脏记录清理：只有"指纹 = 本机自己"的那条被抹掉 ——
    let own = "f".repeat(64);
    credential::set_peer_credential("device-dirty", "tok-dirty", &own).unwrap();
    assert_eq!(
        credential::drop_own_fingerprint(&own).unwrap(),
        1,
        "本机指纹那条要被认出来"
    );
    assert_eq!(
        credential::get_peer_fingerprint("device-dirty").unwrap(),
        None,
        "抹掉 = 回到「还没钉住」（下一次成功连线按 TOFU 重新认）"
    );
    assert_eq!(
        credential::get_peer_token("device-dirty")
            .unwrap()
            .as_deref(),
        Some("tok-dirty"),
        "清的是指纹那一项，token（这段关系本身）不动"
    );
    assert_eq!(
        credential::get_peer_fingerprint("device-a").unwrap(),
        Some("a".repeat(64)),
        "别人的指纹一个字节都不许动"
    );
    assert_eq!(
        credential::drop_own_fingerprint(&own).unwrap(),
        0,
        "清过之后再跑一次不该有东西可清（启动每次都会调）"
    );
    credential::remove_peer("device-dirty").unwrap();

    // 状态与 profile 写入必须是**同一个** BC 目录，否则 prune 剪的不是这一份。
    let bc_exe = hermetic_bc_exe();
    let bc_dir = bc_exe.parent().unwrap().to_path_buf();
    let state = make_state("vault-test-own", bc_exe);
    state.add_peer_book(peer("device-a", "127.0.0.1", 3901));
    state.add_peer_book(peer("device-b", "127.0.0.1", 3902));
    assert_eq!(state.config.lock().peers.len(), 2);

    bc::upsert_webdav_profile(
        &bc_dir,
        "127.0.0.1:3901",
        "http://127.0.0.1:3901/dav/",
        bc::DAV_USER,
        "tok-a",
    )
    .unwrap();
    bc::upsert_webdav_profile(
        &bc_dir,
        "webdav.example:80",
        "http://webdav.example/dav/",
        "alice",
        "alice-secret",
    )
    .unwrap();

    let cleared = orbdrop_lib::wipe_all_credentials(&state).unwrap();
    assert_eq!(cleared, 2);

    assert!(!credential::vault_path().exists(), "vault 文件应当被删掉");
    assert!(!credential::has_peer("device-a") && !credential::has_peer("device-b"));
    assert!(
        state.config.lock().peers.is_empty(),
        "地址簿里的配对行应当清掉"
    );

    let text = std::fs::read_to_string(bc_dir.join("BCProfiles.xml")).unwrap();
    assert!(
        !text.contains(bc::DAV_USER),
        "我们写下的 profile 应当被剪掉:\n{text}"
    );
    assert!(
        text.contains("alice"),
        "用户手建的 profile 一个都不能动:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}
