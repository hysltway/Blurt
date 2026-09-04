//! 设置页 / HUD 调用的 Tauri 命令。

use tauri::{AppHandle, Emitter, Manager, State};

use crate::app::AppState;
use crate::config::{self, Config, Stats};

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Config {
    state.config.read().clone()
}

#[tauri::command]
pub fn set_config(
    app: AppHandle,
    state: State<'_, AppState>,
    mut config: Config,
) -> Result<(), String> {
    let old = state.config.read().clone();

    config.hotkey = crate::hotkey::canonicalize(&config.hotkey)?;

    // 开机自启
    if config.autostart != old.autostart {
        crate::autostart::set_enabled(&app, config.autostart)?;
        crate::tray::sync_autostart(config.autostart);
    }

    // 换麦克风：缓存的噪声本底不再适用，重置后重新学习
    if config.mic_device != old.mic_device {
        let mut stats = state.stats.lock();
        stats.noise_floor = config::Stats::default().noise_floor;
        config::save_stats(&stats);
    }

    *state.config.write() = config.clone();
    config::save(&config).map_err(|e| format!("保存配置失败：{e}"))?;

    let _ = app.emit("config:updated", &config);

    Ok(())
}

#[tauri::command]
pub fn set_hotkey_capture(state: State<'_, AppState>, capturing: bool) {
    use std::sync::atomic::Ordering;

    state.hotkey_capture.store(capturing, Ordering::SeqCst);
}

#[tauri::command]
pub fn doubao_api_key_status() -> serde_json::Value {
    match config::load_doubao_api_keys() {
        Ok(keys) => {
            let active = keys.iter().find(|k| k.is_active).or_else(|| keys.first());
            let configured = active.is_some();
            let active_name = active.map(|k| k.name.clone());
            serde_json::json!({
                "configured": configured,
                "active_name": active_name,
                "count": keys.len(),
                "error": null
            })
        }
        Err(e) => serde_json::json!({
            "configured": false,
            "active_name": null,
            "count": 0,
            "error": format!("{e:#}")
        }),
    }
}

#[tauri::command]
pub fn list_doubao_api_keys() -> Result<Vec<config::ApiKeyDto>, String> {
    config::list_doubao_api_key_dtos().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn add_doubao_api_key(app: AppHandle, name: String, api_key: String) -> Result<(), String> {
    config::add_doubao_api_key(&name, &api_key).map_err(|e| format!("添加 API Key 失败：{e:#}"))?;
    crate::app::refresh_api_status(&app, false);
    Ok(())
}

#[tauri::command]
pub fn select_doubao_api_key(app: AppHandle, id: String) -> Result<(), String> {
    config::select_doubao_api_key(&id).map_err(|e| format!("切换 API Key 失败：{e:#}"))?;
    crate::app::refresh_api_status(&app, false);
    Ok(())
}

#[tauri::command]
pub fn delete_doubao_api_key(app: AppHandle, id: String) -> Result<(), String> {
    config::delete_doubao_api_key(&id).map_err(|e| format!("删除 API Key 失败：{e:#}"))?;
    crate::app::refresh_api_status(&app, false);
    Ok(())
}

#[tauri::command]
pub fn set_doubao_api_key(app: AppHandle, api_key: String) -> Result<(), String> {
    config::save_doubao_api_key(&api_key).map_err(|e| format!("保存 API Key 失败：{e:#}"))?;
    crate::app::refresh_api_status(&app, false);
    Ok(())
}

#[tauri::command]
pub fn list_input_devices() -> Result<Vec<String>, String> {
    crate::audio::list_input_devices()
}

#[tauri::command]
pub fn engine_status(app: AppHandle) -> serde_json::Value {
    crate::app::engine_status_dto(&app)
}

#[tauri::command]
pub fn refresh_engine_status(app: AppHandle) -> serde_json::Value {
    crate::app::refresh_engine_status(&app)
}

#[tauri::command]
pub fn get_usage_stats(state: State<'_, AppState>) -> Stats {
    state.stats.lock().clone()
}

#[tauri::command]
pub fn set_settings_size(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let window = app
        .get_webview_window("settings")
        .ok_or_else(|| "设置窗口不存在".to_string())?;
    let width = width.clamp(360.0, 1400.0);
    let height = height.clamp(600.0, 1000.0);
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    let current = window
        .inner_size()
        .map_err(|error| error.to_string())?
        .to_logical::<f64>(scale);
    if (current.width - width).abs() < 1.0 && (current.height - height).abs() < 1.0 {
        return Ok(());
    }
    window
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn open_log_dir() {
    let _ = std::process::Command::new("explorer.exe")
        .arg(config::logs_dir())
        .spawn();
}

/// HUD ✕ 按钮：取消当前会话（与 Esc 同路径）
#[tauri::command]
pub fn cancel_session(app: AppHandle) {
    crate::app::esc_pressed(&app);
}

/// HUD 启动时读取缓存的环境噪声本底（跨重启复用，免去每次学习过程）
#[tauri::command]
pub fn get_noise_floor(state: State<'_, AppState>) -> f32 {
    state.stats.lock().noise_floor
}

#[tauri::command]
pub fn get_voiceprint_info() -> crate::voiceprint::VoiceprintInfo {
    crate::voiceprint::get_voiceprint_info()
}

#[tauri::command]
pub fn save_voiceprint_from_audio(samples: Vec<f32>) -> Result<(), String> {
    crate::voiceprint::save_voiceprint_from_audio(&samples).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn delete_voiceprint() -> Result<(), String> {
    crate::voiceprint::delete_voiceprint().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn test_voiceprint_match(samples: Vec<f32>) -> Result<Option<f32>, String> {
    crate::voiceprint::test_voiceprint_match(&samples).map_err(|e| format!("{e:#}"))
}
