//! 可配置的全局快捷键。
//!
//! RegisterHotKey 不支持纯修饰键组合，例如默认的 Ctrl+Alt。这里使用常驻
//! WH_KEYBOARD_LL 钩子，同时支持纯修饰键与带主键的组合，例如 Ctrl+Shift+K。

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use tauri::{AppHandle, Manager};

static CHORD_HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

const WM_CHORD_KEY: u32 = 0x8001;

const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;

const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
    key: Option<u32>,
}

impl Default for Shortcut {
    fn default() -> Self {
        Self {
            ctrl: true,
            alt: true,
            shift: false,
            win: false,
            key: None,
        }
    }
}

impl Shortcut {
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut shortcut = Self {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key: None,
        };

        for token in value
            .split('+')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => set_modifier(&mut shortcut.ctrl, "Ctrl")?,
                "alt" => set_modifier(&mut shortcut.alt, "Alt")?,
                "shift" => set_modifier(&mut shortcut.shift, "Shift")?,
                "win" | "windows" | "meta" => set_modifier(&mut shortcut.win, "Win")?,
                _ => {
                    if shortcut.key.is_some() {
                        return Err("快捷键只能包含一个主键".into());
                    }
                    shortcut.key = Some(parse_key(token)?);
                }
            }
        }

        let modifier_count = shortcut.modifier_count();
        if modifier_count == 0 {
            return Err("快捷键至少需要一个修饰键".into());
        }
        if shortcut.key.is_none() && modifier_count < 2 {
            return Err("纯修饰键快捷键至少需要两个按键".into());
        }
        Ok(shortcut)
    }

    fn modifier_count(&self) -> u8 {
        self.ctrl as u8 + self.alt as u8 + self.shift as u8 + self.win as u8
    }

    fn matches_modifiers(&self, mods: &ChordMods) -> bool {
        self.ctrl == mods.ctrl()
            && self.alt == mods.alt()
            && self.shift == mods.shift()
            && self.win == mods.win()
    }

    fn is_held(&self) -> bool {
        (!self.ctrl || vk_is_down(VK_CONTROL))
            && (!self.alt || vk_is_down(VK_MENU))
            && (!self.shift || vk_is_down(VK_SHIFT))
            && (!self.win || vk_is_down(VK_LWIN) || vk_is_down(VK_RWIN))
            && self.key.is_none_or(vk_is_down)
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::with_capacity(5);
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        if let Some(key) = self.key {
            parts.push(key_name(key));
        }
        f.write_str(&parts.join("+"))
    }
}

pub fn canonicalize(value: &str) -> Result<String, String> {
    Shortcut::parse(value).map(|shortcut| shortcut.to_string())
}

fn set_modifier(value: &mut bool, name: &str) -> Result<(), String> {
    if *value {
        return Err(format!("{name} 重复出现"));
    }
    *value = true;
    Ok(())
}

fn parse_key(token: &str) -> Result<u32, String> {
    let token = token.trim();
    let upper = token.to_ascii_uppercase();
    let key = match upper.as_str() {
        "SPACE" => 0x20,
        "TAB" => 0x09,
        "ENTER" => 0x0D,
        "BACKSPACE" => 0x08,
        "INSERT" => 0x2D,
        "DELETE" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" => 0x21,
        "PAGEDOWN" => 0x22,
        "UP" | "ARROWUP" => 0x26,
        "DOWN" | "ARROWDOWN" => 0x28,
        "LEFT" | "ARROWLEFT" => 0x25,
        "RIGHT" | "ARROWRIGHT" => 0x27,
        _ if upper.len() == 1 && upper.as_bytes()[0].is_ascii_alphabetic() => {
            upper.as_bytes()[0] as u32
        }
        _ if upper.len() == 1 && upper.as_bytes()[0].is_ascii_digit() => upper.as_bytes()[0] as u32,
        _ if let Some(number) = upper.strip_prefix('F') => {
            let number = number
                .parse::<u32>()
                .map_err(|_| format!("不支持的主键：{token}"))?;
            if !(1..=24).contains(&number) {
                return Err(format!("不支持的主键：{token}"));
            }
            0x70 + number - 1
        }
        _ => return Err(format!("不支持的主键：{token}")),
    };
    Ok(key)
}

