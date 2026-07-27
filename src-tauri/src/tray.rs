//! 托盘图标与菜单（简体中文）。
//! 交互约定：左键单击 → 打开设置；右键 → 原生菜单（状态/设置/自启/引擎/日志/退出）。

use parking_lot::Mutex;
use tauri::menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_autostart::ManagerExt;

// 菜单项句柄（用于动态更新状态文本 / 同步自启勾选）
static STATUS_ITEM: Mutex<Option<MenuItem<Wry>>> = Mutex::new(None);
static AUTOSTART_ITEM: Mutex<Option<CheckMenuItem<Wry>>> = Mutex::new(None);

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let cfg = app.state::<crate::app::AppState>().config.read().clone();

    let status_i = MenuItemBuilder::with_id("status", "启动中…")
        .enabled(false)
        .build(app)?;
    let settings_i = MenuItemBuilder::with_id("settings", "设置…").build(app)?;
    let autostart_i = CheckMenuItemBuilder::with_id("autostart", "开机自启动")
        .checked(cfg.autostart)
        .build(app)?;
    let reload_i = MenuItemBuilder::with_id("reload", "重新加载引擎").build(app)?;
    let logs_i = MenuItemBuilder::with_id("logs", "打开日志目录").build(app)?;
    let quit_i = MenuItemBuilder::with_id("quit", "退出 Blurt").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&status_i)
        .separator()
        .item(&settings_i)
        .item(&autostart_i)
        .separator()
        .item(&reload_i)
        .item(&logs_i)
        .separator()
        .item(&quit_i)
        .build()?;

    *STATUS_ITEM.lock() = Some(status_i);
    *AUTOSTART_ITEM.lock() = Some(autostart_i);

    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Blurt · 启动中…")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            tracing::info!("托盘菜单：{:?}", event.id());
            match event.id().as_ref() {
                "settings" => open_settings(app),
                "autostart" => {
                    // CheckMenuItem 点击后自动翻转勾选，这里读取新状态并应用
                    let checked = AUTOSTART_ITEM
                        .lock()
                        .as_ref()
                        .and_then(|i| i.is_checked().ok())
                        .unwrap_or(false);
                    apply_autostart(app, checked);
                }
                "reload" => crate::app::spawn_engine_load(app, false),
                "logs" => {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(crate::config::logs_dir())
                        .spawn();
                }
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => {
                tracing::info!("托盘左键 → 打开设置");
                open_settings(tray.app_handle());
            }
            TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Up,
                ..
            } => {
                // 菜单由系统原生弹出；此日志用于确认事件送达
                tracing::info!("托盘右键（原生菜单应已弹出）");
            }
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => open_settings(tray.app_handle()),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn apply_autostart(app: &AppHandle, enable: bool) {
    let state = app.state::<crate::app::AppState>();
    {
        let mut c = state.config.write();
        c.autostart = enable;
        let _ = crate::config::save(&c);
    }
    let al = app.autolaunch();
    let r = if enable { al.enable() } else { al.disable() };
    if let Err(e) = r {
        tracing::error!("设置开机自启失败：{e}");
    }
}

/// 设置页改动自启后，同步托盘勾选状态
pub fn sync_autostart(enabled: bool) {
    if let Some(item) = &*AUTOSTART_ITEM.lock() {
        let _ = item.set_checked(enabled);
    }
}

/// 同步更新托盘悬停提示与菜单里的状态行
pub fn set_tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(text));
    }
    if let Some(item) = &*STATUS_ITEM.lock() {
        let _ = item.set_text(text.strip_prefix("Blurt · ").unwrap_or(text));
    }
}

pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Blurt 设置")
        .inner_size(680.0, 860.0)
        .resizable(false)
        .maximizable(false)
        .center()
        // 浅色主题 + 同色底（避免加载闪烁）
        .theme(Some(tauri::Theme::Light))
        .background_color(tauri::webview::Color(238, 241, 246, 255))
        .build();
}
