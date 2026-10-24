//! 配对邀请的短认证串（SAS）。
//!
//! 邀请那条路没有共享口令（PAKE 用不上），所以把两端看到的证书指纹压成一串
//! **人能比对**的短码：两端各显示 8 位，用户用眼睛（或电话里念）确认一致再接收。
//!
//! 40 bit（8 × 5 bit）：中间人若在 TLS 中间，两端看到的证书不同 → 短码不同。攻击者
//! 想撞出"两边一样"需要离线生成约 2^40 个证书；邀请只活 60 秒，撞不出来。
//! **6 位字母数字只有约 30 bit，不够**——这正是它不能像 PAKE 的 6 位码那样短的原因。
//!
//! 短码不是秘密，可以公开念出来；它也不派生任何密钥。

use sha2::{Digest, Sha256};

/// Crockford Base32：去掉 `I`/`L`/`O`/`U`，避免 1/I、0/O 这类看错。
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 8 位 Base32（40 bit）+ 中间一个连字符，例如 `K7Q2-9M4X`。
///
/// 角色顺序固定：`fp_a` = 发起方（发邀请的那台），`fp_b` = 应答方（显示通知的那台）。
/// 两端必须用同一顺序，否则没中间人也会对不上。
pub fn invite_sas(invite_id: &str, fp_a: &str, fp_b: &str) -> Option<String> {
    if invite_id.is_empty() || fp_a.is_empty() || fp_b.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"orbdrop-invite-sas-v1\0");
    hasher.update(invite_id.as_bytes());
    hasher.update([0]);
    hasher.update(fp_a.as_bytes());
    hasher.update([0]);
    hasher.update(fp_b.as_bytes());
    let digest = hasher.finalize();

    // 精确取 8 个 5-bit 块（40 bit），不做取模，避免偏置。
    let mut out = String::with_capacity(9);
    let mut acc: u32 = 0;
    let mut bits: u8 = 0;
    for &byte in digest.iter() {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            let shift = bits - 5;
            let idx = ((acc >> shift) & 0x1f) as usize;
            out.push(ALPHABET[idx] as char);
            bits -= 5;
            if out.len() == 4 {
                out.push('-');
            }
            if out.len() == 9 {
                return Some(out);
            }
        }
    }
    // SHA-256 有 256 bit，循环一定在 9 个字符前返回；这里只是给编译器一个出口。
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::invite_sas;

    #[test]
    fn same_inputs_produce_the_same_eight_char_code() {
        let a = invite_sas("inv-1", "aa", "bb").unwrap();
        let b = invite_sas("inv-1", "aa", "bb").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 9, "8 位 + 一个连字符");
        assert_eq!(&a[4..5], "-");
        assert!(a
            .chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_digit() || ('A'..='Z').contains(&c)));
        assert!(!a.contains('I') && !a.contains('L') && !a.contains('O') && !a.contains('U'));
    }

    #[test]
    fn different_invites_or_certificates_change_the_code() {
        let base = invite_sas("inv-1", "aa", "bb");
        assert_ne!(base, invite_sas("inv-2", "aa", "bb"));
        // 中间人只换了其中一端看到的证书 → 两端算出来的必须不同。
        assert_ne!(base, invite_sas("inv-1", "aa", "cc"));
        assert_ne!(base, invite_sas("inv-1", "cc", "bb"));
    }

    #[test]
    fn missing_fingerprints_have_no_code() {
        assert!(invite_sas("inv-1", "", "bb").is_none());
        assert!(invite_sas("inv-1", "aa", "").is_none());
        assert!(invite_sas("", "aa", "bb").is_none());
    }
}
