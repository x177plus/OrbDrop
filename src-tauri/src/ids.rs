//! 对端自报的**标识符**（设备 id / 共享项 id）的形态校验。
//!
//! 这两个 id 都不受本机控制：设备 id 来自配对请求、配对邀请、mDNS 公告；共享项 id 来自对端的
//! `/shares`。而本机会把它们当成本地路径段用（`mirror::identity::share_dir` 直接
//! `root.join(peer_id).join(share_id)`），也会当 URL 路径段用（BC 的
//! `http://127.0.0.1:<port>/proxy/<peer_id>/`、profile 里的 `<share_id>/<path>`）。
//!
//! `Path::join` 会照单全收 `..` 与绝对路径：`root.join("C:\\Windows")` 就是 `C:\Windows`。
//! 所以"对端自报"必须在这一层收口成**单一安全路径段**：
//!
//! - 只许 `[A-Za-z0-9._-]` —— 这些字符在 Windows 文件名、URL path segment、BC 的
//!   `profile:<名字>?<子路径>` 写法里都安全，不需要任何转义；
//! - 长度 1..=[`MAX_LEN`]；
//! - 不许 `.` / `..`，不许结尾的点（Windows 会吃掉结尾的点，`abc.` 与 `abc` 撞名）。
//!
//! **只拒绝、不改写。** 清洗（例如把 `/` 换成 `_`）会让同一个 id 在不同地方变成不同字符串：
//! 镜像目录、BC profile 的 BaseURL、对端 DAV 的路径必须永远指向同一项。合法值只有一种形状
//! —— 本机自己生成的 UUID v4（`Uuid::new_v4().to_string()`，见 `config::new_device_id` 与
//! `ShareItem::new`），所以这条白名单对真实流量从不拦人。
//!
//! 校验点分三层（缺一层都可能被绕回去）：
//!
//! 1. **落库前**：`pair::commit_pairing_with_fingerprint`（所有配对路径的汇合点）与
//!    `state::AppState::{upsert_peer, add_peer_book, mark_paired}`（mDNS / 转述 / 凭据库合成）；
//!    服务端的四个配对入口先给一条明确的 400，而不是让请求走到落库才失败。
//! 2. **使用前**：`mirror::identity::share_dir`（返回 `Result`，并在拼完之后再断言落点仍在
//!    镜像根下）与 `bc::resolve_side`（这两个 id 会进 URL）。
//! 3. **读回配置时**：`config::load_config_at` 丢掉 id 不合法的地址簿行（手改过 / 旧数据 /
//!    恶意对端写下的），只丢那一行、不影响整份配置。

/// 一个合法标识符的最大长度（字节；白名单只含 ASCII，所以字节数 == 字符数）。
///
/// 真实值远小于这个数（UUID 36），余量只为将来可能出现的其它格式。
pub const MAX_LEN: usize = 128;

/// 是不是一个可以安全当作**单一路径段**的标识符。
pub fn is_segment(s: &str) -> bool {
    segment(s).is_some()
}

/// 同 [`is_segment`]，通过时把原串还回去（省掉调用方的一次 `unwrap`）。
pub fn segment(s: &str) -> Option<&str> {
    if s.is_empty() || s.len() > MAX_LEN || s == "." || s == ".." || s.ends_with('.') {
        return None;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        .then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_and_real_device_ids_pass() {
        assert!(is_segment("7c9e6679-7425-40de-944b-e07fc1f90ae7"));
        assert!(is_segment("peer-a"));
        assert!(is_segment("orbdrop-test-8f14e45f-ceea-467f-a"));
        assert!(is_segment("a.b_c-D9"));
    }

    #[test]
    fn traversal_and_separators_are_rejected() {
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "..\\x",
            "a/b",
            "a\\b",
            "C:\\Windows",
            "/etc/passwd",
            "a\\..\\..\\Users",
            "a:b",
            "a?b",
            "a#b",
            "a%2e%2e",
            "a b",
            "项目",
            "a.",
            ".a..",
            "a\u{0}b",
            "\n",
        ] {
            assert!(!is_segment(bad), "应当拒绝: {bad:?}");
        }
    }

    #[test]
    fn length_is_bounded() {
        assert!(is_segment(&"a".repeat(MAX_LEN)));
        assert!(!is_segment(&"a".repeat(MAX_LEN + 1)));
    }
}
