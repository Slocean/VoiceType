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

mod control;
mod diagnostics;
mod model_manager;
mod settings;
mod settings_native;
mod settings_web;
mod ui_main;
mod updater;

// ---------- 入口 ----------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--self-check")) {
        return self_check();
    }
    let Some(_instance) = acquire_single_instance()? else {
        return Ok(());
    };
    diagnostics::record("INFO", "startup", updater::APP_VERSION);
    // 启动即后台检查/修复模型（含首次解压，约 50MB 只在首次发生）；
    // 主线程不做散列与写入，模型状态经 manager 查询，UI 收到通知后刷新。
    let manager = model_manager::init_global();
    manager.start_check_and_repair();
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
    let manager = model_manager::init_global();
    let status = manager.run_check_and_repair();
    let dir = match status {
        model_manager::ModelStatus::Ready { dir, .. } => dir,
        other => return Err(format!("模型自检失败：{}", other.summary()).into()),
    };
    let asr_paths = manager
        .scan_asr_files(&dir)
        .ok_or_else(|| format!("识别模型不完整: {}", dir.display()))?;
    let vad_path = manager
        .find_vad_model(&dir, dir.parent().unwrap_or(Path::new(".")))
        .ok_or_else(|| format!("VAD 模型不完整: {}", dir.display()))?;
    let _asr = voicetype_asr::SherpaAsr::new(&engine_config(asr_paths))?;
    let _vad = voicetype_asr::SherpaVad::new(&voicetype_asr::SherpaVadConfig {
        model: vad_path,
        ..Default::default()
    })?;
    Ok(())
}

/// 由扫描到的模型文件路径构造识别引擎配置（int8 优先在扫描时决定）。
fn engine_config(paths: model_manager::AsrFilePaths) -> SherpaAsrConfig {
    SherpaAsrConfig {
        encoder: paths.encoder,
        decoder: paths.decoder,
        joiner: paths.joiner,
        tokens: paths.tokens,
        num_threads: 2,
    }
}

// ---------- 常量 ----------

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_WORKER_EVENT: u32 = WM_APP + 2;
/// 更新后台任务（检查/下载/公告）完成事件。
const WM_UPDATE_EVENT: u32 = WM_APP + 3;
const WM_SETTINGS_READY: u32 = WM_APP + 4;
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

// 模型目录解析已统一收敛到 model_manager（显式目录/默认目录优先级、清单校验、
// 事务修复），调用方不再自行拼装文件存在性检查。

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

#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// 设置存储（注册表实现；测试通过控制层注入）。
    store: std::sync::Arc<dyn settings::SettingsStore>,
    /// 录音开始时捕获的外部前台目标窗口（None=无可靠目标，定稿不注入）。
    record_target: Option<isize>,
    /// 混合模式目标失效后暂停实时上屏（恢复目标后自动继续）。
    interim_paused: bool,
    /// 进行中的异步 UI 动作：seq → 动作键；完成时取走并交付最终结果。
    pending: std::collections::HashMap<u64, String>,
    /// 最近一次检查更新结果（ok, 文本）。持久展示，直到下一次请求替换。
    last_check: Option<(bool, String)>,
    /// 最近一次公告获取结果（ok, 文本）。持久展示，直到下一次请求替换。
    last_announcement: Option<(bool, String)>,
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

// ---------- 录音开关与事件处理（主线程，见 ui_main） ----------
