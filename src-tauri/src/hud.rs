//! 悬浮 HUD 窗口：透明、置顶、点击穿透、永不抢焦点；
//! 每次录音开始前吸附到「当前活动窗口所在显示器」的底部居中。

use serde_json::json;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

pub const HUD_W: f64 = 360.0;
pub const HUD_H: f64 = 140.0;

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let win = WebviewWindowBuilder::new(app, "hud", WebviewUrl::App("hud.html".into()))
        .title("Blurt")
        .inner_size(HUD_W, HUD_H)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        .build()?;

    // 点击穿透
    let _ = win.set_ignore_cursor_events(true);

    // WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW：显示时绝不抢焦点、不出现在 Alt+Tab
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };
        if let Ok(h) = win.hwnd() {
            let hwnd = HWND(h.0 as isize as *mut core::ffi::c_void);
            let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                old | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize,
            );
        }
    }
    Ok(())
}

/// 把 HUD 移到活动窗口所在显示器的底部居中
pub fn position_on_active_monitor(app: &AppHandle) {
    let Some(win) = app.get_webview_window("hud") else {
        return;
    };

    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

        let fg = GetForegroundWindow();
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let wa = mi.rcWork;
            let scale = win.scale_factor().unwrap_or(1.0);
            let (w, h) = ((HUD_W * scale) as i32, (HUD_H * scale) as i32);
            let x = wa.left + ((wa.right - wa.left) - w) / 2;
            let y = wa.bottom - h - (20.0 * scale) as i32;
            let _ = win.set_position(PhysicalPosition::new(x, y));
        }
    }
}

pub fn show(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("hud") {
        let _ = win.show();
        let _ = win.set_always_on_top(true);
        pin_native_topmost(&win);
    }
}

#[cfg(windows)]
fn pin_native_topmost(win: &WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };

    if let Ok(handle) = win.hwnd() {
        let hwnd = HWND(handle.0 as isize as *mut core::ffi::c_void);
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
    }
}

#[cfg(not(windows))]
fn pin_native_topmost(_win: &WebviewWindow) {}

pub fn hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("hud") {
        let _ = win.hide();
    }
}

pub fn emit_state(app: &AppHandle, state: &str, eta_ms: Option<u64>) {
    if state != "hidden" {
        show(app);
    }
    let _ = app.emit_to(
        "hud",
        "hud:state",
        json!({ "state": state, "eta_ms": eta_ms }),
    );
}

pub fn emit_level(app: &AppHandle, v: f32) {
    let _ = app.emit_to("hud", "hud:level", json!({ "v": v }));
}

/// 会话期间轮询鼠标：光标进入 HUD 区域 → 解除点击穿透并浮现 ✕ 按钮；
/// 移出 → 恢复穿透。会话结束自动复原。
pub fn spawn_hover_watcher(app: &AppHandle, gen: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

        let mut inside_prev = false;
        let mut topmost_tick = 0u8;
        loop {
            {
                let state = app.state::<crate::app::AppState>();
                if state.gen.load(Ordering::SeqCst) != gen
                    || matches!(&*state.session.lock(), crate::app::Session::Idle)
                {
                    break;
                }
            }
            let Some(win) = app.get_webview_window("hud") else {
                break;
            };
            if topmost_tick == 0 {
                if !win.is_visible().unwrap_or(false) {
                    let _ = win.show();
                }
                let _ = win.set_always_on_top(true);
                pin_native_topmost(&win);
            }
            topmost_tick = (topmost_tick + 1) % 6;
            let mut pt = POINT::default();
            let _ = unsafe { GetCursorPos(&mut pt) };
            let inside = win
                .outer_position()
                .ok()
                .zip(win.outer_size().ok())
                .map(|(pos, size)| {
                    pt.x >= pos.x
                        && pt.x < pos.x + size.width as i32
                        && pt.y >= pos.y
                        && pt.y < pos.y + size.height as i32
                })
                .unwrap_or(false);
            if inside != inside_prev {
                inside_prev = inside;
                let _ = win.set_ignore_cursor_events(!inside);
                let _ = app.emit_to("hud", "hud:hover", json!({ "v": inside }));
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        if let Some(win) = app.get_webview_window("hud") {
            let _ = win.set_ignore_cursor_events(true);
            let _ = app.emit_to("hud", "hud:hover", json!({ "v": false }));
        }
    });
}

/// `delay_ms` 后，若期间没有新会话开始（gen 未变），隐藏 HUD
pub fn hide_later(app: &AppHandle, delay_ms: u64, gen: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let state = app.state::<crate::app::AppState>();
        if state.gen.load(std::sync::atomic::Ordering::SeqCst) == gen
            && matches!(&*state.session.lock(), crate::app::Session::Idle)
        {
            emit_state(&app, "hidden", None);
            hide(&app);
        }
    });
}
