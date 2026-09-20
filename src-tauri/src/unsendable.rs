//! 「不能推送」的东西：**选择框列目录**（`fs_browse`）和**发送前建清单**（`transfer`）
//! 共用这一份判定 —— 两处各写一套，迟早会漏一处，用户就会看到"列出来了却推不动"。
//!
//! 两类，理由完全不同：
//!
//! 1. **系统自己的目录/文件**（`System Volume Information`、`$RECYCLE.BIN`、`pagefile.sys`…）。
//!    不是用户放的资料：列出来只会让人以为能推，推过去也只是垃圾，而且大多连读都读不了
//!    （`read_dir` 直接 ACL 拒绝）。**只在卷根按名字匹配** —— `D:\backup\Recovery` 是用户
//!    自己的目录，同名也不该被藏掉。
//! 2. **链接**（Windows 的符号链接与 junction、Unix 的符号链接）。走查会**回环**
//!    （`%LOCALAPPDATA%\Application Data` 指向自己的父目录，几百层地重复同一棵树），
//!    或者把同一份内容按两条路径列两遍。任何一层都不列、不进。
//!
//! 除这两类之外**一律不过滤**：`.git`、`AppData`、隐藏的真实文件都得能选到 —— 选择框问的是
//! "我要推哪些东西"，不是"系统给我看什么"（与 `/tree` 相反，那个是给对端看的）。
//!
//! `desktop.ini`、`Thumbs.db`、`Icon\r` 这类"能读但没意义"的小文件**不在这里**：它们几乎
//! 每个目录都有，滤掉只会让"跳过了 N 项"这句话天天出现，把真正该说的（链接、读不到）淹掉。
//!
//! 平台名单只列**有把握的**：Windows 的 `Documents and Settings`、`Users\All Users`、
//! `AppData\Local\Application Data` 这些一个名字都不用写 —— 它们全是 junction，由 [`is_link`]
//! 一并挡掉（而且它们本来也读不了）。

use std::path::Path;

/// 这一项是不是**直接躺在卷根里**（`C:\pagefile.sys`、`C:\Recovery`、`/lost+found`）。
///
/// 所有"只在卷根按名字滤"的地方都经由它，别各自去写 `parent().is_none()` —— 那个写法只对
/// "被列的目录"成立，对"某一项自己"是错的（`C:\pagefile.sys` 的 parent 是 `C:\`，不是
/// `None`，写成那样就等于"顶层源没滤"）。两种问法问的其实是同一件事：**这一项的上一级
/// 是不是卷根**。
pub fn sits_in_volume_root(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    // `C:\pagefile.sys`、`/lost+found`：上一级就是根。
    if parent.parent().is_none() {
        return true;
    }
    // macOS 的外接盘挂在 `/Volumes` 下，它们**也是卷根** —— `.Trashes`、`.Spotlight-V100`
    // 就长在那儿，光看"/ 的下一级"会把它们全漏掉。
    #[cfg(target_os = "macos")]
    if parent.parent() == Some(Path::new("/Volumes")) {
        return true;
    }
    false
}

/// 这个**名字**是不是系统自己的。`at_root` = 它正处在卷根（`C:\`、`/`、`/Volumes/X`）。
///
/// 卷根以外的同名项一律当普通项：用户有权在自己的目录树里叫任何名字。
/// `at_root` 请由 [`sits_in_volume_root`] 算出来。
#[cfg(windows)]
pub fn is_system_name(name: &str, at_root: bool) -> bool {
    at_root && WINDOWS_ROOT.iter().any(|n| name.eq_ignore_ascii_case(n))
}

