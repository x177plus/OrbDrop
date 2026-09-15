use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use chrono::{DateTime, Utc};

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DownloadMode {
    Direct,
    AskEachTime,
}

impl Default for DownloadMode {
    fn default() -> Self {
        Self::Direct
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareItem {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    /// 开播开关（F-22）。可见范围不在这里存：广播对象 = **当前**处于长连接的设备，
    /// 由 `Caller::sees_share` 实时判定（F-25）。
    pub broadcast_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDevice {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub online: bool,
    pub paired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub port: u16,
    pub download_dir: PathBuf,
    pub download_mode: DownloadMode,
    pub inbox_dir: PathBuf,
    pub device_name: String,
    pub auto_start: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            port: 3890,
            download_dir: default_download_dir(),
            download_mode: DownloadMode::Direct,
            inbox_dir: default_inbox_dir(),
            device_name: default_device_name(),
            auto_start: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent config (TOML)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_download_dir")]
    pub download_dir: PathBuf,
    #[serde(default)]
    pub download_mode: DownloadMode,
    #[serde(default = "default_inbox_dir")]
    pub inbox_dir: PathBuf,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    #[serde(default)]
    pub shares: Vec<ShareItem>,
    /// Stable device identity advertised via mDNS / used as peer id.
    ///
    /// Every other field is optional on load: a config written by an older
    /// build (or hand-edited) must not reset our identity, or peers accumulate a
    /// fresh row for the "same" device on each restart.
    #[serde(default = "new_device_id")]
    pub device_id: String,
    /// Known devices: manual adds + paired peers (address book, persisted).
    #[serde(default)]
    pub peers: Vec<PeerDevice>,
}

fn default_port() -> u16 {
    3890
}

fn default_auto_start() -> bool {
    true
}

fn new_device_id() -> String {
    Uuid::new_v4().to_string()
}

impl Config {
    pub fn settings(&self) -> AppSettings {
        AppSettings {
            port: self.port,
            download_dir: self.download_dir.clone(),
            download_mode: self.download_mode,
            inbox_dir: self.inbox_dir.clone(),
            device_name: self.device_name.clone(),
            auto_start: self.auto_start,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3890,
            download_dir: default_download_dir(),
            download_mode: DownloadMode::Direct,
            inbox_dir: default_inbox_dir(),
            device_name: default_device_name(),
            auto_start: true,
            shares: Vec::new(),
            device_id: new_device_id(),
            peers: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Config root: `%APPDATA%/OrbDrop` or `~/Library/Application Support/OrbDrop`
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::config_dir().unwrap_or_else(|| PathBuf::from("C:/Users/Default/AppData/Roaming"))
            });
        base.join("OrbDrop")
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/Library/Application Support"))
            .join("OrbDrop")
    }
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

fn default_download_dir() -> PathBuf {
    dirs::desktop_dir().unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\Desktop"))
}

fn default_inbox_dir() -> PathBuf {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\Downloads"))
        .join("OrbDrop")
}

fn default_device_name() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "OrbDrop-PC".into())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "OrbDrop-Mac".into())
    }
}

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

pub fn load_config() -> Config {
    let path = config_file_path();
    if !path.exists() {
        // First run: keep the generated identity. Peers key their address book
        // and pairing codes off our device id, so it must not be re-minted on
        // every start (that showed up as a duplicate device row per restart).
        let config = Config::default();
        if let Err(e) = save_config_at(&path, &config) {
            eprintln!("Failed to persist initial config: {e}");
        }
        return config;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Failed to parse config, using defaults: {e}");
            // Keep our identity across a corrupt file instead of minting a new
            // device id on each launch — but never silently drop the old file.
            let backup = path.with_extension("toml.bad");
            if let Err(e) = std::fs::rename(&path, &backup) {
                eprintln!("Failed to back up config to {backup:?}: {e}");
            }
            let config = Config::default();
            if let Err(e) = save_config_at(&path, &config) {
                eprintln!("Failed to rewrite config: {e}");
            }
            config
        }),
        Err(e) => {
            eprintln!("Failed to read config, using defaults: {e}");
            Config::default()
        }
    }
}

