use anyhow::{Context, Result};

/// Enable autostart at login (F-06 / S-04).
pub fn enable_autostart() -> Result<()> {
    let exe_path = std::env::current_exe().context("Failed to resolve executable path")?;
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
        let plist_path = plist_dir.join("com.orbdrop.plist");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.orbdrop</string>
    <key>ProgramArguments</key>
    <array><string>{exe_str}</string></array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#
        );
        std::fs::write(&plist_path, plist).context("Failed to write plist")?;
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
        let plist_path = home.join("Library/LaunchAgents/com.orbdrop.plist");
        let _ = std::fs::remove_file(plist_path);
    }

    Ok(())
}
