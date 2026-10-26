use anyhow::{Context, Result};

/// macOS 的 LaunchAgent 标识。用反向域名（与 bundle id 同源），文件名必须与 `Label` 一致。
#[cfg(target_os = "macos")]
const MACOS_LABEL: &str = "com.x177plus.orbdrop";

/// Enable autostart at login (F-06 / S-04).
pub fn enable_autostart() -> Result<()> {
    let exe_path = std::env::current_exe().context("Failed to resolve executable path")?;
    #[cfg(target_os = "windows")]
    let exe_str = exe_path.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = hkcu
            .open_subkey_with_flags(
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                KEY_SET_VALUE,
            )
            .context("Failed to open Run registry key")?;
        run.set_value("OrbDrop", &exe_str)
            .context("Failed to write registry value")?;
    }

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().context("Failed to resolve home dir")?;
        let plist_dir = home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&plist_dir).context("Failed to create LaunchAgents dir")?;

        // 正式安装时 exe 在 `OrbDrop.app/Contents/MacOS/orbdrop`；登录项要拉起的是 **.app**
        // 而不是裸二进制 —— 后者不会经过 LaunchServices，Dock/激活策略与单实例重入都会走样。
        // 开发机上（target/debug/orbdrop）没有 .app，退回直接执行二进制。
        let bundle = exe_path
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e.to_string_lossy() == "app"));
        let (program, args) = match bundle {
            Some(app) => (
                "/usr/bin/open".to_string(),
                vec!["-a".to_string(), app.to_string_lossy().to_string()],
            ),
            None => (exe_path.to_string_lossy().to_string(), Vec::new()),
        };

        let args_xml = std::iter::once(program)
            .chain(args)
            .map(|a| format!("        <string>{}</string>", xml_escape(&a)))
            .collect::<Vec<_>>()
            .join("\n");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{MACOS_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <!-- 只在图形登录会话里加载：没有 Aqua 会话时 LaunchAgent 会被 launchd 直接判失败。 -->
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>"#
        );
        let plist_path = plist_dir.join(format!("{MACOS_LABEL}.plist"));
        std::fs::write(&plist_path, plist).context("Failed to write plist")?;
        // 旧版本用的是 `com.orbdrop.plist`：升级上来的机器上两份同时存在会启动两个进程
        // （第二份被单实例锁挡下，只会在日志里留一条），清掉旧的。
        let _ = std::fs::remove_file(plist_dir.join("com.orbdrop.plist"));
    }

    Ok(())
}

/// Disable autostart.
pub fn disable_autostart() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = hkcu
            .open_subkey_with_flags(
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                KEY_SET_VALUE,
            )
            .context("Failed to open Run registry key")?;
        let _ = run.delete_value("OrbDrop");
    }

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().context("Failed to resolve home dir")?;
        let plist_dir = home.join("Library/LaunchAgents");
        let _ = std::fs::remove_file(plist_dir.join(format!("{MACOS_LABEL}.plist")));
        let _ = std::fs::remove_file(plist_dir.join("com.orbdrop.plist"));
    }

    Ok(())
}

/// XML 文本节点转义（应用路径里可能有 `&`）。
#[cfg(target_os = "macos")]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}