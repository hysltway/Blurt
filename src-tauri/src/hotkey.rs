//! 全局快捷键：解析、注册（强制替换语义）、捕获期挂起、按键松开与 Esc 的轮询看门。

use std::sync::atomic::AtomicU32;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

/// A global shortcut contains at most one modifier and one primary key.
pub const MAX_HOTKEY_KEYS: usize = 2;

static CAPTURE_HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// "ctrl+Space" → Shortcut。修饰键小写；主键用 W3C Code 名（Space/KeyA/F8…）
pub fn parse_shortcut(s: &str) -> Result<Shortcut, String> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    let mut key_count = 0;
    for raw_part in s.split('+') {
        let part = raw_part.trim();
        if part.is_empty() {
            return Err("快捷键包含空按键".to_string());
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                if mods.contains(Modifiers::CONTROL) {
                    return Err("快捷键不能重复包含 Ctrl".to_string());
                }
                mods |= Modifiers::CONTROL;
                key_count += 1;
            }
            "alt" | "option" => {
                if mods.contains(Modifiers::ALT) {
                    return Err("快捷键不能重复包含 Alt".to_string());
                }
                mods |= Modifiers::ALT;
                key_count += 1;
            }
            "shift" => {
                if mods.contains(Modifiers::SHIFT) {
                    return Err("快捷键不能重复包含 Shift".to_string());
                }
                mods |= Modifiers::SHIFT;
                key_count += 1;
            }
            "super" | "win" | "meta" | "cmd" => {
                if mods.contains(Modifiers::SUPER) {
                    return Err("快捷键不能重复包含 Win".to_string());
                }
                mods |= Modifiers::SUPER;
                key_count += 1;
            }
            _ => {
                if code.is_some() {
                    return Err(format!("全局快捷键最多包含 {MAX_HOTKEY_KEYS} 个按键"));
                }
                code = Some(
                    part.parse::<Code>()
                        .map_err(|_| format!("无法识别的按键：{part}"))?,
                );
                key_count += 1;
            }
        }
    }
    if key_count > MAX_HOTKEY_KEYS {
        return Err(format!("全局快捷键最多包含 {MAX_HOTKEY_KEYS} 个按键"));
    }
    let code = code.ok_or_else(|| "快捷键缺少主键".to_string())?;
    let m = if mods.is_empty() { None } else { Some(mods) };
    Ok(Shortcut::new(m, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_modifier_and_primary_key() {
        let shortcut = parse_shortcut("ctrl+Space").expect("two-key shortcut should parse");
        assert_eq!(shortcut.mods, Modifiers::CONTROL);
        assert_eq!(shortcut.key, Code::Space);
    }

    #[test]
    fn rejects_more_than_two_keys() {
        for value in ["ctrl+alt+Space", "ctrl+shift+Space", "ctrl+Space+KeyA"] {
            let error = parse_shortcut(value).expect_err("three-key shortcut should be rejected");
            assert!(error.contains("最多包含 2 个按键"), "{value}: {error}");
        }
    }

    #[test]
    fn rejects_duplicate_modifiers() {
        let error = parse_shortcut("ctrl+ctrl+Space").expect_err("duplicate modifier");
        assert!(error.contains("不能重复"));
    }

    #[test]
    fn rejects_empty_tokens() {
        let error = parse_shortcut("ctrl++Space").expect_err("empty token");
        assert!(error.contains("空按键"));
    }

    #[test]
    fn capture_state_records_regular_and_system_combinations() {
        let mut ctrl = CaptureKeyState::default();
        assert_eq!(ctrl.handle(0x0100, 0xA2), None);
        assert_eq!(
            ctrl.handle(0x0100, 0x41),
            Some(CaptureEvent::Captured("ctrl+KeyA".to_string()))
        );

        let mut alt = CaptureKeyState::default();
        assert_eq!(alt.handle(0x0104, 0xA4), None);
        assert_eq!(
            alt.handle(0x0104, 0x20),
            Some(CaptureEvent::Captured("alt+Space".to_string()))
        );
    }

    #[test]
    fn capture_state_handles_escape_and_rejects_three_keys() {
        let mut cancel = CaptureKeyState::default();
        assert_eq!(cancel.handle(0x0100, 0x1B), Some(CaptureEvent::Cancel));

        let mut overlong = CaptureKeyState::default();
        assert_eq!(overlong.handle(0x0100, 0xA2), None);
        assert_eq!(overlong.handle(0x0104, 0xA4), None);
        assert_eq!(overlong.handle(0x0100, 0x41), Some(CaptureEvent::Invalid));
    }
}

/// 强制注册主快捷键：先注销旧的再注册新的；失败时尝试回滚旧键。
pub fn register_main(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let state = app.state::<crate::app::AppState>();
    let new = parse_shortcut(hotkey)?;
    let gs = app.global_shortcut();

    let old = *state.main_shortcut.lock();
    if let Some(old) = old {
        let _ = gs.unregister(old);
    }
    match gs.register(new) {
        Ok(()) => {
            *state.main_shortcut.lock() = Some(new);
            Ok(())
        }
        Err(e) => {
            if let Some(old) = old {
                let _ = gs.register(old); // 回滚
            }
            Err(format!("快捷键注册失败（可能被其他程序占用）：{e}"))
        }
    }
}

/// 捕获新快捷键期间挂起全局热键 —— 否则 RegisterHotKey 会在系统层吞掉按键，
/// 设置页的输入框根本收不到 keydown（按当前热键还会误触录音）。
pub fn suspend_main(app: &AppHandle) {
    let state = app.state::<crate::app::AppState>();
    if let Some(sc) = *state.main_shortcut.lock() {
        let _ = app.global_shortcut().unregister(sc);
    }
    tracing::info!("全局快捷键已挂起（正在捕获新键）");
}

/// 捕获结束（成功与否都要调用）：按当前配置恢复注册
pub fn resume_main(app: &AppHandle) -> Result<(), String> {
    let hotkey = {
        let state = app.state::<crate::app::AppState>();
        let h = state.config.read().hotkey.clone();
        h
    };
    register_main(app, &hotkey)
}

/// W3C Code → Windows 虚拟键码（用于兜底检测“按键已松开”）
pub fn vk_for_code(code: Code) -> Option<u16> {
    use Code::*;
    let vk = match code {
        KeyA => 0x41,
        KeyB => 0x42,
        KeyC => 0x43,
        KeyD => 0x44,
        KeyE => 0x45,
        KeyF => 0x46,
        KeyG => 0x47,
        KeyH => 0x48,
        KeyI => 0x49,
        KeyJ => 0x4A,
        KeyK => 0x4B,
        KeyL => 0x4C,
        KeyM => 0x4D,
        KeyN => 0x4E,
        KeyO => 0x4F,
        KeyP => 0x50,
        KeyQ => 0x51,
        KeyR => 0x52,
        KeyS => 0x53,
        KeyT => 0x54,
        KeyU => 0x55,
        KeyV => 0x56,
        KeyW => 0x57,
        KeyX => 0x58,
        KeyY => 0x59,
        KeyZ => 0x5A,
        Digit0 => 0x30,
        Digit1 => 0x31,
        Digit2 => 0x32,
        Digit3 => 0x33,
        Digit4 => 0x34,
        Digit5 => 0x35,
        Digit6 => 0x36,
        Digit7 => 0x37,
        Digit8 => 0x38,
        Digit9 => 0x39,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        F13 => 0x7C,
        F14 => 0x7D,
        F15 => 0x7E,
        F16 => 0x7F,
        F17 => 0x80,
        F18 => 0x81,
        F19 => 0x82,
        F20 => 0x83,
        F21 => 0x84,
        F22 => 0x85,
        F23 => 0x86,
        F24 => 0x87,
        Space => 0x20,
        ArrowLeft => 0x25,
        ArrowUp => 0x26,
        ArrowRight => 0x27,
        ArrowDown => 0x28,
        Home => 0x24,
        End => 0x23,
        PageUp => 0x21,
        PageDown => 0x22,
        Insert => 0x2D,
        Delete => 0x2E,
        Backquote => 0xC0,
        Minus => 0xBD,
        Equal => 0xBB,
        BracketLeft => 0xDB,
        BracketRight => 0xDD,
        Backslash => 0xDC,
        Semicolon => 0xBA,
        Quote => 0xDE,
        Comma => 0xBC,
        Period => 0xBE,
        Slash => 0xBF,
        _ => return None,
    };
    Some(vk)
}

/// Windows 虚拟键码 → W3C Code（原生捕获用）
fn vk_to_code(vk: u32) -> Option<Code> {
    use Code::*;
    let code = match vk {
        0x41..=0x5A => match vk {
            0x41 => KeyA,
            0x42 => KeyB,
            0x43 => KeyC,
            0x44 => KeyD,
            0x45 => KeyE,
            0x46 => KeyF,
            0x47 => KeyG,
            0x48 => KeyH,
            0x49 => KeyI,
            0x4A => KeyJ,
            0x4B => KeyK,
            0x4C => KeyL,
            0x4D => KeyM,
            0x4E => KeyN,
            0x4F => KeyO,
            0x50 => KeyP,
            0x51 => KeyQ,
            0x52 => KeyR,
            0x53 => KeyS,
            0x54 => KeyT,
            0x55 => KeyU,
            0x56 => KeyV,
            0x57 => KeyW,
            0x58 => KeyX,
            0x59 => KeyY,
            _ => KeyZ,
        },
        0x30 => Digit0,
        0x31 => Digit1,
        0x32 => Digit2,
        0x33 => Digit3,
        0x34 => Digit4,
        0x35 => Digit5,
        0x36 => Digit6,
        0x37 => Digit7,
        0x38 => Digit8,
        0x39 => Digit9,
        0x70 => F1,
        0x71 => F2,
        0x72 => F3,
        0x73 => F4,
        0x74 => F5,
        0x75 => F6,
        0x76 => F7,
        0x77 => F8,
        0x78 => F9,
        0x79 => F10,
        0x7A => F11,
        0x7B => F12,
        0x7C => F13,
        0x7D => F14,
        0x7E => F15,
        0x7F => F16,
        0x80 => F17,
        0x81 => F18,
        0x82 => F19,
        0x83 => F20,
        0x84 => F21,
        0x85 => F22,
        0x86 => F23,
        0x87 => F24,
        0x20 => Space,
        0x25 => ArrowLeft,
        0x26 => ArrowUp,
        0x27 => ArrowRight,
        0x28 => ArrowDown,
        0x24 => Home,
        0x23 => End,
        0x21 => PageUp,
        0x22 => PageDown,
        0x2D => Insert,
        0x2E => Delete,
        0xC0 => Backquote,
        0xBD => Minus,
        0xBB => Equal,
        0xDB => BracketLeft,
        0xDD => BracketRight,
        0xDC => Backslash,
        0xBA => Semicolon,
        0xDE => Quote,
        0xBC => Comma,
        0xBE => Period,
        0xBF => Slash,
        _ => return None,
    };
    Some(code)
}

#[derive(Default)]
struct CaptureModifiers {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
}

impl CaptureModifiers {
    fn set(&mut self, vk: u32, pressed: bool) -> bool {
        match vk {
            0x10 | 0xA0 | 0xA1 => self.shift = pressed,
            0x11 | 0xA2 | 0xA3 => self.ctrl = pressed,
            0x12 | 0xA4 | 0xA5 => self.alt = pressed,
            0x5B | 0x5C => self.win = pressed,
            _ => return false,
        }
        true
    }

    fn count(&self) -> usize {
        [self.ctrl, self.alt, self.shift, self.win]
            .into_iter()
            .filter(|pressed| *pressed)
            .count()
    }

    fn any(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CaptureEvent {
    Captured(String),
    Invalid,
    Cancel,
}

#[derive(Default)]
struct CaptureKeyState {
    modifiers: CaptureModifiers,
    ignored_key: Option<u32>,
    done: bool,
}

impl CaptureKeyState {
    fn handle(&mut self, message: u32, vk: u32) -> Option<CaptureEvent> {
        const WM_KEYDOWN: u32 = 0x0100;
        const WM_KEYUP: u32 = 0x0101;
        const WM_SYSKEYDOWN: u32 = 0x0104;
        const WM_SYSKEYUP: u32 = 0x0105;

        if self.done {
            return None;
        }

        if message == WM_KEYUP || message == WM_SYSKEYUP {
            self.modifiers.set(vk, false);
            if self.ignored_key == Some(vk) {
                self.ignored_key = None;
            }
            return None;
        }
        if message != WM_KEYDOWN && message != WM_SYSKEYDOWN {
            return None;
        }
        if self.modifiers.set(vk, true) {
            return None;
        }
        if vk == 0x1B {
            self.done = true;
            return Some(CaptureEvent::Cancel);
        }
        if self.ignored_key.is_some() {
            return None;
        }

        let code = vk_to_code(vk)?;
        let is_function_key = (0x70..=0x87).contains(&vk);
        if self.modifiers.count() + 1 > MAX_HOTKEY_KEYS
            || (!self.modifiers.any() && !is_function_key)
        {
            self.ignored_key = Some(vk);
            return Some(CaptureEvent::Invalid);
        }

        let mut parts = Vec::with_capacity(MAX_HOTKEY_KEYS);
        if self.modifiers.ctrl {
            parts.push("ctrl".to_string());
        }
        if self.modifiers.alt {
            parts.push("alt".to_string());
        }
        if self.modifiers.shift {
            parts.push("shift".to_string());
        }
        if self.modifiers.win {
            parts.push("super".to_string());
        }
        parts.push(code.to_string());
        self.done = true;
        Some(CaptureEvent::Captured(parts.join("+")))
    }
}

const WM_CAPTURE_KEY: u32 = 0x8001;

unsafe extern "system" fn capture_keyboard_hook(
    hook_code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, PostThreadMessageW, HC_ACTION, KBDLLHOOKSTRUCT,
    };

    if hook_code == HC_ACTION as i32 && lparam.0 != 0 {
        let key = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let thread_id = CAPTURE_HOOK_THREAD_ID.load(Ordering::SeqCst);
        if thread_id != 0 {
            let _ = unsafe {
                PostThreadMessageW(
                    thread_id,
                    WM_CAPTURE_KEY,
                    WPARAM(key.vkCode as usize),
                    LPARAM(wparam.0 as isize),
                )
            };
        }
    }

    unsafe { CallNextHookEx(None, hook_code, wparam, lparam) }
}

fn emit_capture_event(app: &AppHandle, event: CaptureEvent) -> bool {
    use tauri::Emitter;

    match event {
        CaptureEvent::Captured(hotkey) => {
            tracing::info!("低级键盘钩子捕获到快捷键：{hotkey}");
            let _ = app.emit_to(
                "settings",
                "hotkey:captured",
                serde_json::json!({ "hotkey": hotkey }),
            );
            true
        }
        CaptureEvent::Invalid => {
            let _ = app.emit_to("settings", "hotkey:capture_invalid", serde_json::json!({}));
            false
        }
        CaptureEvent::Cancel => {
            let _ = app.emit_to("settings", "hotkey:capture_cancel", serde_json::json!({}));
            true
        }
    }
}

fn run_capture_hook(
    app: AppHandle,
    gen: u64,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    use std::sync::atomic::Ordering;
    use tauri::Emitter;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, PM_NOREMOVE,
        WH_KEYBOARD_LL,
    };

    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };

    let state = app.state::<crate::app::AppState>();
    state.capture_thread_id.store(thread_id, Ordering::SeqCst);
    if state.capture_gen.load(Ordering::SeqCst) != gen {
        let _ = state.capture_thread_id.compare_exchange(
            thread_id,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        let _ = ready.send(Err("快捷键捕获已取消".to_string()));
        return;
    }
    drop(state);

    CAPTURE_HOOK_THREAD_ID.store(thread_id, Ordering::SeqCst);

    let hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(capture_keyboard_hook), None, 0) } {
            Ok(hook) => hook,
            Err(error) => {
                let _ = CAPTURE_HOOK_THREAD_ID.compare_exchange(
                    thread_id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                let state = app.state::<crate::app::AppState>();
                let _ = state.capture_thread_id.compare_exchange(
                    thread_id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                let _ = ready.send(Err(format!("安装键盘监听器失败：{error}")));
                return;
            }
        };

    tracing::info!("低级键盘捕获钩子已启动（线程 {thread_id}）");
    let _ = ready.send(Ok(()));
    let mut message_error = None;
    let mut keys = CaptureKeyState::default();

    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            message_error = Some("键盘监听消息循环异常退出".to_string());
            break;
        }
        if result.0 == 0 {
            break;
        }
        if message.message != WM_CAPTURE_KEY {
            continue;
        }

        if app
            .state::<crate::app::AppState>()
            .capture_gen
            .load(Ordering::SeqCst)
            != gen
        {
            break;
        }
        let event = keys.handle(message.lParam.0 as u32, message.wParam.0 as u32);
        if event.is_some_and(|event| emit_capture_event(&app, event)) {
            break;
        }
    }

    if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
        tracing::error!("卸载键盘捕获钩子失败：{error}");
    }
    let _ =
        CAPTURE_HOOK_THREAD_ID.compare_exchange(thread_id, 0, Ordering::SeqCst, Ordering::SeqCst);
    let state = app.state::<crate::app::AppState>();
    let _ =
        state
            .capture_thread_id
            .compare_exchange(thread_id, 0, Ordering::SeqCst, Ordering::SeqCst);
    drop(state);
    tracing::info!("低级键盘捕获钩子已停止（线程 {thread_id}）");

    if let Some(error) = message_error {
        tracing::error!("{error}");
        let _ = app.emit_to(
            "settings",
            "hotkey:capture_error",
            serde_json::json!({ "message": error }),
        );
    }
}

