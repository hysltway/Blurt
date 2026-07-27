//! 全局快捷键：代码写死的 Ctrl+Alt 和弦。
//!
//! RegisterHotKey（以及基于它的 global-shortcut 插件）要求组合里必须有一个
//! 主键，纯修饰键组合根本注册不上——这正是设置页始终捕获不到 Ctrl+Alt 的
//! 根因。因此这里用常驻的 WH_KEYBOARD_LL 低级键盘钩子自行检测：
//!   Ctrl+Alt 同时按下（且无其他修饰键）→ Pressed
//!   任一键松开 → Released
//!   按住期间又按了第三个键（Ctrl+Alt+A 截图之类）→ Broken，取消误触发的录音
//! 另含录音会话期的两个轮询看门：主和弦松开兜底与 Esc 取消。

use std::sync::atomic::{AtomicU32, Ordering};
use tauri::{AppHandle, Manager};

static CHORD_HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

const WM_CHORD_KEY: u32 = 0x8001;

const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;

/// Physical modifier-key state, reconstructed from the low-level key stream.
#[derive(Default)]
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

impl ChordMods {
    /// Updates one key's state; returns false when vk is not a modifier.
    fn set(&mut self, vk: u32, pressed: bool) -> bool {
        match vk {
            0x10 | 0xA0 => self.lshift = pressed,
            0xA1 => self.rshift = pressed,
            0x11 | 0xA2 => self.lctrl = pressed,
            0xA3 => self.rctrl = pressed,
            0x12 | 0xA4 => self.lalt = pressed,
            0xA5 => self.ralt = pressed,
            0x5B => self.lwin = pressed,
            0x5C => self.rwin = pressed,
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

    /// Any modifier beyond Ctrl/Alt — makes the combination a different one.
    fn extra(&self) -> bool {
        self.lshift || self.rshift || self.lwin || self.rwin
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChordPhase {
    /// Chord not held, or broken and not yet fully released.
    #[default]
    Idle,
    /// Exactly Ctrl+Alt held; a Pressed event has been emitted.
    Active,
    /// A third key joined while active: the user meant some other shortcut.
    Contaminated,
}

#[derive(Debug, PartialEq, Eq)]
enum ChordEvent {
    Pressed,
    Released,
    Broken,
}

/// Detects the "exactly Ctrl+Alt held" chord from raw key events.
#[derive(Default)]
struct ChordState {
    mods: ChordMods,
    phase: ChordPhase,
}

impl ChordState {
    fn handle(&mut self, message: u32, vk: u32) -> Option<ChordEvent> {
        let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if !pressed && !matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            return None;
        }

        if self.mods.set(vk, pressed) {
            return match self.phase {
                ChordPhase::Idle => {
                    (pressed && self.mods.ctrl() && self.mods.alt() && !self.mods.extra()).then(
                        || {
                            self.phase = ChordPhase::Active;
                            ChordEvent::Pressed
                        },
                    )
                }
                ChordPhase::Active => {
                    if !self.mods.ctrl() || !self.mods.alt() {
                        self.phase = ChordPhase::Idle;
                        Some(ChordEvent::Released)
                    } else if pressed && self.mods.extra() {
                        self.phase = ChordPhase::Contaminated;
                        Some(ChordEvent::Broken)
                    } else {
                        None // key-repeat of an already-held modifier
                    }
                }
                ChordPhase::Contaminated => {
                    if !self.mods.ctrl() || !self.mods.alt() {
                        self.phase = ChordPhase::Idle;
                    }
                    None
                }
            };
        }

        // Non-modifier key while the chord is held: a regular shortcut such as
        // Ctrl+Alt+A — not speech input.
        if pressed && self.phase == ChordPhase::Active {
            self.phase = ChordPhase::Contaminated;
            return Some(ChordEvent::Broken);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u32 = 0xA2;
    const LALT: u32 = 0xA4;
    const LSHIFT: u32 = 0xA0;
    const KEY_A: u32 = 0x41;

    #[test]
    fn chord_press_and_release() {
        let mut s = ChordState::default();
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
        assert_eq!(s.handle(WM_SYSKEYUP, LALT), Some(ChordEvent::Released));
        assert_eq!(s.handle(WM_KEYUP, LCTRL), None);
    }

    #[test]
    fn modifier_key_repeat_does_not_refire() {
        let mut s = ChordState::default();
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYUP, LCTRL), Some(ChordEvent::Released));
    }

    #[test]
    fn third_key_breaks_chord_until_full_release() {
        let mut s = ChordState::default();
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
        assert_eq!(s.handle(WM_SYSKEYDOWN, KEY_A), Some(ChordEvent::Broken));
        assert_eq!(s.handle(WM_SYSKEYDOWN, KEY_A), None); // key repeat
        assert_eq!(s.handle(WM_SYSKEYUP, KEY_A), None);
        assert_eq!(s.handle(WM_SYSKEYUP, LALT), None); // broken → no Released
        assert_eq!(s.handle(WM_KEYUP, LCTRL), None);
        // Fully released → the chord re-arms.
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
    }

    #[test]
    fn extra_modifier_breaks_active_chord() {
        let mut s = ChordState::default();
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
        assert_eq!(s.handle(WM_SYSKEYDOWN, LSHIFT), Some(ChordEvent::Broken));
        assert_eq!(s.handle(WM_SYSKEYUP, LSHIFT), None); // still contaminated
        assert_eq!(s.handle(WM_SYSKEYUP, LALT), None);
    }

    #[test]
    fn no_activation_while_extra_modifier_held() {
        let mut s = ChordState::default();
        assert_eq!(s.handle(WM_KEYDOWN, LSHIFT), None);
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), None); // Ctrl+Shift+Alt ≠ chord
        assert_eq!(s.handle(WM_SYSKEYUP, LSHIFT), None);
        // Re-pressing Alt with only Ctrl held activates the chord.
        assert_eq!(s.handle(WM_SYSKEYUP, LALT), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
    }

    #[test]
    fn rearms_after_release_with_ctrl_still_held() {
        let mut s = ChordState::default();
        assert_eq!(s.handle(WM_KEYDOWN, LCTRL), None);
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
        assert_eq!(s.handle(WM_SYSKEYUP, LALT), Some(ChordEvent::Released));
        assert_eq!(s.handle(WM_SYSKEYDOWN, LALT), Some(ChordEvent::Pressed));
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
        // Physical keystrokes only: our own paste injection (SendInput Ctrl+V)
        // and other synthetic input must never drive the chord.
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

fn run_chord_hook(app: AppHandle, ready: std::sync::mpsc::SyncSender<Result<(), String>>) {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, PM_NOREMOVE,
        WH_KEYBOARD_LL,
    };

    // Create the thread message queue before publishing the thread id.
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };

    // Session work (audio device open, HUD window calls) must not run on this
    // thread: a hook thread that stalls gets its hook silently removed by
    // Windows. A dedicated dispatcher keeps events ordered and the loop fast.
    let (tx, rx) = std::sync::mpsc::channel::<ChordEvent>();
    let dispatch_app = app.clone();
    let dispatcher = std::thread::Builder::new()
        .name("blurt-hotkey-dispatch".to_string())
        .spawn(move || {
            for event in rx {
                match event {
                    ChordEvent::Pressed => crate::app::hotkey_pressed(&dispatch_app),
                    ChordEvent::Released => crate::app::hotkey_released(&dispatch_app, None),
                    ChordEvent::Broken => crate::app::chord_broken(&dispatch_app),
                }
            }
        });
    if let Err(error) = dispatcher {
        let _ = ready.send(Err(format!("启动快捷键分发线程失败：{error}")));
        return;
    }

    CHORD_HOOK_THREAD_ID.store(thread_id, Ordering::SeqCst);
    let hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(chord_keyboard_hook), None, 0) } {
            Ok(hook) => hook,
            Err(error) => {
                CHORD_HOOK_THREAD_ID.store(0, Ordering::SeqCst);
                let _ = ready.send(Err(format!("安装 Ctrl+Alt 键盘监听失败：{error}")));
                return;
            }
        };
    let _ = ready.send(Ok(()));
    tracing::info!("Ctrl+Alt 低级键盘钩子已启动（线程 {thread_id}）");

