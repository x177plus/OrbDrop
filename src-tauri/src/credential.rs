use std::collections::HashMap;

use anyhow::{Context, Result};
use parking_lot::Mutex;

/// Service name used as the keyring entry identifier.
const KEYRING_SERVICE: &str = "orbdrop_peers";

/// The whole peer map lives in a single keyring entry, so every mutation is a
/// read-modify-write of that blob. Serialize them: two pairing flows finishing
/// at the same moment (two tests in one process, or a pairing racing an unpair)
/// would otherwise each write back a map built from the same stale read and
/// silently drop the other's token.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Save peer token map to system credential store.
pub fn save_peers(peers: &HashMap<String, String>) -> Result<()> {
    let json = serde_json::to_string(peers).context("Failed to serialize peer map")?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, "peers")
        .map_err(|e| anyhow::anyhow!("keyring entry creation failed: {e}"))?;
    entry
        .set_password(&json)
        .map_err(|e| anyhow::anyhow!("keyring set_password failed: {e}"))?;
    Ok(())
}

/// Load peer token map from system credential store.
/// Returns an empty map if no entry exists.
pub fn load_peers() -> Result<HashMap<String, String>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, "peers")
        .map_err(|e| anyhow::anyhow!("keyring entry creation failed: {e}"))?;
    match entry.get_password() {
        Ok(json) => {
            serde_json::from_str(&json).context("Failed to parse peer map from keyring")
        }
        Err(keyring::Error::NoEntry) => Ok(HashMap::new()),
        Err(e) => Err(anyhow::anyhow!("keyring get_password failed: {e}")),
    }
}

/// Set (insert or update) a single peer's token.
pub fn set_peer(id: &str, token: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = load_peers()?;
    peers.insert(id.to_string(), token.to_string());
    save_peers(&peers)
}

/// Remove a single peer's token from the stored map.
pub fn remove_peer(id: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = load_peers()?;
    peers.remove(id);
    save_peers(&peers)
}

/// Get token for a peer, if any.
pub fn get_peer_token(id: &str) -> Result<Option<String>> {
    let peers = load_peers()?;
    Ok(peers.get(id).cloned())
}

/// Check if a specific peer is paired (has a stored token).
pub fn has_peer(id: &str) -> bool {
    load_peers()
        .map(|p| p.contains_key(id))
        .unwrap_or(false)
}