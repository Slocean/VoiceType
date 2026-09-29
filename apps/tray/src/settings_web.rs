//! WebView2 设置窗口。界面由内嵌 HTML/CSS 绘制，Rust 保留托盘和录音状态。

use super::control::{self, UiAction};
use super::model_manager::{self, ModelStatus};
use super::*;
use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use std::cell::RefCell;
use std::num::NonZeroIsize;
use wry::{dpi, PageLoadEvent, Rect as WebRect, WebContext, WebView, WebViewBuilder};

const CLIENT_W: i32 = 760;
const CLIENT_H: i32 = 610;
const SETTINGS_CLASS: PCWSTR = w!("VoiceTypeWebSettings");

thread_local! {
    static WEBVIEW: RefCell<Option<WebView>> = const { RefCell::new(None) };
}

struct HostWindow(HWND);

impl HasWindowHandle for HostWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let hwnd = NonZeroIsize::new(self.0 .0 as isize).ok_or(HandleError::Unavailable)?;
        // HostWindow 只在所属 UI 线程使用，窗口存活到 WebView 销毁后。
        Ok(unsafe {
            WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(hwnd)))
        })
    }
}

fn send_js(script: &str) {
    WEBVIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            let _ = view.evaluate_script(script);
        }
    });
}

pub fn refresh_all() {
    // 后台模型检查/修复完成：交付挂起动作的最终结果（take_pending 只交付一次）。
    if let Some(manager) = model_manager::global() {
        if !manager.is_busy() {
            if let Some(seq) = control::take_pending("repairModels") {
                let status = manager.status();
                deliver_done(seq, "repairModels", status.is_ready(), &status.summary());
                return; // deliver_done 内部会再次 refresh_all
            }
        }
    }
    let (mode, recording, vk, mods) =
        with_app(|app| (app.mode, app.recording, app.hotkey_vk, app.hotkey_mods)).unwrap_or((
            InjectMode::Hybrid,
            false,
            0x78,
            0,
        ));
    let autostart = with_app(|app| app.store.autostart_enabled()).unwrap_or(false);
    let pending: Vec<String> =
        with_app(|app| app.pending.values().cloned().collect()).unwrap_or_default();
    let model = model_manager::global().map(|m| m.status());
    let (model_ready, model_status, model_path) = match &model {
        Some(status) => {
            let dir = match status {
                ModelStatus::Ready { dir, .. } => dir.display().to_string(),
                ModelStatus::Corrupt { dir, .. } | ModelStatus::Repairing { dir } => {
                    dir.display().to_string()
                }
                ModelStatus::Missing { dir, .. } | ModelStatus::PermissionDenied { dir, .. } => {
                    dir.display().to_string()
                }
                _ => String::new(),
            };
            (status.is_ready(), status.summary(), dir)
        }
        None => (false, "模型模块未初始化".into(), String::new()),
    };
    let state = serde_json::json!({
        "version": updater::APP_VERSION,
        "mode": match mode { InjectMode::Paste => "paste", InjectMode::Type => "type", InjectMode::Hybrid => "hybrid" },
        "recording": recording,
        "hotkey": settings::combo_text(vk, mods),
        "hotkeyVk": vk,
        "hotkeyMods": mods,
        "autostart": autostart,
        "pending": pending,
        "modelReady": model_ready,
        "modelStatus": model_status,
        "modelPath": model_path,
    });
    send_js(&format!(
        "window.voiceType && window.voiceType.state({state});"
    ));
}

/// 交付"进行中"结果：界面保持按钮禁用与提示。
pub fn deliver_running(seq: u64, action: &str, message: &str) {
    let value = serde_json::json!({
        "seq": seq, "action": action, "state": "running", "message": message,
    });
    send_js(&format!(
        "window.voiceType && window.voiceType.result({value});"
    ));
}

/// 交付最终结果：每个动作恰好一次。随后推送全量状态刷新界面。
pub fn deliver_done(seq: u64, action: &str, ok: bool, message: &str) {
    let value = serde_json::json!({
        "seq": seq, "action": action, "state": "done", "ok": ok, "message": message,
    });
    send_js(&format!(
        "window.voiceType && window.voiceType.result({value});"
    ));
    refresh_all();
}

