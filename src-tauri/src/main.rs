//! Blurt — 完全离线的 Windows 语音输入工具
//! 按住 Ctrl+Alt，说出想法，文字落进光标。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager;

mod app;
mod asr;
mod audio;
mod autostart;
mod commands;
mod config;
mod doubao;
mod hotkey;
mod hud;
mod inject;
mod selftest;
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
            commands::doubao_api_key_status,
            commands::set_doubao_api_key,
            commands::list_input_devices,
            commands::engine_status,
            commands::reload_engine,
            commands::open_model_dir,
            commands::open_log_dir,
            commands::copy_text,
            commands::cancel_session,
            commands::get_noise_floor,
            commands::bench_threads,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            let autostart_enabled = handle.state::<app::AppState>().config.read().autostart;
            if let Err(error) = autostart::set_enabled(&handle, autostart_enabled) {
                tracing::error!("同步开机自启动状态失败：{error}");
            }

            tray::create(&handle)?;
            hud::create(&handle)?;

            // 安装写死的 Ctrl+Alt 键盘钩子（RegisterHotKey 不支持纯修饰键组合，
            // 必须走低级钩子；失败不致命，托盘提示后其余功能照常）
            if let Err(e) = hotkey::spawn_chord_hook(&handle) {
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
