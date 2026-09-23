//! 录音 + 识别 Worker：把音频回调、VAD 会话与事件转发收敛在一个模块。
//!
//! 约束：
//! - 音频回调（cpal 音频线程）内只做：lock 会话 → 喂音频 → 转发事件，
//!   绝不阻塞等待、不在回调内停流（cpal 会死锁）；
//! - worker 线程等待退出条件（VAD 自动断句或手动停止），退出前停流；
//! - 注入只在主线程执行，本模块只发 Ev。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::{Cmd, Ev, NotifyHwnd, WM_WORKER_EVENT};
use voicetype_asr::{SherpaAsr, SherpaVad, SherpaVadConfig, Utterance, VoiceSession};
use voicetype_audio::AudioStream;

pub struct WorkerHandle {
    pub ev_rx: Receiver<Ev>,
    pub join: JoinHandle<()>,
}

/// 启动 worker：创建 VAD+ASR 会话并开始流式采集，返回通道与句柄。
pub fn start_worker(
    notify: NotifyHwnd,
    asr_config: voicetype_asr::SherpaAsrConfig,
    vad_model: PathBuf,
    cmd_rx: Receiver<Cmd>,
) -> Result<WorkerHandle, String> {
    use std::sync::mpsc;

    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    // done 置位即请求 worker 退出；VAD 自动断句时由音频回调置位。
    let done = Arc::new(AtomicBool::new(false));

    let handle = std::thread::spawn({
        let done = Arc::clone(&done);
        move || {
            let session = match SherpaVad::new(&SherpaVadConfig {
                model: vad_model,
                ..Default::default()
            })
            .and_then(|vad| SherpaAsr::new(&asr_config).map(|asr| VoiceSession::new(vad, asr)))
            {
                Ok(s) => Arc::new(Mutex::new(s)),
                Err(e) => {
                    let _ = ev_tx.send(Ev::Error(format!("引擎初始化失败: {e}")));
                    notify.post(WM_WORKER_EVENT);
                    return;
                }
            };

            let session_in_cb = Arc::clone(&session);
            let ev_in_cb = ev_tx.clone();
            let done_in_cb = Arc::clone(&done);
            let stream = AudioStream::start(move |chunk| {
                if let Ok(mut s) = session_in_cb.lock() {
                    // 喂音频并转发会话事件。
                    let batch = match s.accept_samples(chunk) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = ev_in_cb.send(Ev::Error(format!("识别失败: {e}")));
                            notify.post(WM_WORKER_EVENT);
                            done_in_cb.store(true, Ordering::SeqCst);
                            return;
                        }
                    };
                    for u in batch {
                        match u {
                            Utterance::Interim(t) => {
                                let _ = ev_in_cb.send(Ev::Interim(t));
                                notify.post(WM_WORKER_EVENT);
                            }
                            Utterance::Final(t) => {
                                let _ = ev_in_cb.send(Ev::Final(t));
                                notify.post(WM_WORKER_EVENT);
                                // VAD 自动断句：本次会话结束，请求退出。
                                done_in_cb.store(true, Ordering::SeqCst);
                                return;
                            }
                            Utterance::Empty => {
                                let _ = ev_in_cb.send(Ev::Empty);
                                notify.post(WM_WORKER_EVENT);
                                done_in_cb.store(true, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                }
            });
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = ev_tx.send(Ev::Error(format!("打开麦克风失败: {e}")));
                    notify.post(WM_WORKER_EVENT);
                    return;
                }
            };

            // 等待退出条件：VAD 自动断句（done 置位）或手动停止（Cmd::Stop）。
            let mut manual_stop = false;
            loop {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(e) = stream.take_runtime_error() {
                    let _ = ev_tx.send(Ev::Error(format!("麦克风运行失败: {e}")));
                    notify.post(WM_WORKER_EVENT);
                    break;
                }
                match cmd_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Cmd::Stop) => {
                        manual_stop = true;
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            if let Err(e) = stream.stop() {
                let _ = ev_tx.send(Ev::Error(format!("麦克风运行失败: {e}")));
                notify.post(WM_WORKER_EVENT);
                return;
            }
            if manual_stop && !done.load(Ordering::SeqCst) {
                let result = session
                    .lock()
                    .map_err(|_| "识别会话锁已损坏".to_string())
                    .and_then(|mut s| s.finish_manual().map_err(|e| e.to_string()));
                let event = match result {
                    Ok(Some(Utterance::Final(text))) => Some(Ev::Final(text)),
                    Ok(Some(Utterance::Empty)) => Some(Ev::Empty),
                    Ok(Some(Utterance::Interim(_))) | Ok(None) => None,
                    Err(e) => Some(Ev::Error(format!("生成定稿失败: {e}"))),
                };
                if let Some(event) = event {
                    let _ = ev_tx.send(event);
                    notify.post(WM_WORKER_EVENT);
                }
            }
        }
    });

    Ok(WorkerHandle {
        ev_rx,
        join: handle,
    })
}
