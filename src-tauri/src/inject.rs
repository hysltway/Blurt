//! 文本注入：等修饰键松开后，模拟键入（KEYEVENTF_UNICODE）或剪贴板粘贴（Ctrl+V + 恢复）。

use std::thread::sleep;
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_NONAME,
    VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// 清洗 ASR 文本：去首尾空白，内部换行改空格（绝不注入回车，避免误触发送）
pub fn sanitize(text: &str) -> String {
    text.trim().replace("\r\n", " ").replace(['\r', '\n'], " ")
}

fn key_down(vk: VIRTUAL_KEY) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: KEYBD_EVENT_FLAGS(0),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_up(vk: VIRTUAL_KEY) -> INPUT {
    let mut i = key_down(vk);
    i.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
    i
}

fn unicode_pair(unit: u16) -> [INPUT; 2] {
    let mk = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    [
        mk(KEYEVENTF_UNICODE),
        mk(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
    ]
}

fn send(inputs: &[INPUT]) -> Result<(), String> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err("SendInput 被系统拒绝（目标窗口可能以管理员权限运行）".into());
    }
    Ok(())
}

/// 等全部修饰键物理松开（用户可能还按着快捷键），超时则继续
pub fn wait_modifiers_released(timeout_ms: u64) {
    let mods = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN];
    let t0 = Instant::now();
    loop {
        let held = mods
            .iter()
            .any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0);
        if !held || t0.elapsed().as_millis() as u64 > timeout_ms {
            return;
        }
        sleep(Duration::from_millis(12));
    }
}

fn type_text(text: &str) -> Result<(), String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut chunks = units.chunks(24).peekable();
    while let Some(chunk) = chunks.next() {
        let mut inputs: Vec<INPUT> = Vec::with_capacity(chunk.len() * 2);
        for &u in chunk {
            inputs.extend_from_slice(&unicode_pair(u));
        }
        send(&inputs)?;
        // 给目标应用留消化时间；最后一批之后无需再等
        if chunks.peek().is_some() {
            sleep(Duration::from_millis(3));
        }
    }
    Ok(())
}

fn paste_text(text: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("无法访问剪贴板：{e}"))?;
    let old = cb.get_text().ok();
    cb.set_text(text.to_string())
        .map_err(|e| format!("写入剪贴板失败：{e}"))?;
    // SetClipboardData 同步生效；30ms 只为躲开剪贴板监听器的抢占窗口
    sleep(Duration::from_millis(30));

    let vk_v = VIRTUAL_KEY(0x56);
    send(&[key_down(VK_CONTROL), key_down(vk_v)])?;
    sleep(Duration::from_millis(25));
    send(&[key_up(vk_v), key_up(VK_CONTROL)])?;

    // 恢复原剪贴板挪到后台：等目标应用读完再还原，不阻塞本次会话收尾
    if let Some(old) = old {
        std::thread::spawn(move || {
            sleep(Duration::from_millis(400));
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(old);
            }
        });
    }
    Ok(())
}

/// 注入入口。mode: auto | type | paste
pub fn inject(text: &str, mode: &str, type_threshold: usize) -> Result<(), String> {
    let text = sanitize(text);
    if text.is_empty() {
        return Ok(());
    }
    wait_modifiers_released(2500);
    sleep(Duration::from_millis(30));

    match mode {
        "type" => type_text(&text),
        "paste" => paste_text(&text),
        _ => {
            if text.chars().count() <= type_threshold {
                type_text(&text)
            } else {
                paste_text(&text)
            }
        }
    }
}

/// 验证当前前台窗口可接收模拟输入；保留虚拟键不会向目标控件写入字符。
pub fn check(mode: &str) -> Result<(), String> {
    if !matches!(mode, "auto" | "type" | "paste") {
        return Err(format!("未知文本写入方式：{mode}"));
    }
    if unsafe { GetForegroundWindow() }.0.is_null() {
        return Err("未找到可写入的活动窗口".into());
    }
    send(&[key_down(VK_NONAME), key_up(VK_NONAME)])?;
    if mode != "type" {
        arboard::Clipboard::new().map_err(|e| format!("无法访问剪贴板：{e}"))?;
    }
    Ok(())
}
