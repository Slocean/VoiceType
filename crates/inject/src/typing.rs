//! Unicode 逐字注入：模拟真实打字。
//!
//! 优点是有"正在输入"的实时感；缺点是每个字符都会触发目标应用的按键
//! 处理（自动补全、@/# 触发面板等），长文本也较慢。适合粘贴失效时兜底，
//! 或作为 Hybrid 策略的中间结果上屏手段。

use crate::{keyboard, InjectError, TextInjector};
use std::time::Duration;

pub struct TypingInjector {
    /// 字符间隔，防止目标应用事件处理不及而丢字。
    pub char_delay: Duration,
}

impl Default for TypingInjector {
    fn default() -> Self {
        Self {
            char_delay: Duration::from_millis(5),
        }
    }
}

impl TextInjector for TypingInjector {
    fn inject(&self, text: &str) -> Result<(), InjectError> {
        keyboard::send_unicode_text(text, self.char_delay)
    }
}
