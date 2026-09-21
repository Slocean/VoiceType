//! VoiceType CLI：录音 → 识别 → 注入 全链路入口。
//!
//! 子命令：
//!   record  录音 N 秒，输出统计并保存 WAV（验证音频采集）
//!   run     录音 → ASR（当前为 Mock 引擎）→ 注入当前焦点窗口
//!
//! 焦点说明：run 用固定时长录音而非按键停止——按键停止需要切回终端，
//! 焦点会离开目标应用，注入将落空。倒计时 + 定长录音保证注入时焦点仍在目标上。

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;
use voicetype_asr::{MockAsr, StreamingAsr};
use voicetype_audio::AudioCapture;
use voicetype_inject::{
    ClipboardInjector, HybridInjector, StreamingInjector, TextInjector, TypingInjector,
};

#[derive(Parser)]
#[command(name = "voicetype", about = "系统级语音输入工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出系统输入设备（诊断麦克风是否可用）
    Devices,
    /// 实时流式识别：托盘 worker 同款管线（流式采集 → sherpa 解码），
    /// 打印中间结果与定稿，用于验证实时链路无死锁、无崩溃
    Realtime {
        #[arg(long, default_value = "5")]
        seconds: u64,
        #[arg(long)]
        encoder: Option<PathBuf>,
        #[arg(long)]
        decoder: Option<PathBuf>,
        #[arg(long)]
        joiner: Option<PathBuf>,
        #[arg(long)]
        tokens: Option<PathBuf>,
    },
    /// VAD 断句验证：用 WAV 文件驱动 VoiceSession（MockVAD+脚本ASR 或
    /// SherpaVad+SherpaAsr），打印会话事件序列，用于验证状态机与尾段切分
    VadSession {
        /// 输入 WAV（16kHz 单声道最佳，其他格式自动转换）
        #[arg(long)]
        from_file: PathBuf,
        #[arg(long)]
        encoder: Option<PathBuf>,
        #[arg(long)]
        decoder: Option<PathBuf>,
        #[arg(long)]
        joiner: Option<PathBuf>,
        #[arg(long)]
        tokens: Option<PathBuf>,
        /// VAD 模型（不传则用 MockVad + MockAsr 验证状态机骨架）
        #[arg(long)]
        vad_model: Option<PathBuf>,
    },
    /// 录音 N 秒，输出统计并保存 WAV
    Record {
        #[arg(long, default_value = "3")]
        seconds: u64,
        #[arg(long, default_value = "record.wav")]
        out: PathBuf,
    },
    /// 录音 → 识别 → 注入当前焦点窗口
    Run {
        #[arg(long, value_enum, default_value = "paste")]
        mode: Mode,
        #[arg(long, default_value = "5")]
        seconds: u64,
        /// 识别引擎：mock（默认，链路验证）| sherpa（本地真实识别）
        #[arg(long, default_value = "mock")]
        engine: String,
        /// sherpa 引擎的模型文件（engine=sherpa 时必填）
        #[arg(long)]
        encoder: Option<PathBuf>,
        #[arg(long)]
        decoder: Option<PathBuf>,
        #[arg(long)]
        joiner: Option<PathBuf>,
        #[arg(long)]
        tokens: Option<PathBuf>,
        /// 从 WAV 文件读取音频（跳过麦克风，用于无麦环境验证 ASR）
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Mock 引擎返回的识别文本（真实 ASR 接入前用于链路验证）
        #[arg(long, default_value = "这是一段语音输入的测试文本")]
        mock_text: String,
        /// 只识别不注入（打印识别结果，用于无目标窗口时验证）
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum Mode {
    Paste,
    Type,
    Hybrid,
}

fn countdown(seconds: u64) {
    println!("{seconds} 秒后开始，请点击目标输入框……");
    for i in (1..=seconds).rev() {
        println!("{i}…");
        sleep(Duration::from_secs(1));
    }
}

/// 定长录音：采集设备原生数据，返回 16kHz 单声道 PCM。
fn record_seconds(seconds: u64) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let capture = AudioCapture::start()?;
    println!("录音中（{seconds} 秒）……");
    sleep(Duration::from_secs(seconds));
    let pcm = capture.stop()?;
    let stats = voicetype_audio::analyze(&pcm);
    println!(
        "录音完成：{:.1}s, RMS={:.4}, 峰值={:.4}",
        stats.duration_secs, stats.rms, stats.peak
    );
    Ok(pcm)
}

