//! 流式采集：音频回调线程内完成混声道与增量重采样，
//! 以 16kHz 单声道块的形式持续回调给上层（供流式 ASR 实时消费）。

use crate::{build_input_f32_stream, mix_to_mono, AudioError, TARGET_SAMPLE_RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

/// 增量线性重采样器：跨块保持相位连续，避免整块重采样在边界处产生跳变。
pub struct StreamingResampler {
    ratio: f64,
    /// 下一个输出样本对应的输入位置（全局输入坐标）。
    next_pos: f64,
    /// 已消费的输入样本总数。
    consumed: usize,
    /// 上一块最后一个样本，用于跨块边界的插值。
    prev: f32,
    have_prev: bool,
}

impl StreamingResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            ratio: from_rate as f64 / to_rate as f64,
            next_pos: 0.0,
            consumed: 0,
            prev: 0.0,
            have_prev: false,
        }
    }

    /// 喂入一块单声道输入，向 `out` 追加产出的重采样样本。
    pub fn push(&mut self, chunk: &[f32], out: &mut Vec<f32>) {
        if chunk.is_empty() {
            return;
        }
        // data[0] 带上上一块的尾样本，保证边界处插值连续。
        let mut data = Vec::with_capacity(chunk.len() + 1);
        if self.have_prev {
            data.push(self.prev);
        }
        data.extend_from_slice(chunk);
        let base = self.consumed as f64 - if self.have_prev { 1.0 } else { 0.0 };

        while self.next_pos <= base + (data.len() - 1) as f64 {
            let local = self.next_pos - base;
            let idx = local.floor() as usize;
            let frac = (local - idx as f64) as f32;
            let a = data[idx];
            let b = data[(idx + 1).min(data.len() - 1)];
            out.push(a + (b - a) * frac);
            self.next_pos += self.ratio;
        }

        self.prev = *chunk.last().unwrap();
        self.have_prev = true;
        self.consumed += chunk.len();
    }
}

/// 持续录音流。`stop` 停止采集；丢弃而不调用 `stop` 会静默丢失运行错误。
pub struct AudioStream {
    stream: cpal::Stream,
    runtime_error: Arc<Mutex<Option<String>>>,
}

impl AudioStream {
    /// 打开默认输入设备并开始采集；`on_chunk` 在音频线程被调用，
    /// 参数为 16kHz 单声道 f32 块。回调内不要做重活（本层已完成重采样）。
    pub fn start(on_chunk: impl FnMut(&[f32]) + Send + 'static) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?;
        let config = device
            .default_input_config()
            .map_err(|e| AudioError::Config(e.to_string()))?;

        let channels = config.channels();
        let mut resampler = StreamingResampler::new(config.sample_rate().0, TARGET_SAMPLE_RATE);
        let runtime_error = Arc::new(Mutex::new(None));
        let err = Arc::clone(&runtime_error);

        let mut on_chunk = on_chunk;
        let stream = build_input_f32_stream(
            &device,
            &config,
            move |data: &[f32]| {
                let mono = mix_to_mono(data, channels);
                let mut out = Vec::with_capacity(mono.len() / 2 + 2);
                resampler.push(&mono, &mut out);
                if !out.is_empty() {
                    on_chunk(&out);
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
            runtime_error,
        })
    }

    pub fn stop(self) -> Result<(), AudioError> {
        drop(self.stream);
        if let Some(e) = self.runtime_error.lock().ok().and_then(|mut s| s.take()) {
            return Err(AudioError::Runtime(e));
        }
        Ok(())
    }

    /// 录音 worker 周期性读取并清除设备断开等运行错误。
    pub fn take_runtime_error(&self) -> Option<String> {
        self.runtime_error.lock().ok()?.take()
    }
}
