//! 全局快捷键：解析、注册（强制替换语义）、捕获期挂起、按键松开与 Esc 的轮询看门。

use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

/// "ctrl+alt+Space" → Shortcut。修饰键小写；主键用 W3C Code 名（Space/KeyA/F8…）
pub fn parse_shortcut(s: &str) -> Result<Shortcut, String> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            "super" | "win" | "meta" | "cmd" => mods |= Modifiers::SUPER,
            _ => {
                code = Some(
                    part.parse::<Code>()
                        .map_err(|_| format!("无法识别的按键：{part}"))?,
                );
            }
        }
    }
    let code = code.ok_or_else(|| "快捷键缺少主键".to_string())?;
    let m = if mods.is_empty() { None } else { Some(mods) };
    Ok(Shortcut::new(m, code))
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
        KeyA => 0x41, KeyB => 0x42, KeyC => 0x43, KeyD => 0x44, KeyE => 0x45,
        KeyF => 0x46, KeyG => 0x47, KeyH => 0x48, KeyI => 0x49, KeyJ => 0x4A,
        KeyK => 0x4B, KeyL => 0x4C, KeyM => 0x4D, KeyN => 0x4E, KeyO => 0x4F,
        KeyP => 0x50, KeyQ => 0x51, KeyR => 0x52, KeyS => 0x53, KeyT => 0x54,
        KeyU => 0x55, KeyV => 0x56, KeyW => 0x57, KeyX => 0x58, KeyY => 0x59,
        KeyZ => 0x5A,
        Digit0 => 0x30, Digit1 => 0x31, Digit2 => 0x32, Digit3 => 0x33, Digit4 => 0x34,
        Digit5 => 0x35, Digit6 => 0x36, Digit7 => 0x37, Digit8 => 0x38, Digit9 => 0x39,
        F1 => 0x70, F2 => 0x71, F3 => 0x72, F4 => 0x73, F5 => 0x74, F6 => 0x75,
        F7 => 0x76, F8 => 0x77, F9 => 0x78, F10 => 0x79, F11 => 0x7A, F12 => 0x7B,
        F13 => 0x7C, F14 => 0x7D, F15 => 0x7E, F16 => 0x7F, F17 => 0x80, F18 => 0x81,
        F19 => 0x82, F20 => 0x83, F21 => 0x84, F22 => 0x85, F23 => 0x86, F24 => 0x87,
        Space => 0x20,
        ArrowLeft => 0x25, ArrowUp => 0x26, ArrowRight => 0x27, ArrowDown => 0x28,
        Home => 0x24, End => 0x23, PageUp => 0x21, PageDown => 0x22,
        Insert => 0x2D, Delete => 0x2E,
        Backquote => 0xC0, Minus => 0xBD, Equal => 0xBB,
        BracketLeft => 0xDB, BracketRight => 0xDD, Backslash => 0xDC,
        Semicolon => 0xBA, Quote => 0xDE, Comma => 0xBC, Period => 0xBE, Slash => 0xBF,
        _ => return None,
    };
    Some(vk)
}

