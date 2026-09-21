//! sherpa-onnx 本地流式识别引擎（zipformer transducer 等在线模型）。
//!
//! 模型文件路径由外部显式传入，不在引擎内猜测包内文件名——不同模型包的
//! encoder/decoder/joiner 命名规则不同（epoch/avg/chunk 后缀各异）。

use crate::{AsrError, AsrResult, SpeechSpan, StreamingAsr, Vad};
use std::path::PathBuf;

/// sherpa-onnx 要求的输入格式（与 crate 内 feature_config 保持一致）。
const SAMPLE_RATE: i32 = 16_000;
const FEATURE_DIM: i32 = 80;

pub struct SherpaAsrConfig {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    /// 推理线程数，默认 2（常驻工具不应吃满 CPU）。
    pub num_threads: i32,
}

pub struct SherpaAsr {
    recognizer: sherpa_onnx::OnlineRecognizer,
    stream: sherpa_onnx::OnlineStream,
    /// 最近一次产出的文本，用于去重（sherpa 每次 decode 都会重算全量假设）。
    last_text: String,
    pending: Option<AsrResult>,
    finished: bool,
}

impl SherpaAsr {
    pub fn new(config: &SherpaAsrConfig) -> Result<Self, AsrError> {
        for (name, path) in [
            ("encoder", &config.encoder),
            ("decoder", &config.decoder),
            ("joiner", &config.joiner),
            ("tokens", &config.tokens),
        ] {
            if !path.exists() {
                return Err(AsrError::Init(format!("{name} 模型文件不存在: {}", path.display())));
            }
        }
        let to_str = |p: &PathBuf| p.to_string_lossy().into_owned();
        let cfg = sherpa_onnx::OnlineRecognizerConfig {
            feat_config: sherpa_onnx_sys::FeatureConfig {
                sample_rate: SAMPLE_RATE,
                feature_dim: FEATURE_DIM,
            },
            model_config: sherpa_onnx::OnlineModelConfig {
                transducer: sherpa_onnx::OnlineTransducerModelConfig {
                    encoder: Some(to_str(&config.encoder)),
                    decoder: Some(to_str(&config.decoder)),
                    joiner: Some(to_str(&config.joiner)),
                },
                tokens: Some(to_str(&config.tokens)),
                num_threads: config.num_threads,
                provider: Some("cpu".into()),
                ..Default::default()
            },
            decoding_method: Some("greedy_search".into()),
            enable_endpoint: true,
            ..Default::default()
        };
        let recognizer = sherpa_onnx::OnlineRecognizer::create(&cfg)
            .ok_or_else(|| AsrError::Init("OnlineRecognizer::create 返回空（检查模型文件是否匹配）".into()))?;
        let stream = recognizer.create_stream();
        Ok(Self {
            recognizer,
            stream,
            last_text: String::new(),
            pending: None,
            finished: false,
        })
    }

    /// 驱动解码并收集最新识别假设。
    fn pump(&mut self) {
        while self.recognizer.is_ready(&self.stream) {
            self.recognizer.decode(&self.stream);
        }
        if let Some(result) = self.recognizer.get_result(&self.stream) {
            if !result.text.is_empty() && result.text != self.last_text {
                self.pending = Some(AsrResult {
                    text: result.text.clone(),
                    is_final: self.finished || self.recognizer.is_endpoint(&self.stream),
                });
                self.last_text = result.text;
            }
        }
    }
}

impl StreamingAsr for SherpaAsr {
    fn accept_samples(&mut self, pcm_16k_mono: &[f32]) -> Result<(), AsrError> {
        self.stream.accept_waveform(SAMPLE_RATE, pcm_16k_mono);
        self.pump();
        // 端点检测命中后重置流状态，开始下一句话。
        if self.recognizer.is_endpoint(&self.stream) {
            self.recognizer.reset(&self.stream);
        }
        Ok(())
    }

    fn take_result(&mut self) -> Result<Option<AsrResult>, AsrError> {
        Ok(self.pending.take())
    }

    fn finish(&mut self) -> Result<(), AsrError> {
        self.stream.input_finished();
        self.finished = true;
        self.pump();
        // finish 后即使没有文本变化，也要保证上层能拿到定稿信号。
        if self.pending.is_none() {
            self.pending = Some(AsrResult {
                text: self.last_text.clone(),
                is_final: true,
            });
        } else if let Some(p) = &mut self.pending {
            p.is_final = true;
        }
        Ok(())
    }
}

/// sherpa-onnx silero-vad 配置。`silence_seconds` 决定"停顿多久算说完"，
/// 即断句灵敏度；`speech_threshold` 为能量/概率双阈值中的能量部分。
pub struct SherpaVadConfig {
    pub model: PathBuf,
    pub sample_rate: i32,
    /// 连续静音超过该秒数，且此前出现过语音时，产出尾段并结束会话。
    pub silence_seconds: f32,
    /// 能量门限（RMS）：低于此值直接判静音，避免推理浪费与误触发。
    pub energy_threshold: f32,
    /// silero 语音概率门限。
    pub speech_threshold: f32,
    pub num_threads: i32,
}

