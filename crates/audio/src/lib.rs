//! 麦克风音频采集层。
//!
//! ASR 引擎（sherpa-onnx）要求 16kHz / 单声道 / f32 PCM，而设备默认配置
//! 通常是 44.1/48kHz 多声道，因此本层统一输出 16kHz 单声道 f32：
//! - [`AudioCapture`]：整段录音，`stop` 时一次性转换（适合 CLI 定长录音）。
//! - [`stream::AudioStream`]：流式采集，边录边产出重采样块（适合实时识别）。

pub mod stream;
pub use stream::{AudioStream, StreamingResampler};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::sync::{Arc, Mutex};

/// ASR 引擎要求的目标采样率。
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("找不到默认输入设备（麦克风）")]
    NoInputDevice,
    #[error("获取输入设备配置失败: {0}")]
    Config(String),
    #[error("创建音频流失败: {0}")]
    Stream(String),
    #[error("音频流运行错误: {0}")]
    Runtime(String),
}

/// 按设备的真实采样格式建流，并把回调统一转换为 f32。
fn build_typed_input_stream<T, D, E>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut on_data: D,
    on_error: E,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
    D: FnMut(&[f32]) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let converted: Vec<f32> = data
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .collect();
                on_data(&converted);
            },
            on_error,
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))
}

pub(crate) fn build_input_f32_stream<D, E>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    on_data: D,
    on_error: E,
) -> Result<cpal::Stream, AudioError>
where
    D: FnMut(&[f32]) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let stream_config = config.config();
    match config.sample_format() {
        SampleFormat::I8 => {
            build_typed_input_stream::<i8, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::I16 => {
            build_typed_input_stream::<i16, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::I32 => {
            build_typed_input_stream::<i32, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::I64 => {
            build_typed_input_stream::<i64, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::U8 => {
            build_typed_input_stream::<u8, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::U16 => {
            build_typed_input_stream::<u16, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::U32 => {
            build_typed_input_stream::<u32, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::U64 => {
            build_typed_input_stream::<u64, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::F32 => {
            build_typed_input_stream::<f32, _, _>(device, &stream_config, on_data, on_error)
        }
        SampleFormat::F64 => {
            build_typed_input_stream::<f64, _, _>(device, &stream_config, on_data, on_error)
        }
        other => Err(AudioError::Config(format!(
            "不支持的麦克风采样格式: {other}"
        ))),
    }
}

/// 一次录音会话。`start` 开始采集，`stop` 返回 16kHz 单声道 PCM。
pub struct AudioCapture {
    stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    device_sample_rate: u32,
    channels: u16,
    runtime_error: Arc<Mutex<Option<String>>>,
}

impl AudioCapture {
    /// 打开默认输入设备并开始录音。
    pub fn start() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?;
        let config = device
            .default_input_config()
            .map_err(|e| AudioError::Config(e.to_string()))?;

        let device_sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let runtime_error = Arc::new(Mutex::new(None));

        let buf = Arc::clone(&buffer);
        let err = Arc::clone(&runtime_error);
        let stream = build_input_f32_stream(
            &device,
            &config,
            move |data: &[f32]| {
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(data);
                }
            },
            move |e| {
                if let Ok(mut slot) = err.lock() {
                    *slot = Some(e.to_string());
                }
            },
        )?;
        stream
            .play()
            .map_err(|e| AudioError::Stream(e.to_string()))?;

        Ok(Self {
            stream,
            buffer,
            device_sample_rate,
            channels,
            runtime_error,
        })
    }

    /// 停止录音，返回重采样后的 16kHz 单声道 f32 PCM。
    pub fn stop(self) -> Result<Vec<f32>, AudioError> {
        drop(self.stream); // 先停流，确保缓冲不再被写入
        if let Some(e) = self.runtime_error.lock().ok().and_then(|mut s| s.take()) {
            return Err(AudioError::Runtime(e));
        }
        let raw = std::mem::take(&mut *self.buffer.lock().unwrap());
        let mono = mix_to_mono(&raw, self.channels);
        Ok(resample_linear(
            &mono,
            self.device_sample_rate,
            TARGET_SAMPLE_RATE,
        ))
    }
}

/// 多声道混合为单声道（各声道取平均）。
fn mix_to_mono(frames: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    frames
        .chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// 线性插值重采样。质量对语音识别足够，避免引入重型重采样库。
fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (input.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx];
            let b = input.get(idx + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

/// 保存为 WAV（16kHz 单声道 f32），用于人工试听验证采集质量。
pub fn save_wav(path: &std::path::Path, samples: &[f32]) -> Result<(), AudioError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| AudioError::Runtime(e.to_string()))?;
    for s in samples {
        writer
            .write_sample(*s)
            .map_err(|e| AudioError::Runtime(e.to_string()))?;
    }
    writer
        .finalize()
        .map_err(|e| AudioError::Runtime(e.to_string()))
}

/// 从 WAV 文件读取音频，统一转换为 16kHz 单声道 f32。
/// 用于在没有麦克风的环境（如服务器/CI）中用测试音频驱动 ASR 链路。
pub fn load_wav(path: &std::path::Path) -> Result<Vec<f32>, AudioError> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| AudioError::Runtime(e.to_string()))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample.min(31) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / max)
                .collect()
        }
    };
    let mono = mix_to_mono(&samples, spec.channels);
    Ok(resample_linear(&mono, spec.sample_rate, TARGET_SAMPLE_RATE))
}

/// 音频统计信息，用于快速判断采集是否正常（无声/削波）。
pub fn analyze(samples: &[f32]) -> AudioStats {
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &s in samples {
        peak = peak.max(s.abs());
        sum_sq += (s as f64) * (s as f64);
    }
    AudioStats {
        duration_secs: samples.len() as f32 / TARGET_SAMPLE_RATE as f32,
        rms: if samples.is_empty() {
            0.0
        } else {
            (sum_sq / samples.len() as f64).sqrt() as f32
        },
        peak,
    }
}

#[derive(Debug)]
pub struct AudioStats {
    pub duration_secs: f32,
    pub rms: f32,
    pub peak: f32,
}