/// Windows 虚拟键码 → W3C Code（原生捕获用）
fn vk_to_code(vk: u32) -> Option<Code> {
    use Code::*;
    let code = match vk {
        0x41..=0x5A => match vk {
            0x41 => KeyA, 0x42 => KeyB, 0x43 => KeyC, 0x44 => KeyD, 0x45 => KeyE,
            0x46 => KeyF, 0x47 => KeyG, 0x48 => KeyH, 0x49 => KeyI, 0x4A => KeyJ,
            0x4B => KeyK, 0x4C => KeyL, 0x4D => KeyM, 0x4E => KeyN, 0x4F => KeyO,
            0x50 => KeyP, 0x51 => KeyQ, 0x52 => KeyR, 0x53 => KeyS, 0x54 => KeyT,
            0x55 => KeyU, 0x56 => KeyV, 0x57 => KeyW, 0x58 => KeyX, 0x59 => KeyY,
            _ => KeyZ,
        },
        0x30 => Digit0, 0x31 => Digit1, 0x32 => Digit2, 0x33 => Digit3, 0x34 => Digit4,
        0x35 => Digit5, 0x36 => Digit6, 0x37 => Digit7, 0x38 => Digit8, 0x39 => Digit9,
        0x70 => F1, 0x71 => F2, 0x72 => F3, 0x73 => F4, 0x74 => F5, 0x75 => F6,
        0x76 => F7, 0x77 => F8, 0x78 => F9, 0x79 => F10, 0x7A => F11, 0x7B => F12,
        0x7C => F13, 0x7D => F14, 0x7E => F15, 0x7F => F16, 0x80 => F17, 0x81 => F18,
        0x82 => F19, 0x83 => F20, 0x84 => F21, 0x85 => F22, 0x86 => F23, 0x87 => F24,
        0x20 => Space,
        0x25 => ArrowLeft, 0x26 => ArrowUp, 0x27 => ArrowRight, 0x28 => ArrowDown,
        0x24 => Home, 0x23 => End, 0x21 => PageUp, 0x22 => PageDown,
        0x2D => Insert, 0x2E => Delete,
        0xC0 => Backquote, 0xBD => Minus, 0xBB => Equal,
        0xDB => BracketLeft, 0xDD => BracketRight, 0xDC => Backslash,
        0xBA => Semicolon, 0xDE => Quote, 0xBC => Comma, 0xBE => Period, 0xBF => Slash,
        _ => return None,
    };
    Some(code)
}

/// 原生快捷键捕获：轮询物理键盘状态（GetAsyncKeyState）。
/// 网页 keydown 会被输入法切换（Ctrl+Space / Ctrl+Shift）、系统菜单（Alt+Space）
/// 等系统级热键截胡；物理层轮询对这些全部免疫，任何组合都能捕到。
/// 结果经事件推给设置页：hotkey:captured / hotkey:capture_invalid / hotkey:capture_cancel
pub fn spawn_capture(app: &AppHandle, gen: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        use tauri::Emitter;
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

        let down = |vk: u32| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
        let stale = |app: &AppHandle| {
            app.state::<crate::app::AppState>()
                .capture_gen
                .load(Ordering::SeqCst)
                != gen
        };

        // 候选主键
        let mut vks: Vec<u32> = vec![0x20];
        vks.extend(0x41..=0x5A); // A-Z
        vks.extend(0x30..=0x39); // 0-9
        vks.extend(0x70..=0x87); // F1-F24
        vks.extend([0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E]);
        vks.extend([0xBA, 0xBB, 0xBC, 0xBD, 0xBE, 0xBF, 0xC0, 0xDB, 0xDC, 0xDD, 0xDE]);

        // 先等所有候选键与 Esc 抬起（吃掉点击按钮瞬间的残留状态）
        loop {
            if stale(&app) {
                return;
            }
            if !down(0x1B) && !vks.iter().any(|&v| down(v)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        'outer: loop {
            if stale(&app) {
                return;
            }
            if down(0x1B) {
                let _ = app.emit_to("settings", "hotkey:capture_cancel", serde_json::json!({}));
                return;
            }
            for &vk in &vks {
                if !down(vk) {
                    continue;
                }
                let ctrl = down(0x11);
                let alt = down(0x12);
                let shift = down(0x10);
                let win = down(0x5B) || down(0x5C);
                let is_f = (0x70..=0x87).contains(&vk);
                if !(ctrl || alt || shift || win) && !is_f {
                    // 裸键（非 F 键）不允许，等抬起后继续等待
                    let _ =
                        app.emit_to("settings", "hotkey:capture_invalid", serde_json::json!({}));
                    while down(vk) {
                        if stale(&app) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(15));
                    }
                    continue 'outer;
                }
                let Some(code) = vk_to_code(vk) else { continue };
                let mut parts: Vec<String> = vec![];
                if ctrl {
                    parts.push("ctrl".into());
                }
                if alt {
                    parts.push("alt".into());
                }
                if shift {
                    parts.push("shift".into());
                }
                if win {
                    parts.push("super".into());
                }
                parts.push(code.to_string());
                let hotkey = parts.join("+");
                tracing::info!("原生捕获到快捷键：{hotkey}");
                let _ = app.emit_to(
                    "settings",
                    "hotkey:captured",
                    serde_json::json!({ "hotkey": hotkey }),
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    });
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