impl Default for SherpaVadConfig {
    fn default() -> Self {
        Self {
            model: PathBuf::from("models/silero_vad.onnx"),
            sample_rate: 16_000,
            silence_seconds: 0.8,
            energy_threshold: 0.01,
            speech_threshold: 0.5,
            num_threads: 1,
        }
    }
}

/// silero-vad 语音段切分器。内部按 512 采样窗推进，语音段结束判定：
/// 连续 `silence_seconds` 秒无语音（且其间出现过语音）→ 产出尾段。
pub struct SherpaVad {
    vad: sherpa_onnx::VoiceActivityDetector,
    config: ResolvedVadConfig,
    /// 会话累计的全部音频（16kHz 单声道），用于按起止下标切片语音段。
    history: Vec<f32>,
    /// 已取走的语音段总数（用于状态简化：一次会话只产出一个尾段）。
    emitted: bool,
    seen_speech: bool,
}

struct ResolvedVadConfig {
    silence_samples: usize,
    energy_threshold: f32,
}

impl SherpaVad {
    pub fn new(config: &SherpaVadConfig) -> Result<Self, AsrError> {
        if !config.model.exists() {
            return Err(AsrError::Init(format!(
                "VAD 模型文件不存在: {}",
                config.model.display()
            )));
        }
        let vad_config = sherpa_onnx::VadModelConfig {
            silero_vad: sherpa_onnx::SileroVadModelConfig {
                model: Some(config.model.to_string_lossy().into_owned()),
                threshold: config.speech_threshold,
                min_silence_duration: config.silence_seconds,
                // 短于 0.25 秒的语音毛刺视为噪声，不触发语音态。
                min_speech_duration: 0.25,
                // silero window_size 常用 512（16kHz 下 32ms）。
                window_size: 512,
                max_speech_duration: f32::MAX,
            },
            ten_vad: Default::default(),
            sample_rate: config.sample_rate,
            num_threads: config.num_threads,
            provider: Some("cpu".into()),
            debug: false,
        };
        // buffer 保留 30 秒音频，避免长句被 VAD 内部截断。
        let vad = sherpa_onnx::VoiceActivityDetector::create(&vad_config, 30.0)
            .ok_or_else(|| AsrError::Init("VoiceActivityDetector::create 返回空".into()))?;
        Ok(Self {
            vad,
            config: ResolvedVadConfig {
                silence_samples: (config.silence_seconds * config.sample_rate as f32) as usize,
                energy_threshold: config.energy_threshold,
            },
            history: Vec::new(),
            emitted: false,
            seen_speech: false,
        })
    }

    /// 判断最近 `silence_samples` 个采样是否全部低于能量门限。
    fn tail_is_silent(&self) -> bool {
        let n = self.config.silence_samples.min(self.history.len());
        if n == 0 {
            return true;
        }
        let start = self.history.len() - n;
        self.history[start..]
            .iter()
            .all(|&s| s.abs() < self.config.energy_threshold)
    }
}

impl Vad for SherpaVad {
    fn accept_samples(&mut self, pcm: &[f32]) -> Result<(), AsrError> {
        // 能量预筛：静音块直接累积历史，不经过模型推理。
        // 这样静默环境下的 CPU 占用几乎为零，且推理只发生在疑似语音上。
        if pcm.iter().all(|&s| s.abs() < self.config.energy_threshold) {
            self.history.extend_from_slice(pcm);
            return Ok(());
        }
        self.seen_speech = true;
        self.history.extend_from_slice(pcm);
        self.vad.accept_waveform(pcm);
        Ok(())
    }

    fn take_segment(&mut self) -> Result<Option<SpeechSpan>, AsrError> {
        if self.emitted {
            return Ok(None);
        }
        // 优先消费 silero 产出的完整语音段（长句中已确认结束的段）。
        if !self.vad.is_empty() {
            if let Some(seg) = self.vad.front() {
                let start = seg.start() as usize;
                let samples = seg.samples().to_vec();
                self.vad.pop();
                // 只产出第一段就结束本次会话（单句模式），后续音频属于下一句。
                self.emitted = true;
                return Ok(Some(SpeechSpan {
                    start_sample: start,
                    samples,
                }));
            }
        }
        // 尾段判定：出现过语音 + 尾部持续静音 → 冲刷剩余音频作为尾段。
        if self.seen_speech && self.tail_is_silent() {
            self.emitted = true;
            // 取尾部：从最后一个语音活动点往前留一小段前导，
            // 后缀静音块不送识别（节省时间）。
            let end = self.history.len().saturating_sub(self.config.silence_samples);
            let start = end.saturating_sub(self.config.silence_samples / 2);
            return Ok(Some(SpeechSpan {
                start_sample: start,
                samples: self.history[start..end].to_vec(),
            }));
        }
        Ok(None)
    }

    fn total_samples(&self) -> usize {
        self.history.len()
    }

    fn seen_speech(&self) -> bool {
        self.seen_speech
    }
}
