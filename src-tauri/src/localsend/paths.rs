//! LocalSend 给的文件名 → 本机落点。
//!
//! LocalSend 的 `fileName` 是**对端给的**，而且允许带 `/`（发文件夹时是相对路径）。
//! 这里把它当作不可信输入：
//! - 只允许 `/` 作为分隔符，拒绝 `\`、NUL、绝对路径；
//! - 逐段过 `resume::sanitize_file_name`（Windows 非法字符、保留设备名、结尾点/空格）；
//! - 最后逐段拼到根下（`safe_join` 的同一套围栏在 `sanitize_file_name` 之后仍能命中）；
//! - 重名时在扩展名前加 ` (n)`，绝不覆盖已有文件。
//!
//! 这是接收侧唯一允许决定落点的地方。`fileId` 永远只是表键，不参与路径。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::transfer::resume;

/// 把 LocalSend 的 `fileName` 净化成**根目录下的相对路径**。
///
/// `None` = 这条文件名不能安全落盘（空、路径穿越、非法分隔符……）。调用方应当整条请求
/// 拒绝，而不是静默改名：静默改名会让对端的清单和落盘对不上。
pub fn safe_relative(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || raw.len() > 4096 || raw.contains('\0') || raw.contains('\\') {
        return None;
    }
    // Windows 盘符 / UNC / 绝对路径：`Path::is_absolute` 在部分形态上不够，先看前缀。
    if raw.starts_with('/') || raw.starts_with('~') {
        return None;
    }
    // 跨平台显式拒绝 `C:/...` / `C:` 这类盘符根（在 Unix 上 `Path::is_absolute` 不认它）。
    // 注意只拒绝“盘符 + 根”，不要误伤 `a:b.txt` 这种普通文件名（它在 Windows 上会被净化为
    // `a_b.txt`，但仍应作为文件收下）。
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let rest = &raw[2..];
        if rest.is_empty() || rest.starts_with('/') {
            return None;
        }
    }
    let mut out = PathBuf::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        let clean = resume::sanitize_file_name(part);
        if clean.is_empty() || clean == "." || clean == ".." {
            return None;
        }
        // `sanitize_file_name` 会把非法字符换掉；换完仍带分隔符的（理论上不会有）一律拒。
        if clean.contains('/') || clean.contains('\\') || clean.contains('\0') {
            return None;
        }
        out.push(clean);
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

/// 在 `root` 下落一个不覆盖已有文件、也不撞 `taken` 的最终路径。
///
/// `taken` 是**这次接收里已经分出去的落点**：同一份清单也可能出现两个同名 `fileName`
/// （LocalSend 多选时带的是裸文件名），只比盘上有没有会让两条选到同一个目标 ——
/// 后一条的上传把前一条截断/覆盖，界面却两条都显示成功。
pub fn unique_path(root: &Path, rel: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let candidate = root.join(rel);
    if !candidate.exists() && !taken.contains(&candidate) {
        return candidate;
    }
    let parent = candidate.parent().unwrap_or(root);
    let stem = candidate
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());
    let ext = candidate
        .extension()
        .map(|s| s.to_string_lossy().into_owned());
    for n in 1..=10_000u32 {
        let name = match &ext {
            Some(ext) if !ext.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{stem} ({n})"),
        };
        let next = parent.join(name);
        if !next.exists() && !taken.contains(&next) {
            return next;
        }
    }
    // 极端情况下（同一目录一万个同名）退回一个不可预测的名字；仍然不覆盖。
    parent.join(format!("{stem}-{}.bin", uuid::Uuid::new_v4().simple()))
}

/// 接收批量文件时的根：与 OrbDrop 推送共用同一套 `device_subdir` / 发送方名净化。
pub fn landing_root(download_dir: &Path, sender_alias: &str, device_subdir: bool) -> PathBuf {
    resume::landing_dir(download_dir, sender_alias, device_subdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_nested_relative_paths() {
        assert_eq!(
            safe_relative("相册/2026/照片.jpg").unwrap(),
            PathBuf::from("相册").join("2026").join("照片.jpg")
        );
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for bad in [
            "",
            "..",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "C:/Windows/x",
            "a\\b",
            "a//b",
            "./a",
        ] {
            assert!(safe_relative(bad).is_none(), "应当拒绝: {bad:?}");
        }
    }

    #[test]
    fn sanitizes_windows_hostile_segments() {
        let rel = safe_relative("a:b/c?d.txt").unwrap();
        assert_eq!(rel, PathBuf::from("a_b").join("c_d.txt"));
        assert!(safe_relative("CON").is_some());
        assert!(safe_relative("CON")
            .unwrap()
            .to_string_lossy()
            .starts_with('_'));
    }

    #[test]
    fn unique_path_does_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("orbdrop-ls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut taken = HashSet::new();
        let first = unique_path(&dir, Path::new("a.txt"), &taken);
        taken.insert(first.clone());
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(&dir, Path::new("a.txt"), &taken);
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "a (1).txt");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 同一份清单里的两个同名条目：盘上还什么都没有，也必须分到两个落点。
    #[test]
    fn duplicate_names_in_one_batch_get_distinct_targets() {
        let dir = std::env::temp_dir().join(format!("orbdrop-ls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut taken = HashSet::new();
        let a = unique_path(&dir, Path::new("报告.pdf"), &taken);
        taken.insert(a.clone());
        let b = unique_path(&dir, Path::new("报告.pdf"), &taken);
        taken.insert(b.clone());
        let c = unique_path(&dir, Path::new("报告.pdf"), &taken);
        assert_eq!(a.file_name().unwrap().to_string_lossy(), "报告.pdf");
        assert_eq!(b.file_name().unwrap().to_string_lossy(), "报告 (1).pdf");
        assert_eq!(c.file_name().unwrap().to_string_lossy(), "报告 (2).pdf");
        assert_eq!(a.parent(), Some(dir.as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }
}