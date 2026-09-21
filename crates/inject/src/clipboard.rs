//! 剪贴板粘贴注入：备份 → 写入 → Ctrl+V → 延迟恢复。
//!
//! 粘贴是目标应用内的一次原子 paste 事件，不经过逐键处理，因此不会触发
//! 编辑器对 @、#、/ 等字符的按键副作用，且长文本一次到位，作为默认策略。

use crate::{keyboard, InjectError, TextInjector};
use std::time::Duration;

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
        let mut cb =
            arboard::Clipboard::new().map_err(|e| InjectError::Clipboard(e.to_string()))?;
        // 备份用户剪贴板，注入后必须恢复，不能静默破坏用户数据。
        let backup = cb.get_text().ok();
        cb.set_text(text)
            .map_err(|e| InjectError::Clipboard(e.to_string()))?;
        keyboard::send_ctrl_v()?;

        // 恢复必须异步进行：粘贴动作由目标应用在自己的消息循环里消费，
        // 同步睡眠既阻塞调用方，也无法保证目标已完成粘贴——当目标与调用方
        // 处于同一线程时，同步恢复甚至会先于粘贴执行，导致粘贴出旧内容。
        let delay = self.restore_delay;
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if let Ok(mut cb) = arboard::Clipboard::new() {
                match backup {
                    Some(prev) => {
                        let _ = cb.set_text(prev);
                    }
                    None => {
                        let _ = cb.clear();
                    }
                }
            }
        });
        Ok(())
    }
}
