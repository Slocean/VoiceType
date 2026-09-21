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
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use voicetype_asr::{SherpaAsr, SherpaAsrConfig, StreamingAsr};
use voicetype_audio::AudioStream;
use voicetype_inject::{
    ClipboardInjector, HybridInjector, StreamingInjector, TextInjector, TypingInjector,
};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_F9,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

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

enum Ev {
    Interim(String),
    Final(String),
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

// ---------- Worker：录音 + 识别 ----------

fn start_worker(notify: NotifyHwnd) -> Result<(Sender<Cmd>, Receiver<Ev>, JoinHandle<()>), String> {
    let model_dir = std::env::var("VOICETYPE_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_MODEL_DIR));
    let model_cfg = find_model_files(&model_dir)
        .ok_or_else(|| format!("模型文件缺失，请检查目录: {}", model_dir.display()))?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();

    let handle = std::thread::spawn(move || {
        let asr = match SherpaAsr::new(&model_cfg) {
            Ok(a) => Arc::new(Mutex::new(a)),
            Err(e) => {
                let _ = ev_tx.send(Ev::Error(format!("识别引擎初始化失败: {e}")));
                notify.post(WM_WORKER_EVENT);
                return;
            }
        };

        let asr_in_cb = Arc::clone(&asr);
        let ev_in_cb = ev_tx.clone();
        let stream = AudioStream::start(move |chunk| {
            if let Ok(mut engine) = asr_in_cb.lock() {
                if engine.accept_samples(chunk).is_ok() {
                    while let Ok(Some(r)) = engine.take_result() {
                        let _ = ev_in_cb.send(Ev::Interim(r.text));
                        notify.post(WM_WORKER_EVENT);
                    }
                }
            }
        });
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                let _ = ev_tx.send(Ev::Error(format!("打开麦克风失败: {e}")));
                notify.post(WM_WORKER_EVENT);
                return;
            }
        };

        // 阻塞等待停止命令，之后收尾产出定稿。
        let _ = cmd_rx.recv();
        drop(stream);
        if let Ok(mut engine) = asr.lock() {
            let _ = engine.finish();
            let mut text = String::new();
            while let Ok(Some(r)) = engine.take_result() {
                text = r.text;
            }
            let _ = ev_tx.send(Ev::Final(text));
            notify.post(WM_WORKER_EVENT);
        };
    });

    Ok((cmd_tx, ev_rx, handle))
}

// ---------- 录音开关与事件处理（主线程） ----------

fn toggle_recording(main_hwnd: HWND) {
    with_app(|app| {
        if app.recording {
            // 结束录音：通知 worker 收尾，定稿经 Ev::Final 异步回来。
            if let Some(tx) = app.cmd_tx.take() {
                let _ = tx.send(Cmd::Stop);
            }
            set_window_text(app.overlay_text, "识别中…");
        } else {
            match start_worker(NotifyHwnd(main_hwnd)) {
                Ok((tx, rx, handle)) => {
                    app.cmd_tx = Some(tx);
                    app.ev_rx = Some(rx);
                    app.worker = Some(handle);
                    app.recording = true;
                    set_window_text(app.overlay_text, "🎤 录音中…（按 F9 结束）");
                    unsafe {
                        let _ = ShowWindow(app.overlay, SW_SHOW);
                    }
                }
                Err(e) => {
                    set_window_text(app.overlay_text, &format!("启动失败: {e}"));
                    unsafe {
                        let _ = ShowWindow(app.overlay, SW_SHOW);
                    }
                }
            }
        }
    });
}

fn drain_worker_events() {
    // 先收集事件再处理，避免在持锁回调里嵌套操作 App。
    let events: Vec<Ev> = with_app(|app| {
        let mut out = Vec::new();
        if let Some(rx) = &app.ev_rx {
            while let Ok(ev) = rx.try_recv() {
                out.push(ev);
            }
        }
        out
    })
    .unwrap_or_default();

    for ev in events {
        match ev {
            Ev::Interim(text) => {
                with_app(|app| {
                    if !app.recording {
                        return;
                    }
                    set_window_text(app.overlay_text, &format!("🎤 {text}"));
                    if app.mode == InjectMode::Hybrid {
                        let _ = app.hybrid.update_interim(&text);
                    }
                });
            }
            Ev::Final(text) => {
                with_app(|app| {
                    app.recording = false;
                    app.ev_rx = None;
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                    unsafe {
                        let _ = ShowWindow(app.overlay, SW_HIDE);
                    }
                    match app.mode {
                        InjectMode::Paste => {
                            let _ = ClipboardInjector::default().inject(&text);
                        }
                        InjectMode::Type => {
                            let _ = TypingInjector::default().inject(&text);
                        }
                        InjectMode::Hybrid => {
                            let _ = if text.is_empty() {
                                app.hybrid.cancel()
                            } else {
                                app.hybrid.commit(&text)
                            };
                        }
                    }
                });
            }
            Ev::Error(msg) => {
                with_app(|app| {
                    app.recording = false;
                    app.ev_rx = None;
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                    let _ = app.hybrid.cancel();
                    set_window_text(app.overlay_text, &format!("⚠ {msg}"));
                });
            }
        }
    }
}

