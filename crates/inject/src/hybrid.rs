//! 混合流式注入：中间结果逐字上屏，定稿时撤销中间结果并一次性粘贴。
//!
//! 流程：[`update_interim`](StreamingInjector::update_interim) 把识别中的
//! 中间文本增量逐字键入（实时感）；[`commit`](StreamingInjector::commit)
//! 先按字符数退格撤销全部中间结果，再走剪贴板粘贴定稿（干净、原子、
//! 不触发按键副作用）。

use crate::{keyboard, ClipboardInjector, InjectError, StreamingInjector, TextInjector};
use std::time::Duration;

/// 中间结果上屏时的字符间隔，比纯键入模式更短以保证跟手。
const INTERIM_CHAR_DELAY: Duration = Duration::from_millis(2);

#[derive(Default)]
pub struct HybridInjector {
    clipboard: ClipboardInjector,
    /// 已上屏的中间结果字符数（按 char 计，与退格删除粒度一致）。
    interim_chars: usize,
    /// 上一次上屏的中间结果，用于计算增量或判断是否需要重写。
    last_interim: String,
}

impl HybridInjector {
    /// 退格撤销全部已上屏的中间字符。
    fn erase_interim(&mut self) -> Result<(), InjectError> {
        for _ in 0..self.interim_chars {
            keyboard::send_backspace()?;
            // 退格过快时部分目标应用会丢事件，加微小间隔保证可靠删除。
            std::thread::sleep(Duration::from_millis(2));
        }
        self.interim_chars = 0;
        self.last_interim.clear();
        Ok(())
    }
}

impl StreamingInjector for HybridInjector {
    fn update_interim(&mut self, text: &str) -> Result<(), InjectError> {
        // 新结果不是旧结果的前缀扩展时（识别修正了前面的字），
        // 先整体撤销再重写，保证屏上内容与最新识别结果一致。
        if !text.starts_with(self.last_interim.as_str()) {
            self.erase_interim()?;
        }
        // 此处 last_interim 必为 text 的前缀，按字节切片落在字符边界上，安全。
        let appended = &text[self.last_interim.len()..];
        if !appended.is_empty() {
            keyboard::send_unicode_text(appended, INTERIM_CHAR_DELAY)?;
            self.interim_chars += appended.chars().count();
        }
        self.last_interim = text.to_string();
        Ok(())
    }

    fn commit(&mut self, text: &str) -> Result<(), InjectError> {
        self.erase_interim()?;
        self.clipboard.inject(text)
    }

    fn cancel(&mut self) -> Result<(), InjectError> {
        self.erase_interim()
    }
}