/// 按名称构造 ASR 引擎。sherpa 引擎需要 feature 启用且四件模型文件齐全。
fn create_engine(
    engine: &str,
    mock_text: &str,
    encoder: Option<PathBuf>,
    decoder: Option<PathBuf>,
    joiner: Option<PathBuf>,
    tokens: Option<PathBuf>,
) -> Result<Box<dyn StreamingAsr>, Box<dyn std::error::Error>> {
    match engine {
        "mock" => Ok(Box::new(MockAsr::new(mock_text))),
        #[cfg(feature = "sherpa")]
        "sherpa" => {
            let required = |name: &str, v: Option<PathBuf>| -> Result<PathBuf, Box<dyn std::error::Error>> {
                v.ok_or_else(|| format!("engine=sherpa 需要 --{name} 指定模型文件").into())
            };
            let config = voicetype_asr::SherpaAsrConfig {
                encoder: required("encoder", encoder)?,
                decoder: required("decoder", decoder)?,
                joiner: required("joiner", joiner)?,
                tokens: required("tokens", tokens)?,
                num_threads: 2,
            };
            Ok(Box::new(voicetype_asr::SherpaAsr::new(&config)?))
        }
        #[cfg(not(feature = "sherpa"))]
        "sherpa" => Err("本构建未启用 sherpa 引擎，请使用 cargo build --features sherpa".into()),
        other => Err(format!("未知引擎: {other}（可选 mock|sherpa）").into()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Devices => {
            use cpal::traits::{DeviceTrait, HostTrait};
            let host = cpal::default_host();
            let default = host
                .default_input_device()
                .and_then(|d| d.name().ok())
                .unwrap_or_else(|| "（无）".to_string());
            println!("默认输入设备: {default}");
            match host.input_devices() {
                Ok(devices) => {
                    for d in devices {
                        let name = d.name().unwrap_or_else(|_| "（未知设备）".to_string());
                        let cfg = d
                            .default_input_config()
                            .map(|c| format!("{}Hz {}ch", c.sample_rate().0, c.channels()))
                            .unwrap_or_else(|_| "（配置不可用）".to_string());
                        println!("  - {name} [{cfg}]");
                    }
                }
                Err(e) => println!("枚举输入设备失败: {e}"),
            }
        }
        Command::Realtime {
            seconds,
            encoder,
            decoder,
            joiner,
            tokens,
        } => {
            // 与托盘 worker 同构的实时管线：流式采集回调内完成
            // accept_samples + take_result，最后 finish 产出定稿。
            let engine = match create_engine("sherpa", "", encoder, decoder, joiner, tokens) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("创建 sherpa 引擎失败: {e}");
                    std::process::exit(2);
                }
            };
            // asr 在音频回调线程与主线程之间共享（托盘 worker 同款结构）。
            let asr = std::sync::Arc::new(std::sync::Mutex::new(engine));
            println!("实时识别中（{seconds} 秒，中途说中文验证中间结果）……");
            let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
            // 回调与打印之间用通道串行，避免并发输出交错。
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            let asr_in_cb = std::sync::Arc::clone(&asr);
            let stream = voicetype_audio::AudioStream::start(move |chunk| {
                if let Ok(mut e) = asr_in_cb.lock() {
                    e.accept_samples(chunk).ok();
                    while let Ok(Some(r)) = e.take_result() {
                        let _ = tx.send(r.text);
                    }
                }
            })
            .expect("打开麦克风失败");
            let printer = std::thread::spawn(move || {
                for text in rx {
                    println!("识别中: {text}");
                }
            });
            while std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(stream);
            // 收尾定稿并打印，与托盘 worker 的停止流程一致。
            if let Ok(mut e) = asr.lock() {
                e.finish().ok();
                while let Ok(Some(r)) = e.take_result() {
                    if r.is_final {
                        println!("定稿: {}", r.text);
                    }
                }
            }
            drop(printer);
            println!("实时管线运行完毕（未崩溃、无死锁）");
        }
        Command::Record { seconds, out } => {
            let pcm = record_seconds(seconds)?;
            voicetype_audio::save_wav(&out, &pcm)?;
            println!("已保存: {}", out.display());
        }
        Command::Run {
            mode,
            seconds,
            engine,
            encoder,
            decoder,
            joiner,
            tokens,
            from_file,
            mock_text,
            dry_run,
        } => {
            let mut asr = create_engine(&engine, &mock_text, encoder, decoder, joiner, tokens)?;

            let pcm = if let Some(wav) = from_file {
                let pcm = voicetype_audio::load_wav(&wav)?;
                let stats = voicetype_audio::analyze(&pcm);
                println!(
                    "已从文件加载音频：{}（{:.1}s）",
                    wav.display(),
                    stats.duration_secs
                );
                pcm
            } else {
                countdown(3);
                record_seconds(seconds)?
            };

            // 分块喂入模拟流式识别（每块 0.25 秒），并打印中间结果变化。
            let chunk = (voicetype_audio::TARGET_SAMPLE_RATE as usize) / 4;
            for part in pcm.chunks(chunk) {
                asr.accept_samples(part)?;
                if let Some(r) = asr.take_result()? {
                    println!("识别中: {} {}", if r.is_final { "[定稿]" } else { "      " }, r.text);
                }
            }
            asr.finish()?;
            let mut final_text = String::new();
            while let Some(r) = asr.take_result()? {
                println!("识别中: {} {}", if r.is_final { "[定稿]" } else { "      " }, r.text);
                if r.is_final {
                    final_text = r.text;
                }
            }
            if final_text.is_empty() {
                println!("识别结果为空，跳过注入");
                return Ok(());
            }
            println!("识别结果: {final_text:?}");

            if dry_run {
                println!("dry-run，跳过注入");
                return Ok(());
            }
            match mode {
                Mode::Paste => ClipboardInjector::default().inject(&final_text)?,
                Mode::Type => TypingInjector::default().inject(&final_text)?,
                Mode::Hybrid => {
                    let mut injector = HybridInjector::default();
                    injector.commit(&final_text)?;
                }
            }
            println!("注入完成");
        }
    }
    Ok(())
}