#[cfg(target_os = "macos")]
pub fn is_system_name(name: &str, at_root: bool) -> bool {
    let name = name.trim_end_matches('\r');
    if name.starts_with("._") || MACOS_ANY_LEVEL.iter().any(|n| name.eq_ignore_ascii_case(n)) {
        return true;
    }
    at_root && MACOS_ROOT.iter().any(|n| name.eq_ignore_ascii_case(n))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn is_system_name(name: &str, at_root: bool) -> bool {
    at_root
        && (LINUX_ROOT.iter().any(|n| name == *n)
            // `.Trash-1000`：外接盘上按用户 id 命名的回收站，编号不定，只能按前缀认。
            || name.starts_with(".Trash-"))
}

/// 卷根上属于 Windows 自己的名字。
#[cfg(windows)]
const WINDOWS_ROOT: &[&str] = &[
    // 垃圾桶与还原点：系统自己的，不是用户资料。
    "$RECYCLE.BIN",
    "RECYCLER",
    "System Volume Information",
    // 安装 / 升级 / 重置的暂存：读不了，还常常几十 GB。
    "Config.Msi",
    "$WinREAgent",
    "$SysReset",
    "$GetCurrent",
    "$Windows.~BT",
    "$Windows.~WS",
    "Recovery",
    "MSOCache",
    "OneDriveTemp",
    "$Extend",
    // chkdsk 从坏盘上捡回来的碎片。
    "found.000",
    "found.001",
    "found.002",
    // 盘根那几个被系统占着的巨型文件：open 就是共享冲突。
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "DumpStack.log",
    "DumpStack.log.tmp",
];

/// 卷根上属于 macOS 自己的东西：Spotlight / fsevents 的索引库、每个卷的废纸篓、
/// 临时目录、Time Machine 的"别备份我"标记、HFS+ 的私有目录。都是系统产物，且大多不可读。
#[cfg(target_os = "macos")]
const MACOS_ROOT: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".vol",
    ".MobileBackups",
    ".HFS+ Private Directory Data",
    ".com.apple.timemachine.donotpresent",
    ".PKInstallSandboxManager",
    "lost+found",
];

/// macOS 上**任何一层**都算系统产物的：Finder 的窗口状态、AppleDouble 影子文件、
/// Finder 的"这个目录已本地化"标记、废纸篓。留着只会在每个目录里都冒出来。
#[cfg(target_os = "macos")]
const MACOS_ANY_LEVEL: &[&str] = &[".DS_Store", ".localized", ".AppleDouble", ".Trash"];

/// Linux 卷根上的系统产物：ext 的 `lost+found`、各家的回收站目录。
#[cfg(all(unix, not(target_os = "macos")))]
const LINUX_ROOT: &[&str] = &[
    "lost+found",
    ".Trash",
    ".Trashes",
    // 外接盘上被 macOS 写进来的，Linux 上照样是别人的垃圾桶。
    ".Spotlight-V100",
    ".fseventsd",
];

/// 目录项是不是**链接**。选择框不列它、走查不进它。
///
/// Windows 上 `is_symlink()` 覆盖符号链接与 junction（挂载点重解析）—— 正是会回环、会重复
/// 列同一棵树的那两类。其它的重解析点（OneDrive 占位目录、WOF 压缩）不在此列：它们是真的
/// 目录，内容也读得到，照常走。
pub fn is_link(file_type: &std::fs::FileType) -> bool {
    file_type.is_symlink()
}

/// 是不是 OrbDrop **自己的数据目录**（设置、日志、断点，以及装着全部设备 token 的
/// `credentials.dat`）。它不该被推给任何人，也不该出现在选择框里。
///
/// `own` 由调用方**算一次**传进来（`crate::config::data_dir()` 要读一次环境变量，
/// 走查里每个文件项都去读一遍纯属浪费）。
pub fn is_own_data_dir(path: &Path, own: &Path) -> bool {
    // 先过一道便宜的闸门：名字对不上就不用比整条路径。
    match (path.file_name(), own.file_name()) {
        (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => {}
        _ => return false,
    }
    same_path(path, own)
}

/// Windows 的路径大小写不敏感：用户从 `c:\users\…` 那一侧进来时，逐字节比较认不出来。
#[cfg(windows)]
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

#[cfg(not(windows))]
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
}

