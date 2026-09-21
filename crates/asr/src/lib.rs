//! ASR 引擎抽象层。
//!
//! 转写引擎通过 [`StreamingAsr`] trait 接入，语音断句通过 [`Vad`] trait 接入，
//! 上层（录音/注入编排）不依赖具体引擎。实现：
//! - [`MockAsr`] / [`MockVad`]：无模型，链路验证用。
//! - [`SherpaAsr`] / [`SherpaVad`]（feature `sherpa`）：
//!   sherpa-onnx 本地流式识别与 silero-vad。

#[cfg(feature = "sherpa")]
mod sherpa;
#[cfg(feature = "sherpa")]
pub use sherpa::{SherpaAsr, SherpaAsrConfig, SherpaVad, SherpaVadConfig};

mod mock;
pub use mock::{MockAsr, MockVad, ScriptedAsr};

mod session;
pub use session::{Utterance, VoiceSession};

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
    /// 清空识别上下文（新语音段从头识别，防止跨段文本拼接污染）。
    fn reset(&mut self);
}

/// 语音起止检测（VAD）输出的语音段。`start_sample` 是该段在
/// 本次会话累计音频中的起始采样下标，`samples` 为该段 PCM。
#[derive(Debug, Clone)]
pub struct SpeechSpan {
    pub start_sample: usize,
    pub samples: Vec<f32>,
}

/// VAD 抽象：把持续音频流切成"一段句话"。实现负责内部缓冲，
/// 上层只管喂音频、取语音段。
///
/// 要求 Send：与 [`StreamingAsr`] 同理，常被音频回调线程持有。
pub trait Vad: Send {
    /// 喂入一段 PCM 音频。
    fn accept_samples(&mut self, pcm_16k_mono: &[f32]) -> Result<(), AsrError>;
    /// 取出当前可用的完整语音段；没有完整段时返回 None。
    /// 当连续静音超过 `silence_timeout_samples` 且其间出现过语音时，
    /// 返回冲刷出的尾段并由上层结束本次会话。
    fn take_segment(&mut self) -> Result<Option<SpeechSpan>, AsrError>;
    /// 本次会话累计的音频采样数（16kHz 单声道坐标）。
    fn total_samples(&self) -> usize;
    /// 最近喂入的音频是否超过静音阈值（用于静默会话不触发任何动作）。
    fn seen_speech(&self) -> bool;
}
