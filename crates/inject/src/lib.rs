//! 文本注入层：把识别结果送进当前焦点窗口。
//!
//! 三种策略并行实现，运行时可切换：
//! - [`ClipboardInjector`]：纯文本剪贴板使用一次性粘贴；图片、富文本等
//!   无法完整备份的内容保留原剪贴板，改用 Unicode 键入。
//! - [`TypingInjector`]：Unicode 逐字注入。有"正在输入"的实时感，
//!   但每个字符都会触发目标应用的按键处理，适合粘贴失效时兜底。
//! - [`HybridInjector`]：托盘版默认模式。中间识别结果逐字上屏，定稿时撤销
//!   中间结果并注入最终文本。

#[cfg(windows)]
mod keyboard;

#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod hybrid;
#[cfg(windows)]
mod typing;

#[cfg(windows)]
pub use clipboard::{copy_to_clipboard, ClipboardInjector};
#[cfg(windows)]
pub use hybrid::HybridInjector;
#[cfg(windows)]
pub use typing::TypingInjector;

/// 一次性注入定稿文本。
pub trait TextInjector {
    fn inject(&self, text: &str) -> Result<(), InjectError>;
}

/// 流式注入：识别过程中持续输出中间结果，结束时提交定稿。
pub trait StreamingInjector {
    /// 上报新的中间识别结果（内容可能反复变化，实现负责保持屏上文本与其一致）。
    fn update_interim(&mut self, text: &str) -> Result<(), InjectError>;
    /// 识别结束，提交最终文本。
    fn commit(&mut self, text: &str) -> Result<(), InjectError>;
    /// 取消本次会话，清理已上屏的中间结果。
    fn cancel(&mut self) -> Result<(), InjectError>;
}

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("剪贴板不可用: {0}")]
    Clipboard(String),
    #[error("系统输入注入失败: {0}")]
    Os(String),
}
