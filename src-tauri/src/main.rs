//! Blurt — 完全离线的 Windows 语音输入工具
//! 按下快捷键，说出想法，文字落进光标。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod asr;
mod audio;
mod commands;
mod config;
mod hotkey;
mod hud;
mod inject;
mod selftest;
mod tray;

use tauri::Manager;
use tauri_plugin_global_shortcut::ShortcutState;

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
    if let Some(i) = args.iter().position(|a| a == "--selftest") {
        std::process::exit(selftest::run(args.get(i + 1).cloned()));
    }
    if args.iter().any(|a| a == "--list-mics") {
        for d in audio::list_input_devices() {
            println!("{d}");
        }
        return;
    }

    init_logging();
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
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    let state = app.state::<app::AppState>();
                    let is_main = state
                        .main_shortcut
                        .lock()
                        .as_ref()
                        .map_or(false, |s| s == shortcut);
                    if is_main {
                        match event.state {
                            ShortcutState::Pressed => app::hotkey_pressed(app),
                            ShortcutState::Released => app::hotkey_released(app, None),
                        }
                    }
                })
                .build(),
        )
        .manage(app::AppState::new(cfg.clone()))
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::list_input_devices,
            commands::engine_status,
            commands::reload_engine,
            commands::open_model_dir,
            commands::open_log_dir,
            commands::copy_text,
            commands::capture_hotkey_begin,
            commands::capture_hotkey_end,
            commands::cancel_session,
            commands::get_noise_floor,
            commands::bench_threads,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            tray::create(&handle)?;
            hud::create(&handle)?;

            // 注册全局快捷键（失败不致命：托盘提示，可去设置页改键）
            if let Err(e) = hotkey::register_main(&handle, &cfg.hotkey) {
                tracing::error!("{e}");
                tray::set_tooltip(&handle, &format!("Blurt · {e}"));
            }

            // 后台加载引擎；首次缺模型 → 打开设置页引导
            app::spawn_engine_load(&handle, true);

            tracing::info!("Blurt 已启动");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Blurt 启动失败");

    app.run(|_handle, event| {
        // 关闭所有窗口也不退出（常驻托盘）；只有显式 app.exit() 才退出
        if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
}
