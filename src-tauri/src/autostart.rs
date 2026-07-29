//! 开机自启动状态与 Windows 注册表的同步。

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

#[cfg(windows)]
fn ensure_windows_run_key() -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const RUN_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";
    RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN_KEY)
        .map(|_| ())
        .map_err(|error| format!("创建 Windows 启动项注册表失败：{error}"))
}

#[cfg(not(windows))]
fn ensure_windows_run_key() -> Result<(), String> {
    Ok(())
}

/// 应用期望状态，并回读插件状态确认 Windows 已实际接受变更。
pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    // auto-launch 0.5 只打开 Run 键；精简过的用户配置中该键可能尚不存在。
    ensure_windows_run_key()?;

    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch
            .enable()
            .map_err(|error| format!("启用开机自启动失败：{error}"))?;
    } else if autolaunch
        .is_enabled()
        .map_err(|error| format!("读取开机自启动状态失败：{error}"))?
    {
        autolaunch
            .disable()
            .map_err(|error| format!("关闭开机自启动失败：{error}"))?;
    }

    let actual = autolaunch
        .is_enabled()
        .map_err(|error| format!("校验开机自启动状态失败：{error}"))?;
    if actual == enabled {
        Ok(())
    } else {
        Err(format!(
            "开机自启动状态未生效（期望：{}，实际：{}）",
            state_label(enabled),
            state_label(actual)
        ))
    }
}

fn state_label(enabled: bool) -> &'static str {
    if enabled {
        "开启"
    } else {
        "关闭"
    }
}
