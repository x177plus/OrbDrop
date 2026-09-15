use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::config_dir;

/// Persisted resume offsets (F-60): invite_id → relative path → safe offset.
///
/// The receiving side owns the resume file; it survives restarts so an
/// interrupted transfer can continue from where it left off.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResumeFile {
    /// invite_id → (relative path → bytes already flushed)
    pub records: HashMap<String, HashMap<String, u64>>,
}

pub struct ResumeStore {
    path: PathBuf,
    inner: Mutex<ResumeFile>,
}

impl ResumeStore {
    pub fn new() -> Self {
        let path = config_dir().join("resume.json");
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<ResumeFile>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    pub fn load(&self, invite_id: &str, file_path: &str) -> u64 {
        self.inner
            .lock()
            .records
            .get(invite_id)
            .and_then(|m| m.get(file_path))
            .copied()
            .unwrap_or(0)
    }

    /// Record a new safe offset. Persists to disk immediately (records are small).
    pub fn save(&self, invite_id: &str, file_path: &str, offset: u64) {
        let mut guard = self.inner.lock();
        let entry = guard.records.entry(invite_id.to_string()).or_default();
        entry.insert(file_path.to_string(), offset);
        self.persist_locked(&guard);
    }

    /// Remove an invite's record once the transfer completes.
    pub fn clear(&self, invite_id: &str) {
        let mut guard = self.inner.lock();
        guard.records.remove(invite_id);
        self.persist_locked(&guard);
    }

    fn persist_locked(&self, guard: &ResumeFile) {
        if let Some(parent) = self.path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!("resume dir create failed: {e}");
                return;
            }
        }
        match serde_json::to_string_pretty(guard) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    warn!("resume write failed: {e}");
                }
            }
            Err(e) => warn!("resume serialize failed: {e}"),
        }
        debug!(path = %self.path.display(), "resume state persisted");
    }
}

impl Default for ResumeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Sanitize a relative path so it cannot escape the inbox dir.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

/// Track progress bookkeeping: returns true when the offset advanced.
pub fn progress_delta(prev: u64, next: u64) -> u64 {
    next.saturating_sub(prev)
}

#[allow(dead_code)]
pub fn log_done(invite_id: &str, file_path: &str, offset: u64) {
    info!(invite_id, file_path, offset, "resume offset updated");
}
