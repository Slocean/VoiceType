//! Win32 原始输入注入：SendInput 封装。
//!
//! 中文等非 ASCII 字符无法通过虚拟键码表达，必须使用 KEYEVENTF_UNICODE
//! 直接注入 UTF-16 码元；超出 BMP 的字符（如部分 emoji）通过
//! `encode_utf16` 自动展开为代理对，逐码元发送。

use crate::InjectError;
use std::time::Duration;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_V,
};

fn send_input(events: &mut [INPUT]) -> Result<(), InjectError> {
    let sent = unsafe { SendInput(events, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        return Err(InjectError::Os(format!(
            "SendInput 只发出 {sent}/{} 个事件",
            events.len()
        )));
    }
    Ok(())
}

fn key_event(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 发送一次 Ctrl+V（按下 Ctrl → 按下 V → 抬起 V → 抬起 Ctrl）。
pub fn send_ctrl_v() -> Result<(), InjectError> {
    let mut events = [
        key_event(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0)),
        key_event(VK_V, 0, KEYBD_EVENT_FLAGS(0)),
        key_event(VK_V, 0, KEYEVENTF_KEYUP),
        key_event(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ];
    send_input(&mut events)
}

/// 发送一次退格。
pub fn send_backspace() -> Result<(), InjectError> {
    let mut events = [
        key_event(VK_BACK, 0, KEYBD_EVENT_FLAGS(0)),
        key_event(VK_BACK, 0, KEYEVENTF_KEYUP),
    ];
    send_input(&mut events)
}

/// 逐字注入 Unicode 文本。`char_delay` 防止目标应用事件处理不及而丢字。
pub fn send_unicode_text(text: &str, char_delay: Duration) -> Result<(), InjectError> {
    for unit in text.encode_utf16() {
        let mut events = [
            key_event(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE),
            key_event(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
        ];
        send_input(&mut events)?;
        if !char_delay.is_zero() {
            std::thread::sleep(char_delay);
        }
    }
    Ok(())
}