fn key_name(vk: u32) -> String {
    match vk {
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x08 => "Backspace".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x26 => "Up".into(),
        0x28 => "Down".into(),
        0x25 => "Left".into(),
        0x27 => "Right".into(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => char::from_u32(vk).map_or_else(|| format!("VK{vk}"), |key| key.to_string()),
    }
}

fn vk_is_down(vk: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    (unsafe { GetAsyncKeyState(vk as i32) }) as u16 & 0x8000 != 0
}

/// Physical modifier-key state reconstructed from the low-level key stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ChordMods {
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lshift: bool,
    rshift: bool,
    lwin: bool,
    rwin: bool,
}

impl fmt::Display for ChordMods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::with_capacity(4);
        if self.ctrl() {
            parts.push("Ctrl");
        }
        if self.alt() {
            parts.push("Alt");
        }
        if self.shift() {
            parts.push("Shift");
        }
        if self.win() {
            parts.push("Win");
        }
        if parts.is_empty() {
            f.write_str("None")
        } else {
            f.write_str(&parts.join("+"))
        }
    }
}

impl ChordMods {
    fn set(&mut self, vk: u32, pressed: bool) -> bool {
        match vk {
            0x10 | 0xA0 => self.lshift = pressed,
            0xA1 => self.rshift = pressed,
            0x11 | 0xA2 => self.lctrl = pressed,
            0xA3 => self.rctrl = pressed,
            0x12 | 0xA4 => self.lalt = pressed,
            0xA5 => self.ralt = pressed,
            VK_LWIN => self.lwin = pressed,
            VK_RWIN => self.rwin = pressed,
            _ => return false,
        }
        true
    }

    fn ctrl(&self) -> bool {
        self.lctrl || self.rctrl
    }

    fn alt(&self) -> bool {
        self.lalt || self.ralt
    }

    fn shift(&self) -> bool {
        self.lshift || self.rshift
    }

    fn win(&self) -> bool {
        self.lwin || self.rwin
    }

    fn clear(&self) -> bool {
        !self.ctrl() && !self.alt() && !self.shift() && !self.win()
    }