/// 隐藏设置窗口（首页"返回目标应用"动作）。
pub fn hide_settings() {
    let hwnd = with_app(|app| app.settings).unwrap_or_default();
    if !hwnd.is_invalid() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// IPC 入口只做 JSON 解析、字段校验和分发；业务决策集中在控制层。
fn on_ipc(body: &str) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(body) else {
        deliver_done(0, "invalid", false, "无法解析的请求");
        return;
    };
    let seq = msg["seq"].as_u64().unwrap_or(0);
    match msg["action"].as_str().unwrap_or_default() {
        "ready" => refresh_all(),
        "mode" => match msg["value"].as_str() {
            Some("paste") => control::dispatch(seq, UiAction::SetMode(InjectMode::Paste)),
            Some("type") => control::dispatch(seq, UiAction::SetMode(InjectMode::Type)),
            Some("hybrid") => control::dispatch(seq, UiAction::SetMode(InjectMode::Hybrid)),
            _ => deliver_done(seq, "mode", false, "未知的输入方式"),
        },
        "hotkey" => {
            let vk = msg["vk"].as_u64().unwrap_or(0) as u32;
            let mods = msg["mods"].as_u64().unwrap_or(0) as u32;
            control::dispatch(seq, UiAction::SetHotkey { vk, mods });
        }
        "autostart" => match msg["value"].as_bool() {
            Some(value) => control::dispatch(seq, UiAction::SetAutostart(value)),
            None => deliver_done(seq, "autostart", false, "缺少布尔值参数"),
        },
        "record" => control::dispatch(seq, UiAction::RequestRecord),
        "checkUpdate" => control::dispatch(seq, UiAction::CheckUpdate),
        "announcement" => control::dispatch(seq, UiAction::FetchAnnouncement),
        "repairModels" => control::dispatch(seq, UiAction::RepairModels),
        other => deliver_done(seq, "unknown", false, &format!("不支持的操作：{other}")),
    }
}

/// 打开或前置设置窗口。顶层窗口仅作 WebView2 容器，不创建 Win32 表单控件。
pub unsafe fn show_settings(_main_hwnd: HWND) {
    let existing = with_app(|app| app.settings).unwrap_or_default();
    if !existing.is_invalid() {
        let _ = ShowWindow(existing, SW_SHOW);
        let _ = SetForegroundWindow(existing);
        refresh_all();
        return;
    }

    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(ui_main::wndproc),
        hInstance: hinst,
        lpszClassName: SETTINGS_CLASS,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: CLIENT_W,
        bottom: CLIENT_H,
    };
    let _ = AdjustWindowRect(&mut rc, style, false);
    let width = rc.right - rc.left;
    let height = rc.bottom - rc.top;
    let hwnd = match CreateWindowExW(
        WS_EX_APPWINDOW,
        SETTINGS_CLASS,
        w!("VoiceType"),
        style,
        (GetSystemMetrics(SM_CXSCREEN) - width) / 2,
        (GetSystemMetrics(SM_CYSCREEN) - height) / 2,
        width,
        height,
        None,
        None,
        Some(hinst),
        None,
    ) {
        Ok(hwnd) => hwnd,
        Err(e) => {
            diagnostics::record("ERROR", "settings", &e.to_string());
            return;
        }
    };

    let data_dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("VoiceType")
        .join("WebView2");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut context = WebContext::new(Some(data_dir));
    let bounds = WebRect {
        position: dpi::LogicalPosition::new(0, 0).into(),
        size: dpi::LogicalSize::new(CLIENT_W, CLIENT_H).into(),
    };
    let host = HostWindow(hwnd);
    let view = WebViewBuilder::new_with_web_context(&mut context)
        .with_bounds(bounds)
        .with_html(include_str!("settings.html"))
        .with_ipc_handler(|request| on_ipc(request.body()))
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, PageLoadEvent::Finished) {
                let _ = PostMessageW(Some(hwnd), WM_SETTINGS_READY, WPARAM(0), LPARAM(0));
            }
        })
        .build_as_child(&host);
    match view {
        Ok(view) => {
            WEBVIEW.with(|slot| *slot.borrow_mut() = Some(view));
            with_app(|app| app.settings = hwnd);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
        Err(e) => {
            diagnostics::record("ERROR", "settings", &e.to_string());
            let message = wide(&format!(
                "现代界面启动失败：{e}\n\n请安装或修复 Microsoft Edge WebView2 Runtime。"
            ));
            let _ = MessageBoxW(
                Some(hwnd),
                PCWSTR(message.as_ptr()),
                w!("VoiceType"),
                MB_ICONWARNING,
            );
            let _ = DestroyWindow(hwnd);
        }
    }
}

pub fn fill_announcement(text: &str) -> bool {
    let hwnd = with_app(|app| app.settings).unwrap_or_default();
    if hwnd.is_invalid() || !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return false;
    }
    let payload = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    send_js(&format!(
        "window.voiceType && window.voiceType.announcement({payload});"
    ));
    true
}

pub fn refresh_update_status() {
    refresh_all();
}
