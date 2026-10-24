//! 镜像条目的**身份 blob** 与本地路径映射。
//!
//! 身份 blob 是 CFAPI 的占位符里我们自己的不透明数据：建占位符时写进去，平台在每次回调里
//! 原样交回来。它必须自足 —— 回调拿到的只有这个 blob 和一条绝对路径，**不能**靠"从本地路径
//! 反推远端坐标"（路径经过了清洗，反推会和真实路径不一致）。
//!
//! 上限是平台的硬约束：`CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH` = 4 KiB。

use std::path::{Path, PathBuf};

use crate::peer_fs::plain_component;

/// 平台允许的身份 blob 上限（`CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH`）。
pub const IDENTITY_MAX: usize = 4096;

/// 当前 blob 版本。字段只增不改；读到不认识的版本一律拒绝（宁可放弃这一项，也不猜）。
const VERSION: u8 = 1;

/// 一项在镜像里的坐标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub peer_id: String,
    pub share_id: String,
    /// 共享项内相对路径（`/` 分隔；`""` = 共享项根）
    pub path: String,
    pub is_dir: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    v: u8,
    peer: String,
    share: String,
    path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    dir: bool,
}

impl Identity {
    /// 编码成存进占位符的字节。超上限返回 Err —— 与其让平台拒绝，不如在这里说清楚。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let wire = Wire {
            v: VERSION,
            peer: self.peer_id.clone(),
            share: self.share_id.clone(),
            path: self.path.clone(),
            dir: self.is_dir,
        };
        let bytes = serde_json::to_vec(&wire).map_err(|e| format!("身份编码失败: {e}"))?;
        if bytes.len() > IDENTITY_MAX {
            return Err(format!(
                "远端路径太长，身份 blob 超出平台上限（{} 字节）",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    /// 解码回调交回来的字节。
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let wire: Wire = serde_json::from_slice(bytes).map_err(|e| format!("身份解析失败: {e}"))?;
        if wire.v != VERSION {
            return Err(format!("身份版本不认识: {}", wire.v));
        }
        if !crate::ids::is_segment(&wire.peer) || !crate::ids::is_segment(&wire.share) {
            return Err("身份里的设备/共享项标识不合法".into());
        }
        Ok(Self {
            peer_id: wire.peer,
            share_id: wire.share,
            path: wire.path,
            is_dir: wire.dir,
        })
    }
}

/// 某个对端某个共享项的镜像目录：`<镜像根>/<peer>/<share>`。
///
/// 用 id 而不是设备名/共享名做目录名：那些名字会带空格、斜杠、emoji，清洗之后还会重名；
/// 用户可见的名字由**拖出去那一项**自己带（见 [`child_path`]）。
///
/// **两个 id 都是对端自报的**（设备 id 来自配对/mDNS，共享项 id 来自对端 `/shares`），所以
/// 先过 [`crate::ids`] 的白名单再拼：`Path::join` 本身会照单全收 `..` 与绝对路径
/// （`root.join("C:\\Windows")` = `C:\Windows`），不过白名单就等于让对端决定本机往哪写。
/// 拼完再断言一次落点仍在 `root` 之下 —— 万一哪天白名单被放宽，这里会先红。
pub fn share_dir(root: &Path, peer_id: &str, share_id: &str) -> Result<PathBuf, String> {
    let peer = crate::ids::segment(peer_id)
        .ok_or_else(|| format!("对端设备标识不合法（不能用作镜像目录）: {peer_id:?}"))?;
    let share = crate::ids::segment(share_id)
        .ok_or_else(|| format!("共享项标识不合法（不能用作镜像目录）: {share_id:?}"))?;
    let dir = root.join(peer).join(share);
    if !crate::unsendable::is_equal_or_under(&dir, root) {
        return Err(format!("镜像落点越界: {}", dir.display()));
    }
    Ok(dir)
}

/// 镜像里一段子的落点：单段名字必须先过 [`plain_component`]。
///
/// 与平台的约束正好对上：`CF_PLACEHOLDER_CREATE_INFO.RelativeFileName` **只能是单段名字**，
/// 所以这条路也只做一段。
pub fn child_path(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let clean = plain_component(name).ok_or_else(|| format!("这一项的名字本机用不了: {name:?}"))?;
    Ok(parent.join(clean))
}

/// 一段相对路径（`/` 分隔）逐段清洗后拼到 `base` 下。空段与非法段按调用方的要求处理。
pub fn join_remote(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut path = base.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        path = child_path(&path, part)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn id() -> Identity {
        Identity {
            peer_id: "peer-a".into(),
            share_id: "share-1".into(),
            path: "子目录/a.txt".into(),
            is_dir: false,
        }
    }

    #[test]
    fn identity_round_trips() {
        let bytes = id().encode().unwrap();
        assert!(bytes.len() < IDENTITY_MAX);
        assert_eq!(Identity::decode(&bytes).unwrap(), id());
    }

    #[test]
    fn directory_flag_survives_round_trip() {
        let dir = Identity {
            is_dir: true,
            path: "视频/2024".into(),
            ..id()
        };
        assert_eq!(Identity::decode(&dir.encode().unwrap()).unwrap(), dir);
    }

    #[test]
    fn oversized_identity_is_refused_here_not_by_the_platform() {
        // 4 KiB 上限：超了要在编码这侧就报错，别让平台用一句模糊的 HRESULT 打回来。
        let long = Identity {
            path: "x".repeat(IDENTITY_MAX),
            ..id()
        };
        let err = long.encode().unwrap_err();
        assert!(err.contains("上限"), "{err}");
    }

    #[test]
    fn unknown_version_and_garbage_are_refused() {
        assert!(
            Identity::decode(b"{\"v\":99,\"peer\":\"a\",\"share\":\"b\",\"path\":\"\"}").is_err()
        );
        assert!(Identity::decode(b"not json").is_err());
        assert!(
            Identity::decode(b"{\"v\":1,\"peer\":\"\",\"share\":\"b\",\"path\":\"\"}").is_err()
        );
    }

    #[test]
    fn share_dir_is_id_based() {
        let root = Path::new(r"C:\mirror");
        assert_eq!(
            share_dir(root, "peer-a", "share-1").unwrap(),
            Path::new(r"C:\mirror\peer-a\share-1")
        );
    }

    /// 两个 id 都由对端自报：它们必须被当成**单一路径段**，`..` / 绝对路径一个都不能过。
    ///
    /// 这是 P0 那条的回归：修复前 `share_dir(root, "..\\..\\Users", "x")` 会返回
    /// `C:\mirror\..\..\Users\x`，`create_dir_all` 照建、`place_same_name` 照删。
    #[test]
    fn share_dir_refuses_traversal_and_absolute_ids() {
        let root = Path::new(r"C:\mirror");
        for (peer, share) in [
            (r"..\..\Users\victim", "share-1"),
            ("..", ".."),
            ("peer-a", r"..\..\Windows"),
            (r"C:\Users\victim", "share-1"),
            ("peer/a", "share-1"),
            ("peer-a", "share.1."),
            ("", "share-1"),
        ] {
            let err = share_dir(root, peer, share).unwrap_err();
            assert!(
                err.contains("不合法"),
                "应当按标识不合法拒绝（{peer:?}, {share:?}），实际: {err}"
            );
        }
    }

    #[test]
    fn child_names_are_sanitized_and_traversal_is_refused() {
        let base = Path::new(r"C:\mirror\p\s");
        assert_eq!(child_path(base, "a.txt").unwrap(), base.join("a.txt"));
        // 对端可能是 macOS：名字里带 `:`，本机不接受
        assert_eq!(child_path(base, "a:b.txt").unwrap(), base.join("a_b.txt"));
        // 越界三段：空、`.`、`..` 全部不许落盘
        assert!(child_path(base, "").is_err());
        assert!(child_path(base, ".").is_err());
        assert!(child_path(base, "..").is_err());
        assert!(
            child_path(base, "..\\win").is_ok(),
            "反斜杠被清洗成下划线，不再是越界"
        );
    }

    #[test]
    fn remote_relative_paths_are_built_one_component_at_a_time() {
        let base = Path::new(r"C:\mirror\p\s");
        assert_eq!(
            join_remote(base, "子目录/sub/a.txt").unwrap(),
            base.join("子目录").join("sub").join("a.txt")
        );
        assert_eq!(join_remote(base, "").unwrap(), base.to_path_buf());
        assert!(join_remote(base, "../x").is_err());
    }
}
