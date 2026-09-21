//! Mock 实现：无模型也能验证链路时序。

use crate::{AsrError, AsrResult, SpeechSpan, StreamingAsr, Vad};

/// Mock ASR 引擎：不接音频模型，`finish` 时返回固定文本。
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

    fn reset(&mut self) {
        self.pending = None;
    }
}

/// 脚本化 Mock：每次 `accept_samples` 解锁一个预设结果（模拟识别随时间推进），
/// `finish` 时把最近一次文本重发为定稿。用于状态机测试。
pub struct ScriptedAsr {
    script: std::collections::VecDeque<AsrResult>,
    last: String,
    /// 每次 accept 解锁一个结果，take 消费后不再重复产出。
    unlocked: usize,
}

impl ScriptedAsr {
    pub fn new(results: Vec<AsrResult>) -> Self {
        Self {
            script: results.into(),
            last: String::new(),
            unlocked: 0,
        }
    }

    pub fn interim(text: &str) -> AsrResult {
        AsrResult {
            text: text.into(),
            is_final: false,
        }
    }
}

impl StreamingAsr for ScriptedAsr {
    fn accept_samples(&mut self, _pcm: &[f32]) -> Result<(), AsrError> {
        // 每次 accept 最多解锁一个新结果。
        if self.unlocked == 0 {
            self.unlocked = 1;
        }
        Ok(())
    }

    fn take_result(&mut self) -> Result<Option<AsrResult>, AsrError> {
        if self.unlocked == 0 {
            return Ok(None);
        }
        if let Some(r) = self.script.pop_front() {
            self.last = r.text.clone();
            self.unlocked = 0;
            return Ok(Some(r));
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<(), AsrError> {
        self.script.push_back(AsrResult {
            text: self.last.clone(),
            is_final: true,
        });
        self.unlocked = 1;
        Ok(())
    }

    fn reset(&mut self) {
        self.unlocked = 0;
    }
}
/// Mock VAD：累计 `trigger_after_samples` 个采样后即视为一句话结束，
/// 把此前全部音频作为一个语音段返回。用于在没有 VAD 模型时验证状态机。
pub struct MockVad {
    buffer: Vec<f32>,
    trigger_after_samples: usize,
    pending: Option<SpeechSpan>,
}

impl MockVad {
    pub fn new(trigger_after_samples: usize) -> Self {
        Self {
            buffer: Vec::new(),
            trigger_after_samples,
            pending: None,
        }
    }
}

impl Vad for MockVad {
    fn accept_samples(&mut self, pcm: &[f32]) -> Result<(), AsrError> {
        let base = self.buffer.len();
        self.buffer.extend_from_slice(pcm);
        if self.pending.is_none() && self.buffer.len() >= self.trigger_after_samples {
            self.pending = Some(SpeechSpan {
                start_sample: base.saturating_sub(self.buffer.len()),
                samples: std::mem::take(&mut self.buffer),
            });
        }
        Ok(())
    }

    fn take_segment(&mut self) -> Result<Option<SpeechSpan>, AsrError> {
        Ok(self.pending.take())
    }

    fn total_samples(&self) -> usize {
        self.buffer.len()
    }

    fn seen_speech(&self) -> bool {
        !self.buffer.is_empty() || self.pending.is_some()
    }
}
