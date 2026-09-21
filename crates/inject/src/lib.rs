//! 文本注入层：把识别结果送进当前焦点窗口。
//!
//! 三种策略并行实现，运行时可切换：
//! - [`ClipboardInjector`]：剪贴板粘贴（默认）。原子操作，不触发目标应用的
//!   按键副作用（编辑器对 @、#、/ 的联想面板等），长文本一次到位。
//! - [`TypingInjector`]：Unicode 逐字注入。有"正在输入"的实时感，
//!   但每个字符都会触发目标应用的按键处理，适合粘贴失效时兜底。
//! - [`HybridInjector`]：混合流式。中间识别结果逐字上屏，定稿时撤销
//!   中间结果并一次性粘贴，兼顾实时感与最终插入的干净。

#[cfg(windows)]
mod keyboard;

#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod hybrid;
#[cfg(windows)]
mod typing;

#[cfg(windows)]
pub use clipboard::ClipboardInjector;
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
