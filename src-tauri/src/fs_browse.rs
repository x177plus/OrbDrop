//! 本机文件浏览：推送选择框（`src/lib/components/FilePicker.svelte`）列盘符与目录用。
//!
//! 为什么要自绘选择框：**系统原生选择框在 Windows/Linux 上无法把「选文件」和「选目录」
//! 合进同一个对话框** —— Win32 的 `IFileOpenDialog` 只有 `FOS_PICKFOLDERS` 这一个开关
//! （「offers a choice of folders rather than files」，非此即彼），`rfd` 也因此只提供
//! `pick_file(s) / pick_folder(s)` 四个入口。而推送要的正是「文件 + 文件夹混选、选中的项
//! 还能跨目录累积」。所以选择框画在面板里，这个模块只提供它需要的枚举。
//!
//! 这里要滤掉的只有**三类**（判据全在 `crate::unsendable`，与"发送时会不会跳过"是同一份 ——
//! 列出来却推不动比不列更坏）：
//!
//! 1. **卷根上的系统目录/文件**（`$RECYCLE.BIN`、`System Volume Information`、`pagefile.sys`…）
//!    —— 不是用户放的资料，点进去还多半是「拒绝访问」；
//! 2. **链接**（符号链接 / junction）—— 会回环、会把同一份内容列两遍；
//! 3. **OrbDrop 自己的数据目录** —— 里面装着全部设备的 token。
//!
//! 除此之外**不做可见性过滤**：选择框是"我要推哪些东西"，隐藏项也得能选到（`.git`、
//! `AppData` 都在用户真正想推的范围里）—— 与 `/tree`（对端浏览，跳过点开头的项）不同。

use std::path::{Path, PathBuf};

use serde::Serialize;

/// 「设备和驱动器」里的一项。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsDrive {
    pub name: String,
    pub path: String,
    /// 可移动磁盘（U 盘、移动硬盘）：界面上说「可移动磁盘」而不是「本地磁盘」。
    pub removable: bool,
    /// 总容量 / 可用空间（字节）。取不到（如 macOS 的卷）为 0，界面据此不画占用条。
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// 文件夹固定 0：列一层不读它的子项，「几个文件」要把每个子目录都翻一遍才算得出来。
    pub size: u64,
}

/// 面包屑的一段。`path` 是点它要跳到的绝对路径。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCrumb {
    /// 展示名：Windows 的盘根显示成 `C:`，其余是目录名。
    pub name: String,
    pub path: String,
}

/// 一层目录的内容。
///
/// 面包屑由**后端**算：盘根、分隔符、`C:` 与 `C:\` 的区别这类路径语义只在这里有一份，
/// 前端只管把 `path` 原样送回来。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsListing {
    pub path: String,
    pub crumbs: Vec<FsCrumb>,
    pub entries: Vec<FsEntry>,
}

/// 列举失败。`kind` 决定界面显示哪张状态页；就算失败，面包屑也得是对的（用户要能退回去）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsError {
    /// `denied`（无权访问）/ `missing`（不在了）/ `notDir`（不是目录）/ `other`。
    pub kind: &'static str,
    pub message: String,
    pub crumbs: Vec<FsCrumb>,
}

impl FsError {
    fn plain(message: String) -> Self {
        Self { kind: "other", message, crumbs: Vec::new() }
    }

    fn from_io(e: &std::io::Error, crumbs: &[FsCrumb]) -> Self {
        Self { kind: kind_of(e), message: e.to_string(), crumbs: crumbs.to_vec() }
    }
}

/// `io::Error` → 界面状态页用的四种之一。
fn kind_of(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "denied",
        std::io::ErrorKind::NotFound => "missing",
        std::io::ErrorKind::NotADirectory => "notDir",
        _ => "other",
    }
}

/// 从任意一段路径往回叠出面包屑；顺序是从盘根到它自己。
fn crumbs_of(path: &Path) -> Vec<FsCrumb> {
    let mut out: Vec<FsCrumb> = path
        .ancestors()
        .map(|anc| FsCrumb {
            name: match anc.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                // 盘根（`C:\` / `/`）没有"文件名"：整条路径去掉尾分隔符当名字。
                // Windows 上 `C:\` → `C:`，与「此电脑」下面那一栏的写法一致。
                None => {
                    let raw = anc.to_string_lossy().to_string();
                    let trimmed = raw.trim_end_matches(['\\', '/']).to_string();
                    if trimmed.is_empty() { raw } else { trimmed }
                }
            },
            path: anc.to_string_lossy().to_string(),
        })
        .collect();
    out.reverse();
    out
}