    /// 仅在物理按键确实未按下时清除虚假按键（修正锁屏 Win+L 等场景遗留的修饰键）。
    fn sync_physical(&mut self) {
        if self.lshift && !vk_is_down(0xA0) && !vk_is_down(VK_SHIFT) {
            self.lshift = false;
        }
        if self.rshift && !vk_is_down(0xA1) && !vk_is_down(VK_SHIFT) {
            self.rshift = false;
        }
        if self.lctrl && !vk_is_down(0xA2) && !vk_is_down(VK_CONTROL) {
            self.lctrl = false;
        }
        if self.rctrl && !vk_is_down(0xA3) && !vk_is_down(VK_CONTROL) {
            self.rctrl = false;
        }
        if self.lalt && !vk_is_down(0xA4) && !vk_is_down(VK_MENU) {
            self.lalt = false;
        }
        if self.ralt && !vk_is_down(0xA5) && !vk_is_down(VK_MENU) {
            self.ralt = false;
        }
        if self.lwin && !vk_is_down(VK_LWIN) {
            self.lwin = false;
        }
        if self.rwin && !vk_is_down(VK_RWIN) {
            self.rwin = false;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChordPhase {
    #[default]
    Idle,
    Active,
    Contaminated,
}

#[derive(Debug, PartialEq, Eq)]
enum ChordEvent {
    Pressed,
    Released,
    Broken,
}

/// Detects the configured shortcut from raw key events.
#[derive(Default)]
struct ChordState {
    mods: ChordMods,
    phase: ChordPhase,
    active_shortcut: Option<Shortcut>,
}

impl ChordState {
    fn reset(&mut self) {
        self.phase = ChordPhase::Idle;
        self.active_shortcut = None;
    }

    fn clear_for_capture(&mut self) {
        self.mods = ChordMods::default();
        self.reset();
    }

    fn activate(&mut self, shortcut: Shortcut) {
        self.phase = ChordPhase::Active;
        self.active_shortcut = Some(shortcut);
    }

    fn handle(&mut self, message: u32, vk: u32, selected: Shortcut) -> Option<ChordEvent> {
        let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if !pressed && !matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            return None;
        }

        let is_mod = self.mods.set(vk, pressed);
        if cfg!(not(test)) {
            self.mods.sync_physical();
        }

        if is_mod {
            return match self.phase {
                ChordPhase::Idle => {
                    if selected.key.is_none() && selected.matches_modifiers(&self.mods) {
                        self.activate(selected);
                        Some(ChordEvent::Pressed)
                    } else {
                        None
                    }
                }
                ChordPhase::Active => {
                    let active = self.active_shortcut.unwrap_or(selected);
                    if active.matches_modifiers(&self.mods) {
                        None
                    } else if pressed {
                        self.phase = ChordPhase::Contaminated;
                        Some(ChordEvent::Broken)
                    } else {
                        self.reset();
                        Some(ChordEvent::Released)
                    }
                }
                ChordPhase::Contaminated => {
                    if selected.key.is_none() && selected.matches_modifiers(&self.mods) {
                        self.activate(selected);
                        Some(ChordEvent::Pressed)
                    } else if self.mods.clear() || !pressed {
                        self.reset();
                        None
                    } else {
                        None
                    }
                }
            };
        }

        match self.phase {
            ChordPhase::Idle => {
                if pressed && selected.key == Some(vk) && selected.matches_modifiers(&self.mods) {
                    self.activate(selected);
                    Some(ChordEvent::Pressed)
                } else {
                    None
                }
            }
            ChordPhase::Active => {
                let active = self.active_shortcut.unwrap_or(selected);
                if active.key == Some(vk) {
                    if pressed {
                        None
                    } else {
                        self.reset();
                        Some(ChordEvent::Released)
                    }
                } else if pressed {
                    self.phase = ChordPhase::Contaminated;
                    Some(ChordEvent::Broken)
                } else {
                    None
                }
            }
            ChordPhase::Contaminated => {
                if pressed && selected.key == Some(vk) && selected.matches_modifiers(&self.mods) {
                    self.activate(selected);
                    Some(ChordEvent::Pressed)
                } else if self.mods.clear() {
                    self.reset();
                    None
                } else {
                    None
                }
            }
        }
    }
}

fn configured_shortcut(app: &AppHandle) -> Shortcut {
    let hotkey = app
        .state::<crate::app::AppState>()
        .config
        .read()
        .hotkey
        .clone();
    Shortcut::parse(&hotkey).unwrap_or_else(|error| {
        tracing::error!("快捷键配置无效，已回退到 Ctrl+Alt：{error}");
        Shortcut::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u32 = 0xA2;
    const LALT: u32 = 0xA4;
    const LSHIFT: u32 = 0xA0;
    const KEY_A: u32 = 0x41;
    const KEY_K: u32 = 0x4B;

    fn shortcut(value: &str) -> Shortcut {
        Shortcut::parse(value).unwrap()
    }

    #[test]
    fn parses_and_normalizes_shortcuts() {
        assert_eq!(canonicalize("shift + control + k").unwrap(), "Ctrl+Shift+K");
        assert_eq!(canonicalize("alt+space").unwrap(), "Alt+Space");
        assert!(canonicalize("Ctrl").is_err());
        assert!(canonicalize("Ctrl+Alt+K+L").is_err());
    }

    #[test]
    fn pure_modifier_shortcut_presses_and_releases() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Alt");
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        assert_eq!(
            state.handle(WM_SYSKEYDOWN, LALT, shortcut),
            Some(ChordEvent::Pressed)
        );
        assert_eq!(
            state.handle(WM_SYSKEYUP, LALT, shortcut),
            Some(ChordEvent::Released)
        );
    }

    #[test]
    fn pure_modifier_shortcut_rearms_while_one_modifier_stays_held() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Alt");
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        assert_eq!(
            state.handle(WM_SYSKEYDOWN, LALT, shortcut),
            Some(ChordEvent::Pressed)
        );
        assert_eq!(
            state.handle(WM_SYSKEYUP, LALT, shortcut),
            Some(ChordEvent::Released)
        );
        assert_eq!(
            state.handle(WM_SYSKEYDOWN, LALT, shortcut),
            Some(ChordEvent::Pressed)
        );
    }

    #[test]
    fn primary_key_shortcut_starts_on_primary_key_and_stops_on_release() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Shift+K");
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        assert_eq!(state.handle(WM_KEYDOWN, LSHIFT, shortcut), None);
        assert_eq!(
            state.handle(WM_KEYDOWN, KEY_K, shortcut),
            Some(ChordEvent::Pressed)
        );
        assert_eq!(
            state.handle(WM_KEYUP, KEY_K, shortcut),
            Some(ChordEvent::Released)
        );
    }

    #[test]
    fn extra_key_breaks_an_active_shortcut() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Alt");
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        assert_eq!(
            state.handle(WM_SYSKEYDOWN, LALT, shortcut),
            Some(ChordEvent::Pressed)
        );
        assert_eq!(
            state.handle(WM_KEYDOWN, KEY_A, shortcut),
            Some(ChordEvent::Broken)
        );
    }