    let mut chord = ChordState::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            tracing::error!("Ctrl+Alt 键盘监听消息循环异常退出");
            break;
        }
        if result.0 == 0 {
            break;
        }
        if message.message != WM_CHORD_KEY {
            continue;
        }
        if let Some(event) = chord.handle(message.lParam.0 as u32, message.wParam.0 as u32) {
            let _ = tx.send(event);
        }
    }

    CHORD_HOOK_THREAD_ID.store(0, Ordering::SeqCst);
    if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
        tracing::error!("卸载 Ctrl+Alt 键盘钩子失败：{error}");
    }
    tracing::warn!("Ctrl+Alt 键盘钩子已停止（正常情况下应常驻）");
}

/// 安装常驻的 Ctrl+Alt 和弦钩子；等待钩子真正挂上后才返回。
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
pub fn spawn_release_watcher(app: &AppHandle, gen: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        const VK_CONTROL: i32 = 0x11;
        const VK_MENU: i32 = 0x12;
        std::thread::sleep(std::time::Duration::from_millis(30));
        loop {
            {
                let state = app.state::<crate::app::AppState>();
                if state.gen.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return;
                }
            }
            let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL) } as u16 & 0x8000 != 0;
            let alt = unsafe { GetAsyncKeyState(VK_MENU) } as u16 & 0x8000 != 0;
            if !ctrl || !alt {
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