/// 列一层目录（同步；调用方负责放到阻塞线程上）。
fn list_dir(raw: &str) -> Result<FsListing, FsError> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        // 前端只可能把我们给过的绝对路径送回来；真收到相对路径说明状态串了，
        // 这时**不能**按当前工作目录去解析 —— 那会列出一个用户没点过的目录。
        return Err(FsError::plain(format!("不是绝对路径：{raw}")));
    }

    let crumbs = crumbs_of(&path);
    let own_data_dir = crate::config::data_dir();
    let mut entries = Vec::new();
    let dir = std::fs::read_dir(&path).map_err(|e| FsError::from_io(&e, &crumbs))?;
    for entry in dir {
        // 中途出错（网络盘掉线、U 盘被拔）就整层报失败：静默少几项比报错更坏 ——
        // 用户会以为"这里就这些"，然后推漏东西。
        let entry = entry.map_err(|e| FsError::from_io(&e, &crumbs))?;
        let name = entry.file_name().to_string_lossy().to_string();
        // 不外露的三类（见 `unsendable`）。用 `file_type()` 不用 `metadata()`：前者不跟随
        // 链接 —— 跟随了就判不出"这一项是链接"，junction 会装成一个普通目录。
        let Ok(ft) = entry.file_type() else {
            // 单项读不到类型（断掉的软链、刚好被删）跳过：列目录不是审计。
            continue;
        };
        let entry_path = entry.path();
        // 只有卷根那一层按系统名字滤（`D:\backup\Recovery` 这种用户目录不能被藏掉）。
        let at_root = crate::unsendable::sits_in_volume_root(&entry_path);
        if crate::unsendable::is_link(&ft)
            || crate::unsendable::is_system_name(&name, at_root)
            || crate::unsendable::is_own_data_dir(&entry_path, &own_data_dir)
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_dir = ft.is_dir();
        entries.push(FsEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_dir,
            size: if is_dir { 0 } else { meta.len() },
        });
    }

    // 目录在前、再按名（与 `/tree` 同一套排法：本机和对端看到的顺序一致）。
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(FsListing { path: raw.to_string(), crumbs, entries })
}

/// 「设备与驱动器」：选择框打开时取一次（「常用」默认是空的，全靠用户右键加）。
///
/// **必须 async**：同步命令跑在主线程上（见 `accept_push` 的说明），而枚举盘符会调
/// `GetDiskFreeSpaceExW` —— 一张掉线的映射网络盘能让它卡上好几秒，那几秒就是整个界面僵住。
#[tauri::command]
pub async fn fs_drives() -> Result<Vec<FsDrive>, FsError> {
    tauri::async_runtime::spawn_blocking(drives)
        .await
        .map_err(|e| FsError::plain(format!("枚举盘符失败：{e}")))
}

/// 列一层目录。失败时 reject 的是一整个 `FsError`（前端按 `kind` 选状态页）。
#[tauri::command]
pub async fn fs_list(path: String) -> Result<FsListing, FsError> {
    tauri::async_runtime::spawn_blocking(move || list_dir(&path))
        .await
        .map_err(|e| FsError::plain(format!("读取目录失败：{e}")))?
}

// ---------------------------------------------------------------------------
// 盘符枚举
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn drives() -> Vec<FsDrive> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOVABLE};

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        // 盘根必须带尾分隔符：`C:` 在 Windows 路径语义里是"C 盘上的当前目录"，
        // 少了这个反斜杠，之后由它拼出来的一切都会指到别处去。
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let pcw = PCWSTR(wide.as_ptr());

        let dtype = unsafe { GetDriveTypeW(pcw) };
        if dtype == DRIVE_CDROM {
            // 空光驱点进去只会报错；U 盘/移动硬盘是 REMOVABLE，不受这一条影响。
            continue;
        }

        let mut label_buf = [0u16; 64];
        let _ = unsafe { GetVolumeInformationW(pcw, Some(&mut label_buf), None, None, None, None) };
        let end = label_buf.iter().position(|c| *c == 0).unwrap_or(0);
        let label = String::from_utf16_lossy(&label_buf[..end]);
        let label = label.trim();

        let mut total = 0u64;
        let mut free = 0u64;
        let _ = unsafe {
            GetDiskFreeSpaceExW(
                pcw,
                None,
                Some(&mut total as *mut u64),
                Some(&mut free as *mut u64),
            )
        };

        // 没有卷标（U 盘、刚格式化的盘）就用类型名顶上，别给出 ` (F:)` 这种没有主语的条目。
        let name = if label.is_empty() {
            format!("{} ({letter}:)", drive_type_name(dtype))
        } else {
            format!("{label} ({letter}:)")
        };

        out.push(FsDrive {
            name,
            path: root,
            removable: dtype == DRIVE_REMOVABLE,
            total,
            free,
        });
    }
    out
}