    #[test]
    fn stuck_modifier_self_heals_on_sync() {
        let mut mods = ChordMods::default();
        mods.lwin = true;
        mods.rctrl = true;
        assert!(mods.win());
        assert!(mods.ctrl());

        // 当物理按键并未按下时，sync_physical 会自动清除虚假按键
        mods.sync_physical();
        assert!(!mods.lwin);
        assert!(!mods.rctrl);
        assert!(mods.clear());
    }

    #[test]
    fn recovers_from_stuck_win_key_and_matches_ctrl_alt() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Alt");

        // 模拟 Win+L 锁屏后遗留的 lwin=true
        state.mods.lwin = true;
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        // 在没有 sync_physical 时，按下 Alt 后因 lwin=true 不匹配
        assert_eq!(state.handle(WM_SYSKEYDOWN, LALT, shortcut), None);

        // 执行 sync_physical 后，物理同步会立刻修正 lwin
        state.mods.lwin = true;
        state.mods.sync_physical();
        assert!(!state.mods.win());
    }

    #[test]
    fn contaminated_phase_self_heals_when_modifier_repressed() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Alt");

        // 1. 用户按下 Ctrl + Alt 触发快捷键
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        assert_eq!(
            state.handle(WM_SYSKEYDOWN, LALT, shortcut),
            Some(ChordEvent::Pressed)
        );

        // 2. 此时用户按下其他键（如日常截图 Ctrl+Alt+A），状态进入 Contaminated，录音打断
        assert_eq!(
            state.handle(WM_KEYDOWN, KEY_A, shortcut),
            Some(ChordEvent::Broken)
        );
        assert_eq!(state.handle(WM_KEYUP, KEY_A, shortcut), None);

        // 3. 用户松开 Alt，但手指依然按住 Ctrl
        assert_eq!(state.handle(WM_SYSKEYUP, LALT, shortcut), None);

        // 4. 用户再次按下 Alt，意图重新触发 Ctrl+Alt 进行语音输入
        // 验证自愈机制：即使 Ctrl 一直未松开，再次按下 Alt 能够立即自愈并触发 Pressed！
        let event = state.handle(WM_SYSKEYDOWN, LALT, shortcut);
        assert_eq!(
            event,
            Some(ChordEvent::Pressed),
            "自愈：在 Contaminated 态下重新按下快捷键应成功触发 Pressed"
        );
        // 5. 再次松开 Alt 能够正常发出 Released
        assert_eq!(
            state.handle(WM_SYSKEYUP, LALT, shortcut),
            Some(ChordEvent::Released)
        );
    }

    #[test]
    fn primary_key_shortcut_self_heals_from_contaminated() {
        let mut state = ChordState::default();
        let shortcut = shortcut("Ctrl+Shift+K");

        // 1. 用户按下 Ctrl+Shift+K
        assert_eq!(state.handle(WM_KEYDOWN, LCTRL, shortcut), None);
        assert_eq!(state.handle(WM_KEYDOWN, LSHIFT, shortcut), None);
        assert_eq!(
            state.handle(WM_KEYDOWN, KEY_K, shortcut),
            Some(ChordEvent::Pressed)
        );

        // 2. 误触 A 键，被打断进入 Contaminated
        assert_eq!(
            state.handle(WM_KEYDOWN, KEY_A, shortcut),
            Some(ChordEvent::Broken)
        );
        assert_eq!(state.handle(WM_KEYUP, KEY_A, shortcut), None);

        // 3. 用户再次按下 K，自愈并触发 Pressed
        assert_eq!(
            state.handle(WM_KEYDOWN, KEY_K, shortcut),
            Some(ChordEvent::Pressed)
        );
        // 4. 松开 K 触发 Released
        assert_eq!(
            state.handle(WM_KEYUP, KEY_K, shortcut),
            Some(ChordEvent::Released)
        );
    }
}