/// Write a config to an arbitrary path. Used by production code and tests.
pub fn save_config_at(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create config directory")?;
    }
    let content = toml::to_string_pretty(config).context("Failed to serialize config")?;
    std::fs::write(path, content).context("Failed to write config file")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unique file naming (S-01)
// ---------------------------------------------------------------------------

pub fn unique_path(dir: &PathBuf, name: &str) -> PathBuf {
    let target = dir.join(name);
    if !target.exists() {
        return target;
    }
    let stem = match target.extension() {
        Some(ext) => {
            let s = name.strip_suffix(&format!(".{}", ext.to_string_lossy())).unwrap_or(name);
            s.to_string()
        }
        None => name.to_string(),
    };
    let ext = target.extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for i in 1..9999 {
        let candidate = dir.join(format!("{} ({}){}", stem, i, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{} ({}){}", stem, 9999, ext))
}

// ---------------------------------------------------------------------------
// Helpers for Tauri-side share management
// ---------------------------------------------------------------------------

impl ShareItem {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let id = Uuid::new_v4().to_string();
        Self {
            id,
            name,
            path,
            created_at: Utc::now(),
            broadcast_enabled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: save a config with a share, load it back, and check equality.
    /// This is the persistence contract behind "重启后共享项/设置恢复".
    #[test]
    fn config_round_trip_preserves_shares_and_settings() {
        let dir = std::env::temp_dir().join(format!("orbdrop_cfg_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let config = Config {
            port: 3895,
            download_dir: PathBuf::from("C:/test/downloads"),
            download_mode: DownloadMode::AskEachTime,
            inbox_dir: PathBuf::from("C:/test/inbox"),
            device_name: "TestPC".into(),
            auto_start: false,
            device_id: "device-test-1".into(),
            peers: vec![],
            shares: vec![ShareItem {
                id: "share-1".into(),
                name: "Projects".into(),
                path: PathBuf::from("C:/Projects"),
                created_at: Utc::now(),
                broadcast_enabled: true,
            }],
        };

        // Isolate config file writes to the temp dir: write the TOML exactly as
        // save_config would, then parse it back exactly as load_config does.
        let tmp_file = dir.join("config.toml");

        std::fs::write(&tmp_file, toml::to_string_pretty(&config).unwrap()).unwrap();
        let loaded: Config = toml::from_str(&std::fs::read_to_string(&tmp_file).unwrap().as_str()).unwrap();

        assert_eq!(loaded.port, 3895);
        assert_eq!(loaded.download_dir, PathBuf::from("C:/test/downloads"));
        assert_eq!(loaded.download_mode, DownloadMode::AskEachTime);
        assert_eq!(loaded.inbox_dir, PathBuf::from("C:/test/inbox"));
        assert_eq!(loaded.device_name, "TestPC");
        assert_eq!(loaded.auto_start, false);
        assert_eq!(loaded.shares.len(), 1);
        assert_eq!(loaded.shares[0].id, "share-1");
        assert_eq!(loaded.shares[0].name, "Projects");
        assert_eq!(loaded.shares[0].path, PathBuf::from("C:/Projects"));
        assert_eq!(loaded.shares[0].broadcast_enabled, true);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// S-01: same-name downloads get "xxx (1).ext" style renames.
    #[test]
    fn unique_path_appends_counter() {
        let dir = std::env::temp_dir().join(format!("orbdrop_uniq_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing exists → base name.
        let p1 = unique_path(&dir, "report.txt");
        assert_eq!(p1.file_name().unwrap(), "report.txt");

        std::fs::write(&p1, "x").unwrap();

        // Exists → "report (1).txt"
        let p2 = unique_path(&dir, "report.txt");
        assert_eq!(p2.file_name().unwrap(), "report (1).txt");
        std::fs::write(&p2, "x").unwrap();

        // Both exist → "report (2).txt"
        let p3 = unique_path(&dir, "report.txt");
        assert_eq!(p3.file_name().unwrap(), "report (2).txt");

        // No-extension files still get suffixes.
        let p4 = unique_path(&dir, "README");
        assert_eq!(p4.file_name().unwrap(), "README");
        std::fs::write(&p4, "x").unwrap();
        let p5 = unique_path(&dir, "README");
        assert_eq!(p5.file_name().unwrap(), "README (1)");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}