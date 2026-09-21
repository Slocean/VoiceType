//! VoiceType 托盘常驻应用。
//!
//! 线程模型：
//! - 主线程跑 Win32 消息循环，持有全部 UI 状态（托盘/悬浮条/注入器），
//!   注入动作只在主线程执行；
//! - Worker 线程负责录音与识别：音频回调（cpal 音频线程）内做流式重采样
//!   与 sherpa 解码，识别事件经 mpsc + PostMessage 通知主线程；
//! - 两条线程只通过 Cmd/Ev 两个通道交互，无共享可变状态（asr 本体在
//!   worker 内部以 Arc<Mutex> 与音频回调共享，不暴露给主线程）。
//!
//! 交互：F9 开始录音（悬浮条显示中间结果，hybrid 模式同步上屏到目标应用），
//! 再按 F9 结束并注入定稿；托盘右键菜单切换注入模式 / 退出。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use voicetype_asr::SherpaAsrConfig;
use voicetype_inject::{
    ClipboardInjector, HybridInjector, StreamingInjector, TextInjector, TypingInjector,
};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_NOREPEAT, VK_F9};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

mod worker;
use worker::start_worker;

mod ui_main;

// ---------- 入口 ----------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ui_main::run()
}

// ---------- 常量 ----------

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_WORKER_EVENT: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;
const TRAY_UID: u32 = 1;

const MENU_MODE_PASTE: i32 = 1001;
const MENU_MODE_TYPE: i32 = 1002;
const MENU_MODE_HYBRID: i32 = 1003;
const MENU_QUIT: i32 = 1004;

const OVERLAY_W: i32 = 520;
const OVERLAY_H: i32 = 72;
/// STATIC 控件样式：居中 + 垂直居中。
const SS_CENTER_TEXT: u32 = 0x1 | 0x200;

/// 默认模型目录（相对工作目录），可用 VOICETYPE_MODEL_DIR 环境变量覆盖。
const DEFAULT_MODEL_DIR: &str = "models/sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23";

// ---------- 线程间消息 ----------

enum Cmd {
    Stop,
}

#[allow(dead_code)]
enum Ev {
    Interim(String),
    Final(String),
    /// VAD 断句但无语音内容（静默会话），上层静默收尾。
    Empty,
    Error(String),
}

/// 可跨线程发送的窗口句柄包装，仅用于 PostMessage 唤醒主线程。
#[derive(Clone, Copy)]
struct NotifyHwnd(HWND);
unsafe impl Send for NotifyHwnd {}

impl NotifyHwnd {
    fn post(&self, msg: u32) {
        unsafe {
            let _ = PostMessageW(Some(self.0), msg, WPARAM(0), LPARAM(0));
        }
    }
}

// ---------- 应用状态（仅主线程访问） ----------

#[derive(Clone, Copy, PartialEq)]
enum InjectMode {
    Paste,
    Type,
    Hybrid,
}

struct App {
    mode: InjectMode,
    recording: bool,
    cmd_tx: Option<Sender<Cmd>>,
    ev_rx: Option<Receiver<Ev>>,
    worker: Option<JoinHandle<()>>,
    overlay: HWND,
    overlay_text: HWND,
    hybrid: HybridInjector,
}

thread_local! {
    static APP: std::cell::RefCell<Option<App>> = const { std::cell::RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

// ---------- 工具 ----------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_window_text(hwnd: HWND, text: &str) {
    let buf = wide(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(buf.as_ptr()));
    }
}

/// 在模型目录内定位四件模型文件（优先 int8 量化版）。
fn find_model_files(dir: &Path) -> Option<SherpaAsrConfig> {
    let pick = |prefix: &str| -> Option<PathBuf> {
        // 文件名带 epoch/avg 后缀，按前缀扫描目录，int8 量化版优先。
        let rd = std::fs::read_dir(dir).ok()?;
        let mut candidates: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(prefix) && n.ends_with(".onnx"))
                    .unwrap_or(false)
            })
            .collect();
        candidates.sort_by_key(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if name.contains(".int8.") { 0 } else { 1 }
        });
        candidates.into_iter().next()
    };
    Some(SherpaAsrConfig {
        encoder: pick("encoder")?,
        decoder: pick("decoder")?,
        joiner: pick("joiner")?,
        tokens: {
            let t = dir.join("tokens.txt");
            t.exists().then_some(t)?
        },
        num_threads: 2,
    })
}

/// 在模型目录内定位 VAD 模型（silero_vad.onnx）。
fn find_vad_model(model_dir: &Path) -> Option<PathBuf> {
    for name in ["silero_vad.onnx", "vad.onnx"] {
        let p = model_dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// ---------- 录音开关与事件处理（主线程） ----------

// ---------- 录音开关与事件处理（主线程，见 ui_main） ----------
