//! sherpa-onnx 本地流式识别引擎（zipformer transducer 等在线模型）。
//!
//! 模型文件路径由外部显式传入，不在引擎内猜测包内文件名——不同模型包的
//! encoder/decoder/joiner 命名规则不同（epoch/avg/chunk 后缀各异）。

use crate::{AsrError, AsrResult, StreamingAsr};
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