// ---------- 托盘 ----------

unsafe fn add_tray_icon(hwnd: HWND) {
    let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    let tip = wide("VoiceType 语音输入（F9 开始/停止）");
    data.szTip[..tip.len() - 1].copy_from_slice(&tip[..tip.len() - 1]);
    let _ = Shell_NotifyIconW(NIM_ADD, &data);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let mode = with_app(|app| app.mode).unwrap_or(InjectMode::Hybrid);
    let Ok(menu) = CreatePopupMenu() else { return };
    let item = |id: i32, label: &str, checked: bool| {
        let flags = if checked { MF_STRING | MF_CHECKED } else { MF_STRING };
        let text = wide(label);
        let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(text.as_ptr()));
    };
    item(MENU_MODE_PASTE, "粘贴模式", mode == InjectMode::Paste);
    item(MENU_MODE_TYPE, "键入模式", mode == InjectMode::Type);
    item(MENU_MODE_HYBRID, "混合模式（流式）", mode == InjectMode::Hybrid);
    let sep = wide("");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR(sep.as_ptr()));
    let quit = wide("退出");
    let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, PCWSTR(quit.as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // TrackPopupMenu 要求窗口在前台，否则点击菜单外区域不收起菜单。
    let _ = SetForegroundWindow(hwnd);
    // TPM_RETURNCMD：返回值即菜单项 id（0 表示取消），不走 WM_COMMAND。
    let ret = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);

    match ret.0 {
        MENU_MODE_PASTE => {
            with_app(|app| app.mode = InjectMode::Paste);
        }
        MENU_MODE_TYPE => {
            with_app(|app| app.mode = InjectMode::Type);
        }
        MENU_MODE_HYBRID => {
            with_app(|app| app.mode = InjectMode::Hybrid);
        }
        MENU_QUIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

// ---------- 窗口过程 ----------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if wparam.0 as i32 == HOTKEY_ID {
                toggle_recording(hwnd);
            }
            LRESULT(0)
        }
        WM_WORKER_EVENT => {
            drain_worker_events();
            LRESULT(0)
        }
        WM_TRAY_CALLBACK => {
            let mouse = (lparam.0 as u32) & 0xFFFF;
            if mouse == WM_RBUTTONUP {
                show_tray_menu(hwnd);
            } else if mouse == WM_LBUTTONDBLCLK {
                toggle_recording(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // 退出前收尾：移除托盘、停掉进行中的录音。
            remove_tray_icon(hwnd);
            let _ = UnregisterHotKey(None, HOTKEY_ID);
            with_app(|app| {
                if app.recording {
                    if let Some(tx) = app.cmd_tx.take() {
                        let _ = tx.send(Cmd::Stop);
                    }
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------- 入口 ----------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();


        // 主窗口：隐藏的消息窗口（热键/托盘回调的载体）。
        let class = w!("VoiceTypeMainWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);



        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("VoiceType"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )?;

        // 悬浮条：topmost + toolwindow + noactivate，不抢目标应用焦点。
        let overlay_class = w!("VoiceTypeOverlay");
        let wc2 = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: overlay_class,
            ..Default::default()
        };
        RegisterClassW(&wc2);


        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let overlay = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            overlay_class,
            w!("VoiceType"),
            WS_POPUP,
            (screen_w - OVERLAY_W) / 2,
            screen_h - OVERLAY_H - 120,
            OVERLAY_W,
            OVERLAY_H,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let overlay_text = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("VoiceType 就绪"),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | SS_CENTER_TEXT),
            0,
            0,
            OVERLAY_W,
            OVERLAY_H,
            Some(overlay),
            None,
            Some(hinst),
            None,
        )?;

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                mode: InjectMode::Hybrid,
                recording: false,
                cmd_tx: None,
                ev_rx: None,
                worker: None,
                overlay,
                overlay_text,
                hybrid: HybridInjector::default(),
            });
        });


        RegisterHotKey(Some(hwnd), HOTKEY_ID, MOD_NOREPEAT, VK_F9.0 as u32)?;
        add_tray_icon(hwnd);


        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
