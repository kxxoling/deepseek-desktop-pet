//! 开机自启：向操作系统登记用户级自启项，不需要管理员权限。
//! 三个平台的登记处不同——macOS 写 `~/Library/LaunchAgents/` 下的 plist 文件，Windows 写注册表当前用户下的 `Run` 项，Linux 写 `~/.config/autostart/` 下的 desktop 文件。
//! 内容生成与解析都是纯函数，实际读写按平台分包；开启与关闭幂等，启动时顺带清理指向失效路径的旧自启项。

/// 自启项的统一标识：macOS 作 plist 的 Label，Windows 作注册表值名，由各平台的文件名/键名派生
pub const AUTOSTART_ID: &str = "com.ds-pet";

/// macOS 的自启登记文件（LaunchAgent plist）内容
#[cfg(not(target_os = "windows"))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchagent_plist(exe_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AUTOSTART_ID}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe_path}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// Linux 的自启登记文件（XDG autostart 的 desktop 文件）内容
#[cfg(not(target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn desktop_entry(exe_path: &str) -> String {
    format!(
        r"[Desktop Entry]
Type=Application
Name=ds-pet
Exec={exe_path}
X-GNOME-Autostart-enabled=true
"
    )
}

/// Windows 注册表自启项里存的值：带引号的完整命令行
#[cfg(target_os = "windows")]
pub fn run_entry_value(exe_path: &str) -> String {
    format!("\"{exe_path}\"")
}

/// 从自启内容里提取指向的可执行文件路径；解析不出返回 None
pub fn extract_exec_path(content: &str) -> Option<String> {
    if content.contains("<plist") {
        // Label 也是 <string>，取 ProgramArguments 数组里的那个
        let pa = content.find("<key>ProgramArguments</key>")?;
        let i = content[pa..].find("<string>")? + pa + "<string>".len();
        let j = content[i..].find("</string>")? + i;
        return Some(content[i..j].to_string());
    }
    if content.starts_with("[Desktop Entry]") {
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("Exec=") {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
        return None;
    }
    // Run 键值：去引号
    Some(content.trim().trim_matches('"').to_string())
}

// ---------- 平台落盘 ----------

/// 是否已开启
#[cfg(not(target_os = "windows"))]
pub fn is_enabled() -> bool {
    entry_path().is_file()
}

/// 开启（幂等：内容一致不重写）。返回是否成功
#[cfg(not(target_os = "windows"))]
pub fn enable() -> bool {
    let Some(exe) = current_exe() else {
        return false;
    };
    write_entry(&exe)
}

/// 关闭（幂等：不存在也返回成功）
#[cfg(not(target_os = "windows"))]
pub fn disable() -> bool {
    let p = entry_path();
    match std::fs::remove_file(&p) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            log::error!("移除自启项失败：{e}");
            false
        }
    }
}

/// 启动自检：自启项存在但指向的可执行文件已不在（旧安装位置），清掉它
#[cfg(not(target_os = "windows"))]
pub fn cleanup_stale() {
    let p = entry_path();
    let Ok(content) = std::fs::read_to_string(&p) else {
        return;
    };
    let stale = extract_exec_path(&content).is_some_and(|exe| !std::path::Path::new(&exe).exists());
    if stale {
        log::info!("清理失效自启项（指向的路径已不存在）：{}", p.display());
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(target_os = "macos")]
fn entry_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{AUTOSTART_ID}.plist"))
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn entry_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(
            || {
                let home = std::env::var_os("HOME").unwrap_or_default();
                std::path::PathBuf::from(home).join(".config")
            },
            std::path::PathBuf::from,
        )
        .join("autostart")
        .join("ds-pet.desktop")
}

#[cfg(not(target_os = "windows"))]
fn current_exe() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
}

#[cfg(target_os = "macos")]
fn write_entry(exe: &str) -> bool {
    write_file(launchagent_plist(exe))
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn write_entry(exe: &str) -> bool {
    write_file(desktop_entry(exe))
}

#[cfg(not(target_os = "windows"))]
fn write_file(content: String) -> bool {
    let p = entry_path();
    let Some(dir) = p.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        log::error!("创建自启目录失败：{}", dir.display());
        return false;
    }
    match std::fs::write(&p, content) {
        Ok(()) => true,
        Err(e) => {
            log::error!("写入自启项失败：{e}");
            false
        }
    }
}

