//! 凭据文件（vault）的两条关键路径：老系统凭据库的**迁移**、以及**一键清空**（F-15）。
//!
//! 两个测试都跑在隔离环境里，**不碰用户真实数据**：
//! - vault 路径靠 `ORBDROP_DATA_DIR` 指到临时目录（`config::data_dir` 的唯一出口）；
//! - 迁移测试用一次性的 keyring 服务名跑**同一条**迁移路径（`migrate_legacy` 的那两个
//!   参数就是为它留的），不去碰 `orbdrop_peers` 那一条；
//! - 清空测试必然会碰到"老版本留在系统凭据库里的那一条"（`credential::wipe` 顺带清它），
//!   所以它先把那条的**原始字节**备份下来，测试结束（含 panic 展开）原样写回。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use orbdrop_lib::bc;
use orbdrop_lib::config::{Config, PeerDevice};
use orbdrop_lib::credential;
use orbdrop_lib::state::AppState;

/// 老版本那条凭据的坐标（`credential.rs` 里的同名常量是私有的，测试照着写一份）。
const LEGACY_SERVICE: &str = "orbdrop_peers";
const LEGACY_USER: &str = "peers";

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
        auto_connect: false, paused: false,
    }
}

/// 老条目（`orbdrop_peers`）的备份：`credential::wipe` 会顺手删掉它，
/// 测试必须原样放回去 —— 那是用户真实的配对凭据。
struct LegacyEntryGuard {
    existed: bool,
    blob: Vec<u8>,
}

impl LegacyEntryGuard {
    fn take() -> Self {
        let entry = keyring::Entry::new(LEGACY_SERVICE, LEGACY_USER).unwrap();
        match entry.get_secret() {
            Ok(blob) => Self { existed: true, blob },
            Err(_) => Self { existed: false, blob: Vec::new() },
        }
    }
}

impl Drop for LegacyEntryGuard {
    fn drop(&mut self) {
        let entry = keyring::Entry::new(LEGACY_SERVICE, LEGACY_USER).unwrap();
        if self.existed {
            let _ = entry.set_secret(&self.blob);
        } else {
            let _ = entry.delete_credential();
        }
    }
}

/// 迁移：老条目（`set_password` 写的 JSON；Windows 上落成 UTF-16LE）→ vault 文件，
/// 且**老条目必须被删掉**（文件成为唯一事实来源）。顺序反了就会丢配对。
#[test]
fn legacy_credential_entry_migrates_into_the_vault() {
    let service = format!("orbdrop_vault_test_{}", uuid::Uuid::new_v4());
    let entry = keyring::Entry::new(&service, "peers").unwrap();
    let json = r#"{"device-a":"tok-a","device-b":"tok-b"}"#;
    entry.set_password(json).unwrap();
    assert!(entry.get_secret().is_ok(), "老条目应当已经写进系统凭据库");

    let dir = temp_dir("migrate");
    let vault = dir.join("credentials.dat");
    let moved = credential::migrate_legacy(&service, "peers", &vault)
        .unwrap()
        .expect("老条目里的映射应当被搬出来");
    assert_eq!(moved.len(), 2);

    let text = std::fs::read_to_string(&vault).unwrap();
    assert!(
        !text.contains("ODBF") && !text.contains("177plus"),
        "写出的文件不该有可读的版本头/署名:\n{text}"
    );
    assert_eq!(credential::parse_vault(&text).unwrap(), moved, "写出的文件必须能被正常读到");

    assert!(
        matches!(entry.get_secret(), Err(keyring::Error::NoEntry)),
        "迁移完成后老凭据必须被删掉"
    );
    assert!(
        credential::migrate_legacy(&service, "peers", &vault).unwrap().is_none(),
        "没有老条目时迁移什么都不做"
    );

    let _ = entry.delete_credential();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 一键清空：vault 文件、地址簿里的配对行、以及本机写进 BC 的 WebDAV profile 一起清；
/// 用户**自己**在 BC 里建的 profile 一个都不能动（判定依据只有 `Username`）。
#[test]
fn wipe_clears_vault_address_book_and_our_bc_profiles_only() {
    let legacy = LegacyEntryGuard::take();
    let data_dir = temp_dir("data");
    // 必须在任何 `load_peers` 之前落地一份 vault：否则 `AppState::new` 会读不到文件而去
    // 迁移**真实的**系统凭据库那一条（那是用户的数据）。
    std::env::set_var("ORBDROP_DATA_DIR", &data_dir);

    let seeded: HashMap<String, String> = [
        ("device-a".to_string(), "tok-a".to_string()),
        ("device-b".to_string(), "tok-b".to_string()),
    ]
    .into_iter()
    .collect();
    credential::save_peers(&seeded).unwrap();
    assert!(credential::vault_path().is_file());

    // 状态与 profile 写入必须是**同一个** BC 目录，否则 prune 剪的不是这一份。
    let bc_exe = hermetic_bc_exe();
    let bc_dir = bc_exe.parent().unwrap().to_path_buf();
    let state = make_state("vault-test-own", bc_exe);
    state.add_peer_book(peer("device-a", "127.0.0.1", 3901));
    state.add_peer_book(peer("device-b", "127.0.0.1", 3902));
    assert_eq!(state.config.lock().peers.len(), 2);

    bc::upsert_webdav_profile(&bc_dir, "127.0.0.1:3901", "http://127.0.0.1:3901/dav/", bc::DAV_USER, "tok-a")
        .unwrap();
    bc::upsert_webdav_profile(&bc_dir, "webdav.example:80", "http://webdav.example/dav/", "alice", "alice-secret")
        .unwrap();

    let cleared = orbdrop_lib::wipe_all_credentials(&state).unwrap();
    assert_eq!(cleared, 2);

    assert!(!credential::vault_path().exists(), "vault 文件应当被删掉");
    assert!(!credential::has_peer("device-a") && !credential::has_peer("device-b"));
    assert!(state.config.lock().peers.is_empty(), "地址簿里的配对行应当清掉");

    let text = std::fs::read_to_string(bc_dir.join("BCProfiles.xml")).unwrap();
    assert!(!text.contains(bc::DAV_USER), "我们写下的 profile 应当被剪掉:\n{text}");
    assert!(text.contains("alice"), "用户手建的 profile 一个都不能动:\n{text}");

    drop(legacy);
    let _ = std::fs::remove_dir_all(&data_dir);
}