unsafe extern "system" fn chord_keyboard_hook(
    hook_code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, PostThreadMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    };

    if hook_code == HC_ACTION as i32 && lparam.0 != 0 {
        let key = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // Synthetic input such as paste must never drive the shortcut.
        if key.flags.0 & LLKHF_INJECTED.0 == 0 {
            let thread_id = CHORD_HOOK_THREAD_ID.load(Ordering::SeqCst);
            if thread_id != 0 {
                let _ = unsafe {
                    PostThreadMessageW(
                        thread_id,
                        WM_CHORD_KEY,
                        WPARAM(key.vkCode as usize),
                        LPARAM(wparam.0 as isize),
                    )
                };
            }
        }
    }

    unsafe { CallNextHookEx(None, hook_code, wparam, lparam) }
}

fn rearm_hook(hook: &mut windows::Win32::UI::WindowsAndMessaging::HHOOK) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    };
    if !hook.is_invalid() {
        let _ = unsafe { UnhookWindowsHookEx(*hook) };
    }
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(chord_keyboard_hook), None, 0) } {
        Ok(new_hook) => {
            *hook = new_hook;
            tracing::info!("已重新安装全局键盘钩子 (WH_KEYBOARD_LL)");
        }
        Err(error) => {
            tracing::error!("重新安装全局键盘钩子失败：{error}");
        }
    }
}

