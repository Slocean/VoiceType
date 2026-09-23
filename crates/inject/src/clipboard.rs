//! 剪贴板粘贴注入：纯文本剪贴板备份 → 写入 → Ctrl+V → 条件恢复。
//! 图片、富文本及无法可靠读取的内容改用 Unicode 键入，保留原剪贴板。

use crate::{keyboard, InjectError, TextInjector};
use clipboard_win::{formats, get, seq_num, set, Clipboard, EnumFormats};
use std::num::NonZeroU32;
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
};

/// SetClipboardData 需要非空的剪贴板所有者；临时隐藏窗口由调用线程创建和销毁。
struct ClipboardOwner(HWND);

impl ClipboardOwner {
    fn new() -> Option<Self> {
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("VoiceType Clipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
            .ok()?
        };
        Some(Self(hwnd))
    }

    fn raw(&self) -> clipboard_win::types::HWND {
        let hwnd = self.0;
        hwnd.0.cast()
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

fn is_plain_text_format(format: u32) -> bool {
    matches!(
        format,
        formats::CF_TEXT | formats::CF_OEMTEXT | formats::CF_UNICODETEXT | formats::CF_LOCALE
    )
}

/// 读取和替换在同一次剪贴板锁内完成，避免备份后被别的复制操作抢占。
fn backup_and_replace(text: &str) -> Result<Option<(String, NonZeroU32)>, InjectError> {
    let Some(owner) = ClipboardOwner::new() else {
        return Ok(None);
    };
    let Ok(_clipboard) = Clipboard::new_attempts_for(owner.raw(), 10) else {
        return Ok(None);
    };
    let formats: Vec<u32> = EnumFormats::new().collect();
    if formats.is_empty() || !formats.into_iter().all(is_plain_text_format) {
        return Ok(None);
    }
    let Ok(backup) = get::<String, _>(formats::Unicode) else {
        return Ok(None);
    };
    if seq_num().is_none() {
        return Ok(None);
    }
    if let Err(e) = set(formats::Unicode, text) {
        let _ = set(formats::Unicode, &backup);
        return Err(InjectError::Clipboard(e.to_string()));
    }
    let Some(sequence) = seq_num() else {
        let _ = set(formats::Unicode, &backup);
        return Err(InjectError::Clipboard("无法追踪剪贴板变化".into()));
    };
    Ok(Some((backup, sequence)))
}

fn restore_if_unchanged(sequence: NonZeroU32, original: &str) {
    // 复制与序号检查同在剪贴板锁内，用户新复制的内容不会被覆盖。
    for _ in 0..5 {
        let Some(owner) = ClipboardOwner::new() else {
            return;
        };
        if let Ok(_clipboard) = Clipboard::new_attempts_for(owner.raw(), 10) {
            if seq_num() != Some(sequence) {
                return;
            }
            if set(formats::Unicode, original).is_ok() {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub struct ClipboardInjector {
    /// 粘贴后等待目标应用消费剪贴板再恢复，避免恢复过早导致粘贴出旧内容。
    pub restore_delay: Duration,
}

impl Default for ClipboardInjector {
    fn default() -> Self {
        Self {
            restore_delay: Duration::from_millis(300),
        }
    }
}

impl TextInjector for ClipboardInjector {
    fn inject(&self, text: &str) -> Result<(), InjectError> {
        let Some((backup, sequence)) = backup_and_replace(text)? else {
            return keyboard::send_unicode_text(text, Duration::from_millis(2));
        };
        if seq_num() != Some(sequence) {
            // 写入后用户已经复制了新内容；直接键入定稿，避免粘贴错文本。
            return keyboard::send_unicode_text(text, Duration::from_millis(2));
        }
        if let Err(e) = keyboard::send_ctrl_v() {
            restore_if_unchanged(sequence, &backup);
            return Err(e);
        }

        // 目标应用在自己的消息循环中消费粘贴；异步等待后再恢复。
        let delay = self.restore_delay;
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            restore_if_unchanged(sequence, &backup);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_text_formats_are_not_treated_as_safe_to_replace() {
        assert!(is_plain_text_format(formats::CF_UNICODETEXT));
        assert!(!is_plain_text_format(formats::CF_BITMAP));
    }
}