/// 测试用：造一个链接。Windows 走 `mklink /J`（junction 不需要管理员，`symlink_dir` 要
/// 开发者模式），Unix 走符号链接。造不出来（环境没开权限）返回 `false`，由调用方跳过断言
/// —— 测试不该因为机器配置就红。
#[cfg(test)]
pub(crate) fn make_link_for_test(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名单是**卷根限定**的：别处的同名目录是用户自己的东西，不能因为撞名就被藏掉
    /// （`D:\backup\Recovery` 是很常见的目录名）。
    #[test]
    fn root_only_names_are_not_filtered_elsewhere() {
        for name in ["System Volume Information", "$RECYCLE.BIN", "Recovery", "lost+found"] {
            assert!(!is_system_name(name, false), "{name} 在非卷根不该被滤");
        }
    }

    /// "在不在卷根"只有一种问法，就是这一句 —— 列一层时问的是它里面的项，顶层源问的是
    /// 源自己，两者都得落到"上一级是不是卷根"上。
    #[test]
    fn volume_root_means_the_first_level_only() {
        #[cfg(windows)]
        {
            assert!(sits_in_volume_root(Path::new("C:\\pagefile.sys")));
            assert!(sits_in_volume_root(Path::new("C:\\Recovery")));
            assert!(!sits_in_volume_root(Path::new("C:\\Users\\All Users")));
            // 卷根本身不是"躺在卷根里"，它是卷根。
            assert!(!sits_in_volume_root(Path::new("C:\\")));
        }
        #[cfg(unix)]
        {
            assert!(sits_in_volume_root(Path::new("/lost+found")));
            assert!(!sits_in_volume_root(Path::new("/home/lost+found")));
            assert!(!sits_in_volume_root(Path::new("/")));
        }
        // macOS 的外接盘也是卷根：`.Trashes` 就长在 `/Volumes/U盘/` 下面。
        #[cfg(target_os = "macos")]
        {
            assert!(sits_in_volume_root(Path::new("/Volumes/优盘/.Trashes")));
            assert!(sits_in_volume_root(Path::new("/Volumes/优盘/素材")));
            assert!(!sits_in_volume_root(Path::new("/Volumes/优盘/素材/子目录")));
            // 只是名字里有个 Volumes 的普通目录，不算卷根。
            assert!(!sits_in_volume_root(Path::new("/Users/x/Volumes/优盘/.Trashes")));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_root_names_are_filtered_case_insensitively() {
        assert!(is_system_name("System Volume Information", true));
        assert!(is_system_name("system volume information", true));
        assert!(is_system_name("$RECYCLE.BIN", true));
        assert!(is_system_name("$Recycle.Bin", true));
        assert!(is_system_name("Config.Msi", true));
        assert!(is_system_name("pagefile.sys", true));
        assert!(is_system_name("hiberfil.sys", true));
        assert!(is_system_name("$WinREAgent", true));
        assert!(is_system_name("found.000", true));
        // 用户真的会有的名字：不能滤。
        for name in ["Windows", "Users", "ProgramData", "Program Files", "AppData", ".git", "素材"] {
            assert!(!is_system_name(name, true), "{name} 不该被滤");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_names_are_filtered() {
        assert!(is_system_name(".Spotlight-V100", true));
        assert!(is_system_name(".fseventsd", true));
        assert!(is_system_name(".Trashes", true));
        assert!(is_system_name(".DS_Store", false), "Finder 的状态文件在任何一层都是垃圾");
        assert!(is_system_name("._报告.pdf", false), "AppleDouble 影子文件");
        assert!(is_system_name(".localized", false));
        // 用户的真实目录：不能滤。
        for name in ["Users", "Applications", "Library", "System", "Documents"] {
            assert!(!is_system_name(name, true), "{name} 不该被滤");
        }
        assert!(!is_system_name(".Spotlight-V100", false), "只有卷根那个是真的");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_root_names_are_filtered() {
        assert!(is_system_name("lost+found", true));
        assert!(is_system_name(".Trash-1000", true));
        assert!(is_system_name(".Trashes", true));
        assert!(!is_system_name("lost+found", false));
        assert!(!is_system_name("home", true));
    }

    #[test]
    fn our_own_data_dir_is_refused() {
        let own = crate::config::data_dir();
        assert!(is_own_data_dir(&own, &own));
        assert!(!is_own_data_dir(&own.join("logs"), &own), "只挡数据目录本身，别挡它外面的世界");
        assert!(!is_own_data_dir(Path::new("/definitely/not/orbdrop"), &own));
    }
}