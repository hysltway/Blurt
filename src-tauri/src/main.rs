//! Blurt — 基于豆包 API 的 Windows 语音输入工具
//! 按住全局快捷键，说出想法，文字落进光标。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager;

mod app;
mod audio;
mod autostart;
mod commands;
mod config;
mod doubao;
mod endpoint;
mod hotkey;
mod hud;
mod inject;
mod media_volume;
mod tray;

fn init_logging() {
    use tracing_subscriber::prelude::*;
    let file = tracing_appender::rolling::daily(config::logs_dir(), "blurt.log");
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file)
        .with_ansi(false);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter).with(file_layer);
    #[cfg(debug_assertions)]
    let registry = registry.with(tracing_subscriber::fmt::layer());
    registry.init();
}

fn main() {
    // 命令行模式
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list-mics") {
        match audio::list_input_devices() {
            Ok(devices) => {
                for device in devices {
                    println!("{device}");
                }
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }

    init_logging();
    // 支持部署脚本一次性注入密钥；读取后立刻从进程环境移除。
    if let Ok(api_key) = std::env::var("BLURT_DOUBAO_API_KEY") {
        if let Err(e) = config::save_doubao_api_key(&api_key) {
            tracing::error!("保存环境变量提供的豆包 API Key 失败：{e:#}");
        }
        std::env::remove_var("BLURT_DOUBAO_API_KEY");
    }
    let cfg = config::load();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 二次启动 → 打开设置窗口
            tray::open_settings(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(app::AppState::new(cfg))
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::set_hotkey_capture,
            commands::doubao_api_key_status,
            commands::set_doubao_api_key,
            commands::list_input_devices,
            commands::engine_status,
            commands::refresh_engine_status,
            commands::get_usage_stats,
            commands::set_settings_size,
            commands::open_log_dir,
            commands::cancel_session,
            commands::get_noise_floor,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            let autostart_enabled = handle.state::<app::AppState>().config.read().autostart;
            if let Err(error) = autostart::set_enabled(&handle, autostart_enabled) {
                tracing::error!("同步开机自启动状态失败：{error}");
            }

            tray::create(&handle)?;
            hud::create(&handle)?;

            // 安装可配置的低级键盘钩子（RegisterHotKey 不支持纯修饰键组合）。
            if let Err(e) = hotkey::spawn_chord_hook(&handle) {
                tracing::error!("{e}");
                tray::set_tooltip(&handle, &format!("Blurt · {e}"));
            }

            // 首次缺少 API Key 时打开设置页引导。
            app::refresh_api_status(&handle, true);

            tracing::info!("Blurt 已启动");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Blurt 启动失败");

    app.run(|app_handle, event| {
        match &event {
            tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::Destroyed | tauri::WindowEvent::CloseRequested { .. },
                ..
            } if label == "settings" => {
                app_handle
                    .state::<app::AppState>()
                    .hotkey_capture
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            // 关闭所有窗口也不退出（常驻托盘）；只有显式 app.exit() 才退出
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            _ => {}
        }
    });
}