#[cfg(target_os = "windows")]
pub mod windows_registry {
    //! 注册表自启项（当前用户 `Run` 项）读写。用 windows crate（已是依赖）
    //! 的注册表 feature，不另引 winreg。

    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    /// 当前用户登录时资源管理器会执行此项下所有值
    const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE_NAME: &str = crate::autostart::AUTOSTART_ID;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_bytes(v: &[u16]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), v.len() * 2) }
    }

    fn open_key(access: REG_SAM_FLAGS) -> Option<HKEY> {
        let mut key = HKEY::default();
        let subkey = wide(RUN_SUBKEY);
        let ok = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                access,
                std::ptr::from_mut(&mut key),
            )
        };
        ok.is_ok().then_some(key)
    }

    pub fn read_value() -> Option<String> {
        let key = open_key(KEY_READ)?;
        let name = wide(VALUE_NAME);
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let mut ty = REG_VALUE_TYPE::default();
        let ok = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(std::ptr::from_mut(&mut ty)),
                Some(buf.as_mut_ptr().cast()),
                Some(std::ptr::from_mut(&mut len)),
            )
        };
        unsafe {
            let _ = RegCloseKey(key);
        }
        if ok.is_err() {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
        Some(s)
    }

    pub fn write_value(cmd: &str) -> bool {
        let Some(key) = open_key(KEY_SET_VALUE) else {
            log::error!("打开 HKCU Run 键失败");
            return false;
        };
        let name = wide(VALUE_NAME);
        let data = wide(cmd);
        let ok = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                REG_SZ,
                Some(wide_bytes(&data)),
            )
        };
        unsafe {
            let _ = RegCloseKey(key);
        }
        ok.is_ok()
    }

    pub fn delete_value() -> bool {
        let Some(key) = open_key(KEY_SET_VALUE) else {
            return true; // 键本身不存在视为已删除
        };
        let name = wide(VALUE_NAME);
        let ok = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        ok.is_ok() || read_value().is_none()
    }
}

#[cfg(target_os = "windows")]
pub fn is_enabled() -> bool {
    windows_registry::read_value().is_some()
}

#[cfg(target_os = "windows")]
pub fn enable() -> bool {
    match std::env::current_exe() {
        Ok(exe) => windows_registry::write_value(&run_entry_value(&exe.display().to_string())),
        Err(_) => false,
    }
}

#[cfg(target_os = "windows")]
pub fn disable() -> bool {
    windows_registry::delete_value()
}

#[cfg(target_os = "windows")]
pub fn cleanup_stale() {
    if let Some(cmd) = windows_registry::read_value() {
        let exe = extract_exec_path(&cmd).unwrap_or_default();
        if !std::path::Path::new(&exe).exists() {
            log::info!("清理失效自启项（指向的路径已不存在）：{exe}");
            let _ = windows_registry::delete_value();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "windows"))]
    const EXE: &str = "/Applications/ds-pet.app/Contents/MacOS/ds-pet";

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn plist_content_pinned() {
        let want = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.ds-pet</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Applications/ds-pet.app/Contents/MacOS/ds-pet</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#;
        assert_eq!(launchagent_plist(EXE), want);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn desktop_entry_pinned() {
        let want = r"[Desktop Entry]
Type=Application
Name=ds-pet
Exec=/usr/bin/ds-pet
X-GNOME-Autostart-enabled=true
";
        assert_eq!(desktop_entry("/usr/bin/ds-pet"), want);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn run_entry_value_quotes_path() {
        assert_eq!(
            run_entry_value(r"C:\Program Files\ds-pet\ds-pet.exe"),
            r#""C:\Program Files\ds-pet\ds-pet.exe""#
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn extract_exec_path_roundtrips_all_formats() {
        assert_eq!(
            extract_exec_path(&launchagent_plist(EXE)).as_deref(),
            Some(EXE)
        );
        assert_eq!(
            extract_exec_path(&desktop_entry("/usr/bin/ds-pet")).as_deref(),
            Some("/usr/bin/ds-pet")
        );
        // 带引号的 desktop Exec 同样可用
        let quoted_desktop = "[Desktop Entry]\nType=Application\nExec=\"/opt/ds-pet/ds-pet\"\n";
        assert_eq!(
            extract_exec_path(quoted_desktop),
            Some("/opt/ds-pet/ds-pet".to_string())
        );
    }

    #[test]
    fn extract_exec_path_unparseable() {
        // plist 缺 string 段
        assert_eq!(extract_exec_path("<plist><dict/></plist>"), None);
        // desktop 缺 Exec 行
        assert_eq!(
            extract_exec_path("[Desktop Entry]\nType=Application\n"),
            None
        );
    }

    // 文件路径生成（macOS 口径）
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn entry_path_matches_platform_convention() {
        let p = entry_path();
        #[cfg(target_os = "macos")]
        assert!(
            p.ends_with("Library/LaunchAgents/com.ds-pet.plist"),
            "{}",
            p.display()
        );
        #[cfg(not(target_os = "macos"))]
        assert!(p.ends_with("ds-pet.desktop"), "{}", p.display());
    }

    // 幂等与清理的文件级行为（macOS 本机跑；CI 的 linux 走 cfg 分支同构）
    #[cfg(target_os = "macos")]
    #[test]
    fn enable_disable_idempotent_and_cleanup() {
        // 临时 HOME，避免动真实 LaunchAgents
        let tmp =
            std::env::temp_dir().join(format!("ds-pet-autostart-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        assert!(!is_enabled());
        assert!(enable());
        assert!(is_enabled());
        let first = std::fs::read_to_string(entry_path()).unwrap();
        assert!(enable()); // 幂等
        assert_eq!(std::fs::read_to_string(entry_path()).unwrap(), first);
        assert!(disable());
        assert!(!is_enabled());
        assert!(disable()); // 不存在也成功

        // 失效项清理：写入指向不存在路径的内容
        std::fs::create_dir_all(tmp.join("Library/LaunchAgents")).unwrap();
        std::fs::write(entry_path(), launchagent_plist("/deleted/old/ds-pet")).unwrap();
        cleanup_stale();
        assert!(!is_enabled());

        unsafe {
            match prev {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
