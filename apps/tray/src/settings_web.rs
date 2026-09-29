//! WebView2 设置窗口。界面由内嵌 HTML/CSS 绘制，Rust 保留托盘和录音状态。

use super::*;
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
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
        let hwnd = NonZeroIsize::new(self.0.0 as isize).ok_or(HandleError::Unavailable)?;
        // HostWindow 只在所属 UI 线程使用，窗口存活到 WebView 销毁后。
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))) })
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
    let (mode, recording, vk, mods) = with_app(|app| {
        (app.mode, app.recording, app.hotkey_vk, app.hotkey_mods)
    }).unwrap_or((InjectMode::Hybrid, false, 0x78, 0));
    let model_ready = models_dl::models_ready();
    let root = models_dl::models_root();
    let state = serde_json::json!({
        "version": updater::APP_VERSION,
        "mode": match mode { InjectMode::Paste => "paste", InjectMode::Type => "type", InjectMode::Hybrid => "hybrid" },
        "recording": recording,
        "hotkey": settings::combo_text(vk, mods),
        "hotkeyVk": vk,
        "hotkeyMods": mods,
        "autostart": settings::autostart_enabled(),
        "modelReady": model_ready,
        "modelPath": root.display().to_string(),
    });
    send_js(&format!("window.voiceType && window.voiceType.state({state});"));
}

fn reply(message: &str, ok: bool) {
    let value = serde_json::to_string(message).unwrap_or_else(|_| "\"操作失败\"".into());
    send_js(&format!("window.voiceType && window.voiceType.notice({value}, {ok});"));
}

fn on_ipc(body: &str) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(body) else { return };
    match msg["action"].as_str().unwrap_or_default() {
        "ready" => refresh_all(),
        "mode" => {
            let next = match msg["value"].as_str() {
                Some("paste") => Some(InjectMode::Paste),
                Some("type") => Some(InjectMode::Type),
                Some("hybrid") => Some(InjectMode::Hybrid),
                _ => None,
            };
            if let Some(mode) = next { with_app(|app| app.mode = mode); }
            refresh_all();
        }
        "record" => {
            let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
            if !hwnd.is_invalid() { ui_main::toggle_recording(hwnd); }
            refresh_all();
        }
        "hotkey" => {
            let vk = msg["vk"].as_u64().unwrap_or(0) as u32;
            let mods = msg["mods"].as_u64().unwrap_or(0) as u32;
            if !(0x08..=0xFE).contains(&vk) || mods & !0x7 != 0 {
                reply("请先按下有效的快捷键", false);
            } else if ui_main::apply_new_hotkey(vk, mods) {
                if let Err(e) = settings::save_hotkey(vk, mods) { reply(&e, false); }
                else { reply("快捷键已应用", true); }
            } else {
                reply("快捷键被其他应用占用，请换一个组合", false);
            }
            refresh_all();
        }
        "autostart" => {
            let value = msg["value"].as_bool().unwrap_or(false);
            if let Err(e) = settings::set_autostart(value) { reply(&e, false); }
            refresh_all();
        }
        "checkUpdate" => {
            let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
            if !hwnd.is_invalid() {
                reply("正在检查更新…", true);
                updater::spawn_check_and_download(hwnd.0 as isize);
            }
        }
        "announcement" => {
            let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
            if !hwnd.is_invalid() { updater::spawn_fetch_announcement(hwnd.0 as isize); }
        }
        "repairModels" => {
            match models_dl::ensure_models_extracted() {
                Ok(_) if models_dl::models_ready() => reply("模型已就绪", true),
                Ok(_) => reply("模型文件仍不完整，请检查模型目录", false),
                Err(e) => {
                    diagnostics::record("ERROR", "models", &e);
                    reply(&e, false);
                }
            }
            refresh_all();
        }
        _ => {}
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
    let mut rc = RECT { left: 0, top: 0, right: CLIENT_W, bottom: CLIENT_H };
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
        .join("VoiceType").join("WebView2");
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
            let message = wide(&format!("现代界面启动失败：{e}\n\n请安装或修复 Microsoft Edge WebView2 Runtime。"));
            let _ = MessageBoxW(Some(hwnd), PCWSTR(message.as_ptr()), w!("VoiceType"), MB_ICONWARNING);
            let _ = DestroyWindow(hwnd);
        }
    }
}

pub fn fill_announcement(text: &str) -> bool {
    let hwnd = with_app(|app| app.settings).unwrap_or_default();
    if hwnd.is_invalid() || !unsafe { IsWindowVisible(hwnd).as_bool() } { return false; }
    let payload = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    send_js(&format!("window.voiceType && window.voiceType.announcement({payload});"));
    true
}

pub fn refresh_update_status() { refresh_all(); }

pub fn set_model_error(message: &str) {
    let payload = serde_json::to_string(message).unwrap_or_else(|_| "\"模型不可用\"".into());
    send_js(&format!("window.voiceType && window.voiceType.modelError({payload});"));
}
