use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::Rng;
use tracing::info;

/// Pending pairing codes (F-12): the 6-digit codes this machine has issued and
/// is currently displaying, each valid for 2 minutes and consumable once.
///
/// Codes are *not* bound to a peer id: possession of the code shown on our
/// screen is the proof, and a peer whose advertised id changed under us (app
/// restarted with a fresh identity, stale discovery entry, address book ghost)
/// would otherwise be locked out with "配对码错误或已过期".
pub struct PairStore {
    pending: Mutex<Vec<PendingPair>>,
}

struct PendingPair {
    code: String,
    created_at: Instant,
}

pub const CODE_TTL: Duration = Duration::from_secs(120);

impl PairStore {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Issue a fresh 6-digit code (dropping everything already expired).
    pub fn create_code(&self) -> String {
        let code: u32 = rand::thread_rng().gen_range(100_000..1_000_000);
        let code = code.to_string();
        let mut pending = self.pending.lock();
        pending.retain(|p| !p.is_expired());
        pending.push(PendingPair { code: code.clone(), created_at: Instant::now() });
        info!(code, "pairing code issued");
        code
    }

    /// True if `code` is one of our live codes; consumes it (single use).
    pub fn verify_code(&self, code: &str) -> bool {
        let mut pending = self.pending.lock();
        pending.retain(|p| !p.is_expired());
        match pending.iter().position(|p| p.code == code) {
            Some(pos) => {
                pending.remove(pos);
                true
            }
            None => false,
        }
    }
}

impl PendingPair {
    fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.created_at) >= CODE_TTL
    }
}

impl Default for PairStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a random 32-byte hex token for peer authentication.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