/// 原生快捷键捕获：安装 WH_KEYBOARD_LL 低级键盘钩子并运行 Windows 消息循环。
/// 这与 rdev/global keyboard listener 的 Windows 实现一致，可捕获被输入法或系统菜单
/// 截获的 Ctrl+Space、Alt+Space 和 Esc。函数会等待钩子安装完成后再返回。
pub fn spawn_capture(app: &AppHandle, gen: u64) -> Result<(), String> {
    use std::sync::mpsc::{sync_channel, RecvTimeoutError};
    use std::time::Duration;

    let (ready_tx, ready_rx) = sync_channel(1);
    let app = app.clone();
    std::thread::Builder::new()
        .name("blurt-hotkey-capture".to_string())
        .spawn(move || run_capture_hook(app, gen, ready_tx))
        .map_err(|error| format!("启动键盘监听线程失败：{error}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err("启动键盘监听器超时".to_string()),
        Err(RecvTimeoutError::Disconnected) => Err("键盘监听线程异常退出".to_string()),
    }
}

/// 停止当前捕获线程。线程消息队列在安装钩子前已创建，因此 WM_QUIT 不会丢失。
pub fn stop_capture(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};

    let thread_id = app
        .state::<crate::app::AppState>()
        .capture_thread_id
        .swap(0, Ordering::SeqCst);
    if thread_id == 0 {
        return;
    }
    let _ =
        CAPTURE_HOOK_THREAD_ID.compare_exchange(thread_id, 0, Ordering::SeqCst, Ordering::SeqCst);
    if let Err(error) = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) } {
        tracing::debug!("停止键盘捕获线程 {thread_id} 时消息投递失败：{error}");
    }
}