fn run_chord_hook(app: AppHandle, ready: std::sync::mpsc::SyncSender<Result<(), String>>) {
    use windows::core::w;
    use windows::Win32::System::RemoteDesktop::{
        WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, PeekMessageW,
        SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, CW_USEDEFAULT, HHOOK,
        MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_POWERBROADCAST, WM_TIMER, WM_WTSSESSION_CHANGE,
        WS_OVERLAPPEDWINDOW, WTS_SESSION_LOCK, WTS_SESSION_LOGON, WTS_SESSION_UNLOCK,
    };

    const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;
    const PBT_APMRESUMESUSPEND: u32 = 0x0007;

    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };

    // 创建隐藏窗口以监听 Windows 会话锁屏/解锁 (WM_WTSSESSION_CHANGE) 与系统休眠唤醒 (WM_POWERBROADCAST)
    let session_window = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("BlurtHotkeySessionWatcher"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            None,
            None,
        )
    };
    if let Ok(hwnd) = session_window {
        let _ = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) };
    }

    let (tx, rx) = std::sync::mpsc::channel::<ChordEvent>();
    let dispatch_app = app.clone();
    let dispatcher = std::thread::Builder::new()
        .name("blurt-hotkey-dispatch".to_string())
        .spawn(move || {
            for event in rx {
                match event {
                    ChordEvent::Pressed => {
                        tracing::info!("快捷键事件：Pressed → 触发录音");
                        crate::app::hotkey_pressed(&dispatch_app);
                    }
                    ChordEvent::Released => {
                        tracing::debug!("快捷键事件：Released → 松开");
                        crate::app::hotkey_released(&dispatch_app, None);
                    }
                    ChordEvent::Broken => {
                        tracing::info!("快捷键事件：Broken → 误触其他按键，打断录音");
                        crate::app::chord_broken(&dispatch_app);
                    }
                }
            }
        });
    if let Err(error) = dispatcher {
        let _ = ready.send(Err(format!("启动快捷键分发线程失败：{error}")));
        return;
    }

    CHORD_HOOK_THREAD_ID.store(thread_id, Ordering::SeqCst);
    let mut hook: HHOOK =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(chord_keyboard_hook), None, 0) } {
            Ok(hook) => hook,
            Err(error) => {
                CHORD_HOOK_THREAD_ID.store(0, Ordering::SeqCst);
                let _ = ready.send(Err(format!("安装全局快捷键监听失败：{error}")));
                return;
            }
        };
    let _ = ready.send(Ok(()));
    tracing::info!("全局快捷键低级键盘钩子已启动（线程 {thread_id}）");

    // 启动 30 秒看门狗定时器，定期清理按键漂移并自愈异常状态
    let watchdog_timer = unsafe { SetTimer(None, 0, 30_000, None) };

    let mut chord = ChordState::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            tracing::error!("全局快捷键监听消息循环异常退出");
            break;
        }
        if result.0 == 0 {
            break;
        }

        let _ = unsafe { TranslateMessage(&message) };
        let _ = unsafe { DispatchMessageW(&message) };

        match message.message {
            WM_CHORD_KEY => {
                let vk = message.wParam.0 as u32;
                let msg_type = message.lParam.0 as u32;
                let pressed = matches!(msg_type, WM_KEYDOWN | WM_SYSKEYDOWN);
                let shortcut = configured_shortcut(&app);

                if app
                    .state::<crate::app::AppState>()
                    .hotkey_capture
                    .load(Ordering::SeqCst)
                {
                    tracing::info!("[快捷键拦截] 设置页热键录制中 (hotkey_capture=true)，忽略按键 vk=0x{vk:02X} ({})", key_name(vk));
                    chord.clear_for_capture();
                    continue;
                }

                let before_phase = chord.phase;
                let before_mods = chord.mods;
                let event = chord.handle(msg_type, vk, shortcut);

                let is_modifier_key = matches!(vk, 0x10..=0x12 | 0x5B..=0x5C | 0xA0..=0xA5);
                let is_primary_key = shortcut.key == Some(vk);

                if let Some(event) = event {
                    tracing::info!(
                        "[快捷键事件] 产生事件: {:?} | 按键: vk=0x{:02X} ({}) | 修饰键: {} → {} | 状态: {:?} → {:?} | 目标: {}",
                        event,
                        vk,
                        key_name(vk),
                        before_mods,
                        chord.mods,
                        before_phase,
                        chord.phase,
                        shortcut
                    );
                    let _ = tx.send(event);
                } else if is_modifier_key || is_primary_key {
                    // 当用户按下了与快捷键相关的按键，但未能产生事件时，详细记录状态用于排查
                    tracing::info!(
                        "[按键处理] 未产生事件 | 按键: vk=0x{:02X} ({}, pressed={}) | 修饰键: {} → {} | 状态: {:?} → {:?} | 期望: {}",
                        vk,
                        key_name(vk),
                        pressed,
                        before_mods,
                        chord.mods,
                        before_phase,
                        chord.phase,
                        shortcut
                    );
                }
            }
            WM_TIMER => {
                // 定时自愈与状态维护
                chord.mods.sync_physical();
                // 兜底：如果设置窗口不存在但 hotkey_capture 处于 true，自动恢复
                let state = app.state::<crate::app::AppState>();
                if state.hotkey_capture.load(Ordering::SeqCst) {
                    let settings_open = app
                        .get_webview_window("settings")
                        .is_some_and(|w| w.is_visible().unwrap_or(false));
                    if !settings_open {
                        tracing::warn!(
                            "看门狗检测到设置窗口已关闭，自动重置 hotkey_capture 录制状态"
                        );
                        state.hotkey_capture.store(false, Ordering::SeqCst);
                    }
                }
            }
            WM_WTSSESSION_CHANGE => {
                let session_event = message.wParam.0 as u32;
                match session_event {
                    WTS_SESSION_LOCK => {
                        tracing::info!(
                            "检测到系统锁屏 (WTS_SESSION_LOCK) → 清空按键状态并取消录音"
                        );
                        chord.clear_for_capture();
                        crate::app::esc_pressed(&app);
                    }
                    WTS_SESSION_UNLOCK | WTS_SESSION_LOGON => {
                        tracing::info!(
                            "检测到系统解锁/登录 (0x{session_event:X}) → 重置按键状态并重装键盘钩子"
                        );
                        chord.clear_for_capture();
                        rearm_hook(&mut hook);
                    }
                    _ => {}
                }
            }
            WM_POWERBROADCAST => {
                let power_event = message.wParam.0 as u32;
                if power_event == PBT_APMRESUMEAUTOMATIC || power_event == PBT_APMRESUMESUSPEND {
                    tracing::info!(
                        "检测到系统从休眠/睡眠恢复 (0x{power_event:X}) → 重置按键状态并重装键盘钩子"
                    );
                    chord.clear_for_capture();
                    rearm_hook(&mut hook);
                }
            }
            _ => {}
        }
    }

    if watchdog_timer != 0 {
        let _ = unsafe { KillTimer(None, watchdog_timer) };
    }

    CHORD_HOOK_THREAD_ID.store(0, Ordering::SeqCst);
    if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
        tracing::error!("卸载全局快捷键钩子失败：{error}");
    }
    if let Ok(hwnd) = session_window {
        let _ = unsafe { WTSUnRegisterSessionNotification(hwnd) };
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    tracing::warn!("全局快捷键钩子已停止（正常情况下应常驻）");
}

