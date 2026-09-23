//! VoiceType 托盘常驻应用。
//!
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

// GUI 子系统：托盘应用启动不弹控制台窗口。
#![windows_subsystem = "windows"]

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
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::MOD_NOREPEAT;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

mod worker;
use worker::start_worker;

mod ui_main;
mod updater;
mod models_dl;
mod settings;
mod diagnostics;

// ---------- 入口 ----------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--self-check")) {
        return self_check();
    }
    let Some(_instance) = acquire_single_instance()? else {
        return Ok(());
    };
    diagnostics::record("INFO", "startup", updater::APP_VERSION);
    // 首次启动把内置模型解压到模型目录（幂等，约 50MB 只在首次发生）。
    if let Err(e) = models_dl::ensure_models_extracted() {
        // 解压失败不阻断启动：F9 时会给出明确提示。
        diagnostics::record("ERROR", "models", &e);
        eprintln!("内置模型解压失败: {e}");
    }
    ui_main::run()
}

/// 同一登录会话只运行一个托盘实例。重复启动时打开已运行实例的主界面。
struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn acquire_single_instance() -> Result<Option<InstanceGuard>, Box<dyn std::error::Error>> {
    unsafe {
        let mutex = CreateMutexW(None, false, w!("Local\\Slocean.VoiceType.Tray"))?;
        let existing = GetLastError() == ERROR_ALREADY_EXISTS;
        let guard = InstanceGuard(mutex);
        if existing {
            if let Ok(hwnd) = FindWindowW(w!("VoiceTypeMainWindow"), None) {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_TRAY_CALLBACK,
                    WPARAM(0),
                    LPARAM(WM_LBUTTONDBLCLK as isize),
                );
            }
            return Ok(None);
        }
        Ok(Some(guard))
    }
}

/// 发布验收入口：用最终 exe 解压内置模型并初始化真实 ASR/VAD，无需麦克风或 UI。
fn self_check() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(feature = "embedded-models") {
        return Err("自检要求使用内置模型构建".into());
    }
    models_dl::ensure_models_extracted()?;
    let model_dir = resolve_model_dir();
    let asr_config = find_model_files(&model_dir)
        .ok_or_else(|| format!("识别模型不完整: {}", model_dir.display()))?;
    let vad_path = find_vad_model(&model_dir)
        .ok_or_else(|| format!("VAD 模型不完整: {}", model_dir.display()))?;
    let _asr = voicetype_asr::SherpaAsr::new(&asr_config)?;
    let _vad = voicetype_asr::SherpaVad::new(&voicetype_asr::SherpaVadConfig {
        model: vad_path,
        ..Default::default()
    })?;
    Ok(())
}

// ---------- 常量 ----------

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_WORKER_EVENT: u32 = WM_APP + 2;
/// 更新后台任务（检查/下载/公告）完成事件。
const WM_UPDATE_EVENT: u32 = WM_APP + 3;
const HOTKEY_ID: i32 = 1;
const TRAY_UID: u32 = 1;

const MENU_MODE_PASTE: i32 = 1001;
const MENU_MODE_TYPE: i32 = 1002;
const MENU_MODE_HYBRID: i32 = 1003;
const MENU_UPDATE_CHECK: i32 = 1005;
const MENU_ANNOUNCEMENT: i32 = 1006;
const MENU_MAIN: i32 = 1008;
const MENU_QUIT: i32 = 1004;

const OVERLAY_W: i32 = 520;
const OVERLAY_H: i32 = 72;
/// STATIC 控件样式：居中 + 垂直居中。
const SS_CENTER_TEXT: u32 = 0x1 | 0x200;

/// 与内置模型解压和就绪检查共用模型路径；兼容环境变量直接指向识别模型目录。
fn resolve_model_dir() -> PathBuf {
    let root = models_dl::models_root();
    if root.join("tokens.txt").is_file() {
        return root;
    }
    let current = root.join(models_dl::MODEL_SUBDIR);
    if find_model_files(&current).is_some() && find_vad_model(&current).is_some() {
        return current;
    }
    let legacy = root.join(models_dl::LEGACY_MODEL_SUBDIR);
    if find_model_files(&legacy).is_some() && find_vad_model(&legacy).is_some() {
        return legacy;
    }
    current
}

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
    stopping: bool,
    cmd_tx: Option<Sender<Cmd>>,
    ev_rx: Option<Receiver<Ev>>,
    worker: Option<JoinHandle<()>>,
    overlay: HWND,
    overlay_text: HWND,
    hybrid: HybridInjector,
    /// 全局热键和托盘消息所绑定的隐藏主窗口。
    main_hwnd: HWND,
    /// 主界面窗口（首次打开时创建，关闭=隐藏复用）。
    settings: HWND,
    /// 录音热键（用户可自定义，注册表持久化；vk 为虚拟键码，mods 为 MOD_* 组合）。
    hotkey_vk: u32,
    hotkey_mods: u32,
    /// 热键组合的显示文本（如 "Ctrl + F9"），托盘提示与悬浮条文案用。
    hotkey_name: String,
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
                p.is_file()
                    && p.file_name()
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
            t.is_file().then_some(t)?
        },
        num_threads: 2,
    })
}

/// 在识别模型目录及其父目录（models\ 根）内定位 VAD 模型。
fn find_vad_model(model_dir: &Path) -> Option<PathBuf> {
    let mut dirs = vec![model_dir.to_path_buf()];
    if let Some(parent) = model_dir.parent() {
        dirs.push(parent.to_path_buf());
    }
    for dir in dirs {
        for name in ["silero_vad.onnx", "vad.onnx"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

// ---------- 录音开关与事件处理（主线程，见 ui_main） ----------