#[cfg(windows)]
fn drive_type_name(dtype: u32) -> &'static str {
    use windows::Win32::System::WindowsProgramming::{
        DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };
    match dtype {
        DRIVE_REMOVABLE => "可移动磁盘",
        DRIVE_FIXED => "本地磁盘",
        DRIVE_REMOTE => "网络驱动器",
        DRIVE_RAMDISK => "内存盘",
        _ => "磁盘",
    }
}

#[cfg(target_os = "macos")]
fn drives() -> Vec<FsDrive> {
    // macOS 的"盘"就是 `/` 和挂在 `/Volumes` 下的卷（含 U 盘与外接盘）。
    // 容量留给 future：`statfs` 不在当前依赖里，拿不到就只列名字（界面按 total = 0 不画占用条）。
    let mut out = vec![FsDrive {
        name: "/".to_string(),
        path: "/".to_string(),
        removable: false,
        total: 0,
        free: 0,
    }];
    if let Ok(dir) = std::fs::read_dir("/Volumes") {
        for entry in dir.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            out.push(FsDrive {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                removable: false,
                total: 0,
                free: 0,
            });
        }
    }
    out
}

#[cfg(all(unix, not(target_os = "macos")))]
fn drives() -> Vec<FsDrive> {
    vec![FsDrive {
        name: "/".to_string(),
        path: "/".to_string(),
        removable: false,
        total: 0,
        free: 0,
    }]
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbdrop_fs_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_dirs_first_then_names_and_keeps_non_ascii() {
        let dir = temp_dir("list");
        std::fs::create_dir_all(dir.join("素材")).unwrap();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("readme.md"), b"hi").unwrap();
        std::fs::write(dir.join("中文名.txt"), b"hello world").unwrap();

        let listing = list_dir(&dir.to_string_lossy()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        // 目录在前，各自按名排（'a' < '素'：先按 code point 比，中文排在 ASCII 之后）
        assert_eq!(names, vec!["assets", "素材", "readme.md", "中文名.txt"]);

        let file = listing.entries.iter().find(|e| e.name == "中文名.txt").unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.size, 11);
        assert!(file.path.ends_with("中文名.txt"));

        let sub = listing.entries.iter().find(|e| e.name == "素材").unwrap();
        assert!(sub.is_dir);
        assert_eq!(sub.size, 0, "文件夹不报大小");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn crumbs_run_from_root_down_to_the_listed_dir() {
        let dir = temp_dir("crumbs");
        let listing = list_dir(&dir.to_string_lossy()).unwrap();

        assert_eq!(listing.path, dir.to_string_lossy());
        let last = listing.crumbs.last().unwrap();
        assert_eq!(last.path, dir.to_string_lossy());
        assert_eq!(last.name, dir.file_name().unwrap().to_string_lossy());
        // 第一段是盘根：Windows 上显示成 `C:`（不带尾分隔符），路径仍带
        let first = &listing.crumbs[0];
        assert!(!first.name.ends_with('\\') && !first.name.ends_with('/'));
        assert!(Path::new(&first.path).is_absolute());
        // 相邻两段的 path 必须真的构成父子关系，否则"点面包屑跳过去"会跳到别处
        for pair in listing.crumbs.windows(2) {
            assert!(Path::new(&pair[1].path).starts_with(&pair[0].path));
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_dir_reports_kind_and_keeps_crumbs() {
        let dir = temp_dir("missing");
        let gone = dir.join("已经不在了");

        let err = list_dir(&gone.to_string_lossy()).unwrap_err();
        assert_eq!(err.kind, "missing");
        assert_eq!(err.crumbs.last().unwrap().path, gone.to_string_lossy());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listing_a_file_is_not_a_directory_error() {
        let dir = temp_dir("file");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        let err = list_dir(&file.to_string_lossy()).unwrap_err();
        assert!(
            err.kind == "notDir" || err.kind == "other",
            "文件当目录列不该说成 missing/denied，实际 {}",
            err.kind
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relative_paths_are_refused() {
        // 相对路径绝不能按当前工作目录解析：那会列出用户没点过的目录
        let err = list_dir("some/relative").unwrap_err();
        assert_eq!(err.kind, "other");
        assert!(err.crumbs.is_empty());
    }

    #[test]
    fn io_errors_map_to_ui_states() {
        let mk = |kind| std::io::Error::new(kind, "x");
        assert_eq!(kind_of(&mk(std::io::ErrorKind::PermissionDenied)), "denied");
        assert_eq!(kind_of(&mk(std::io::ErrorKind::NotFound)), "missing");
        assert_eq!(kind_of(&mk(std::io::ErrorKind::NotADirectory)), "notDir");
        assert_eq!(kind_of(&mk(std::io::ErrorKind::TimedOut)), "other");
    }

    #[test]
    fn json_field_names_match_the_frontend_contract() {
        // 前端按 camelCase 读（`src/lib/types.ts`）：字段名走形只会在运行时炸，这里钉死。
        let listing = serde_json::to_value(&FsListing {
            path: "C:\\".into(),
            crumbs: vec![FsCrumb { name: "C:".into(), path: "C:\\".into() }],
            entries: vec![FsEntry { name: "a".into(), path: "C:\\a".into(), is_dir: true, size: 0 }],
        })
        .unwrap();
        assert!(listing.get("crumbs").is_some());
        assert!(listing["entries"][0].get("isDir").is_some());

        let drive = serde_json::to_value(&FsDrive {
            name: "本地磁盘 (C:)".into(),
            path: "C:\\".into(),
            removable: false,
            total: 1,
            free: 1,
        })
        .unwrap();
        assert!(drive.get("removable").is_some());

        let err = serde_json::to_value(FsError {
            kind: "denied",
            message: "x".into(),
            crumbs: vec![],
        })
        .unwrap();
        assert_eq!(err["kind"], "denied");
        assert!(err.get("crumbs").is_some());
    }

    #[test]
    fn links_are_not_listed() {
        // junction / 符号链接一律不列：它们会回环（`%LOCALAPPDATA%\Application Data` 指向
        // 自己的父目录），也会把同一份内容按两条路径列两遍。
        let dir = temp_dir("link");
        std::fs::create_dir_all(dir.join("真的")).unwrap();
        let link = dir.join("链接");
        if !crate::unsendable::make_link_for_test(&link, &dir.join("真的")) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let listing = list_dir(&dir.to_string_lossy()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["真的"], "链接不该出现在选择框里");

        // 先摘链接再删目录：`remove_dir_all` 会跟着链接把目标里的东西也删掉。
        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 系统名字**只在卷根**生效：别处的同名目录是用户自己的东西（`D:\backup\Recovery`
    /// 是很常见的目录名），滤掉就是把用户的数据藏起来。
    #[test]
    fn system_names_only_apply_at_the_volume_root() {
        let dir = temp_dir("sysname");
        std::fs::create_dir_all(dir.join("System Volume Information")).unwrap();
        std::fs::write(dir.join("pagefile.sys"), b"not a real pagefile").unwrap();

        let listing = list_dir(&dir.to_string_lossy()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"System Volume Information"), "非卷根的同名目录照旧可见：{names:?}");
        assert!(names.contains(&"pagefile.sys"), "非卷根的同名文件照旧可见：{names:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 真盘根上的系统项一个都不该露出来 —— "只要在卷根就滤"这条规则的真实验证
    /// （临时目录永远造不出卷根，只能拿真盘来验）。
    #[cfg(windows)]
    #[test]
    fn real_volume_root_hides_its_system_entries() {
        let Ok(listing) = list_dir("C:\\") else {
            return; // 拿不到 C:\ 就算了，不是这条测试要管的事
        };
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        for hidden in ["$RECYCLE.BIN", "System Volume Information", "pagefile.sys"] {
            if std::fs::metadata(format!("C:\\{hidden}")).is_err() {
                continue; // 这台机器上没有它，没什么可滤的
            }
            assert!(
                !names.iter().any(|n| n.eq_ignore_ascii_case(hidden)),
                "{hidden} 就在 C:\\ 上，却没被滤掉：{names:?}"
            );
        }
        assert!(
            names.iter().any(|n| *n == "Windows" || *n == "Users"),
            "正常目录照常列出：{names:?}"
        );
    }

    /// 自家数据目录（里面是 `credentials.dat` —— 全部设备的 token）不该出现在选择框里。
    #[test]
    fn our_own_data_dir_is_not_listed() {
        let own = crate::config::data_dir();
        let Some(parent) = own.parent() else {
            return;
        };
        if std::fs::metadata(&own).is_err() {
            return; // 还没建出来（全新的机器）：没得测
        }
        let Ok(listing) = list_dir(&parent.to_string_lossy()) else {
            return;
        };
        assert!(
            !listing.entries.iter().any(|e| Path::new(&e.path) == own.as_path()),
            "{} 不该出现在选择框里",
            own.display()
        );
    }

    #[test]
    fn drives_are_absolute_roots() {
        let list = drives();
        assert!(!list.is_empty(), "至少该有一个盘");
        for d in &list {
            let p = Path::new(&d.path);
            assert!(p.is_absolute(), "{} 不是绝对路径", d.path);
            assert!(
                p.parent().is_none(),
                "{} 不是盘根 —— 少了尾分隔符就会变成「C 盘上的当前目录」",
                d.path
            );
            assert!(d.name.contains(':') || cfg!(not(windows)));
        }
        // 盘符顺序即字母顺序（A→Z），界面直接按返回顺序画
        #[cfg(windows)]
        {
            let letters: Vec<char> = list
                .iter()
                .filter_map(|d| d.path.chars().next())
                .collect();
            let mut sorted = letters.clone();
            sorted.sort();
            assert_eq!(letters, sorted);
        }
    }
}