/// 兜底松开监视：插件的 Released 事件之外再轮询一层，确保“按住说话”一定能停下。
pub fn spawn_release_watcher(app: &AppHandle, gen: u64, code: Code) {
    let Some(vk) = vk_for_code(code) else { return };
    let app = app.clone();
    std::thread::spawn(move || {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        std::thread::sleep(std::time::Duration::from_millis(30));
        loop {
            {
                let state = app.state::<crate::app::AppState>();
                if state.gen.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return;
                }
            }
            let down = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            if !down {
                crate::app::hotkey_released(&app, Some(gen));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
    });
}

/// 会话期间轮询 Esc（录音 + 识别全程随时可取消）。
/// 用轮询而非全局注册 Esc：更可靠，也不会与其他应用抢占 Esc 键。
pub fn spawn_esc_watcher(app: &AppHandle, gen: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        const VK_ESCAPE: i32 = 0x1B;
        // 若进入时 Esc 恰好按着，先等抬起，避免误触
        while unsafe { GetAsyncKeyState(VK_ESCAPE) } as u16 & 0x8000 != 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        loop {
            {
                let state = app.state::<crate::app::AppState>();
                if state.gen.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return; // 会话已换代/取消
                }
                if matches!(&*state.session.lock(), crate::app::Session::Idle) {
                    return; // 会话已正常收尾
                }
            }
            if unsafe { GetAsyncKeyState(VK_ESCAPE) } as u16 & 0x8000 != 0 {
                tracing::info!("Esc → 取消当前会话");
                crate::app::esc_pressed(&app);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    });
}