/// 安装常驻的全局快捷键钩子；等待钩子真正挂上后才返回。
pub fn spawn_chord_hook(app: &AppHandle) -> Result<(), String> {
    use std::sync::mpsc::{sync_channel, RecvTimeoutError};
    use std::time::Duration;

    let (ready_tx, ready_rx) = sync_channel(1);
    let app = app.clone();
    std::thread::Builder::new()
        .name("blurt-hotkey".to_string())
        .spawn(move || run_chord_hook(app, ready_tx))
        .map_err(|error| format!("启动键盘监听线程失败：{error}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err("启动键盘监听器超时".to_string()),
        Err(RecvTimeoutError::Disconnected) => Err("键盘监听线程异常退出".to_string()),
    }
}

/// 兜底松开监视：钩子事件之外再轮询一层物理键态，确保“按住说话”一定能停。
pub fn spawn_release_watcher(app: &AppHandle, gen: u64, hotkey: String) {
    let app = app.clone();
    let shortcut = Shortcut::parse(&hotkey).unwrap_or_default();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        loop {
            {
                let state = app.state::<crate::app::AppState>();
                if state.gen.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return;
                }
            }
            if !shortcut.is_held() {
                crate::app::hotkey_released(&app, Some(gen));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
    });
}

/// 会话期间轮询 Esc（录音 + 识别全程随时可取消）。
pub fn spawn_esc_watcher(app: &AppHandle, gen: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        const VK_ESCAPE: i32 = 0x1B;
        while unsafe { GetAsyncKeyState(VK_ESCAPE) } as u16 & 0x8000 != 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        loop {
            {
                let state = app.state::<crate::app::AppState>();
                if state.gen.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return;
                }
                if matches!(&*state.session.lock(), crate::app::Session::Idle) {
                    return;
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
