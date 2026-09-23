//! 一句话识别会话（VoiceSession）：VAD 断句 + ASR 识别的状态编排。
//!
//! 状态机（单句模式）：
//!
//! ```text
//!              accept_samples(chunk)
//!  Listening ─────────────────────────► Listening
//!     │  VAD.take_segment() -> Some(span)      │ take_segment() -> None
//!     │  产出 Utterance::Final                  │ 产出 Utterance::Interim（如有）
//!     ▼                                        ▼
//!   Closed                                   Listening
//! ```
//!
//! 约束与决策：
//! - ASR 只在 VAD 语音段上运行（VBox 结构），静音不经过识别，
//!   因此 `asr.take_result` 只在段内轮询；
//! - 段识别结束后 `finish` 冲刷定稿，中间的临时假设经 Utterance::Interim
//!   上屏，最终文本经 Utterance::Final 定稿；
//! - 一次会话只产出一句话，结束后状态为 Closed（上层负责重建会话）；
//! - ASR 定稿为空时产出 Utterance::Empty 而非 Final，避免注入空文本。

use crate::{AsrError, SpeechSpan, StreamingAsr, Vad};

/// 会话产出：中间上屏 / 定稿 / 空结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Utterance {
    /// 中间假设（内容可能变化，仅用于上屏预览）。
    Interim(String),
    /// 定稿（可安全注入）。
    Final(String),
    /// 本次会话无语音内容（上层静默收尾，不注入）。
    Empty,
}

/// 一句话识别会话：拥有一个 Vad 与一个 ASR，聚合两者状态。
pub struct VoiceSession<V: Vad, A: StreamingAsr> {
    vad: V,
    asr: A,
    closed: bool,
    /// 段内已上屏的文本，用于 ASR 无新假设时补 Interim。
    last_interim: String,
    /// 会话开启后一直无语音时，累计多少采样自动 Empty 收尾（默认 10 秒）。
    /// 16kHz × 10s。
    empty_after_samples: usize,
}

impl<V: Vad, A: StreamingAsr> VoiceSession<V, A> {
    pub fn new(vad: V, asr: A) -> Self {
        Self {
            vad,
            asr,
            closed: false,
            last_interim: String::new(),
            empty_after_samples: 160_000,
        }
    }

    /// 覆盖"无语音自动收尾"阈值（16kHz 采样数）。
    pub fn set_empty_after_samples(&mut self, samples: usize) {
        self.empty_after_samples = samples;
    }

    /// 是否已结束（产出过 Final/Empty 后为 true）。
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// 喂入一块 16kHz 单声道 PCM，返回本次产生的会话事件
    /// （通常 0~1 个，VAD 段结束时恰好 1 个 Final/Empty）。
    pub fn accept_samples(&mut self, pcm: &[f32]) -> Result<Vec<Utterance>, AsrError> {
        if self.closed {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();

        self.vad.accept_samples(pcm)?;
        self.asr.accept_samples(pcm)?;
        while let Some(r) = self.asr.take_result()? {
            if !r.is_final && r.text != self.last_interim {
                self.last_interim = r.text.clone();
                out.push(Utterance::Interim(r.text));
            }
        }

        if let Some(span) = self.vad.take_segment()? {
            self.closed = true;
            let final_text = self.recognize_span(&span)?;
            if final_text.is_empty() {
                out.push(Utterance::Empty);
            } else {
                out.push(Utterance::Final(final_text));
            }
            return Ok(out);
        }

        // 开启后持续无语音：自动 Empty 收尾，避免静默会话永不结束。
        if !self.vad.seen_speech() && self.vad.total_samples() >= self.empty_after_samples {
            self.closed = true;
            out.push(Utterance::Empty);
        }
        Ok(out)
    }

    /// 用户主动结束录音时，冲刷当前流式结果并产出一次定稿。
    pub fn finish_manual(&mut self) -> Result<Option<Utterance>, AsrError> {
        if self.closed {
            return Ok(None);
        }
        self.closed = true;
        if !self.vad.seen_speech() {
            return Ok(Some(Utterance::Empty));
        }
        self.asr.finish()?;
        let mut text = String::new();
        while let Some(result) = self.asr.take_result()? {
            if result.is_final {
                text = result.text;
            }
        }
        if text.is_empty() {
            text = self.last_interim.clone();
        }
        Ok(Some(if text.is_empty() {
            Utterance::Empty
        } else {
            Utterance::Final(text)
        }))
    }

    /// 对 VAD 切出的语音段做识别：重置 ASR 上下文后从零识别该段，
    /// finish 冲刷定稿。重置是必须的——流式引擎上下文里还留着
    /// 实时 Interim 阶段喂入的音频，不重置会把跨段内容拼进定稿。
    fn recognize_span(&mut self, span: &SpeechSpan) -> Result<String, AsrError> {
        self.asr.reset();
        self.asr.accept_samples(&span.samples)?;
        self.asr.finish()?;
        let mut text = String::new();
        while let Some(r) = self.asr.take_result()? {
            if r.is_final {
                text = r.text;
            } else if r.text != self.last_interim {
                self.last_interim = r.text.clone();
            }
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockVad, ScriptedAsr};

    const CHUNK: [f32; 4] = [0.5, 0.5, 0.5, 0.5];

    /// VAD 在累计 8 个采样后断句；ASR 预设两段中间结果。
    fn session() -> VoiceSession<MockVad, ScriptedAsr> {
        VoiceSession::new(
            MockVad::new(8),
            ScriptedAsr::new(vec![
                ScriptedAsr::interim("你好"),
                ScriptedAsr::interim("你好世界"),
            ]),
        )
    }

    #[test]
    fn interim_flows_before_segment_ends() {
        let mut s = session();
        let e1 = s.accept_samples(&CHUNK).unwrap();
        assert_eq!(e1, vec![Utterance::Interim("你好".into())]);
        assert!(!s.is_closed());
    }

    #[test]
    fn final_emitted_when_vad_segment_ends() {
        let mut s = session();
        s.accept_samples(&CHUNK).unwrap();
        // 第二块累计满 8 采样，VAD 产出语音段，会话结束。
        let e2 = s.accept_samples(&CHUNK).unwrap();
        // 可能包含第二段 Interim + Final。
        assert!(e2.contains(&Utterance::Interim("你好世界".into())));
        let finals: Vec<_> = e2
            .iter()
            .filter_map(|u| match u {
                Utterance::Final(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(finals.len(), 1);
        // finish 冲出最近一次文本作为定稿。
        assert_eq!(finals[0], "你好世界");
        assert!(s.is_closed());
    }

    #[test]
    fn closed_session_ignores_further_audio() {
        let mut s = session();
        s.accept_samples(&CHUNK).unwrap();
        s.accept_samples(&CHUNK).unwrap();
        assert!(s.is_closed());
        assert_eq!(s.accept_samples(&CHUNK).unwrap(), Vec::new());
    }

    #[test]
    fn manual_stop_emits_final_once() {
        let mut s = session();
        s.accept_samples(&CHUNK).unwrap();
        assert_eq!(s.finish_manual().unwrap(), Some(Utterance::Final("你好".into())));
        assert_eq!(s.finish_manual().unwrap(), None);
    }

    #[test]
    fn manual_stop_without_speech_is_empty() {
        let mut s = session();
        assert_eq!(s.finish_manual().unwrap(), Some(Utterance::Empty));
    }
}
