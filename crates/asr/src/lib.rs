//! ASR 引擎抽象层。
//!
//! 转写引擎通过 [`StreamingAsr`] trait 接入，上层（录音/注入编排）不依赖
//! 具体引擎。实现：
//! - [`MockAsr`]：无模型，返回固定文本，用于链路验证。
//! - [`SherpaAsr`]（feature `sherpa`）：sherpa-onnx 本地流式识别。

#[cfg(feature = "sherpa")]
mod sherpa;
#[cfg(feature = "sherpa")]
pub use sherpa::{SherpaAsr, SherpaAsrConfig};

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("引擎初始化失败: {0}")]
    Init(String),
    #[error("识别失败: {0}")]
    Recognize(String),
}

/// 一次识别结果。`is_final = false` 为中间结果（内容会随后续音频变化），
/// `is_final = true` 为定稿（一段音频的最终文本）。
#[derive(Debug, Clone)]
pub struct AsrResult {
    pub text: String,
    pub is_final: bool,
}

/// 流式语音识别引擎。音频以 16kHz 单声道 f32 PCM 持续喂入，
/// 识别结果通过 [`take_result`](StreamingAsr::take_result) 轮询取出。
///
/// 要求 Send：引擎常被音频回调线程持有（托盘 worker / CLI realtime），
/// 跨线程共享时靠 Mutex 串行访问。
pub trait StreamingAsr: Send {
    /// 喂入一段 PCM 音频。
    fn accept_samples(&mut self, pcm_16k_mono: &[f32]) -> Result<(), AsrError>;
    /// 取出当前可用的识别结果；没有新结果时返回 None。
    fn take_result(&mut self) -> Result<Option<AsrResult>, AsrError>;
    /// 标记音频输入结束，引擎应产出最终定稿。
    fn finish(&mut self) -> Result<(), AsrError>;
}

/// Mock 引擎：不接音频模型，`finish` 时返回固定文本。
/// 用于在没有模型文件的情况下验证全链路时序。
pub struct MockAsr {
    final_text: String,
    pending: Option<AsrResult>,
}

impl MockAsr {
    pub fn new(final_text: impl Into<String>) -> Self {
        Self {
            final_text: final_text.into(),
            pending: None,
        }
    }
}

impl StreamingAsr for MockAsr {
    fn accept_samples(&mut self, _pcm: &[f32]) -> Result<(), AsrError> {
        Ok(())
    }

    fn take_result(&mut self) -> Result<Option<AsrResult>, AsrError> {
        Ok(self.pending.take())
    }

    fn finish(&mut self) -> Result<(), AsrError> {
        self.pending = Some(AsrResult {
            text: self.final_text.clone(),
            is_final: true,
        });
        Ok(())
    }
}
