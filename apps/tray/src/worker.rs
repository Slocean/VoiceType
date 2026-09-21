//! 录音 + 识别 Worker：把音频回调、VAD 会话与事件转发收敛在一个模块。
//!
//! 约束：
//! - 音频回调（cpal 音频线程）内只做：lock 会话 → 喂音频 → 转发事件，
//!   绝不阻塞等待、不在回调内停流（cpal 会死锁）；
//! - worker 线程等待退出条件（VAD 自动断句或手动停止），退出前停流；
//! - 注入只在主线程执行，本模块只发 Ev。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
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
    _cmd_tx: Sender<Cmd>,
) -> Result<WorkerHandle, String> {
    use std::sync::mpsc;

    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    // done 置位即请求 worker 退出（手动停止）；VAD 自动断句时由内部置位。
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
            if stream.is_err() {
                let _ = ev_tx.send(Ev::Error("打开麦克风失败".into()));
                notify.post(WM_WORKER_EVENT);
                return;
            }

            // 等待退出条件：VAD 自动断句（done 置位）或手动停止（Cmd::Stop）。
            loop {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                if cmd_rx.recv_timeout(Duration::from_millis(100)).is_ok() {
                    break;
                }
            }
        }
    });

    Ok(WorkerHandle {
        ev_rx,
        join: handle,
    })
}
