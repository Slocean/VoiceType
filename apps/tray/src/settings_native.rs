//! 原生备用设置窗口（工作包 C2）。
//!
//! WebView2 Runtime 缺失或创建失败时的可操作界面：注入模式、热键、自启、
//! 模型状态/修复、检查更新/公告六类功能全部经同一控制层（control::dispatch）
//! 执行，不复制任何业务逻辑。

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};

use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

use super::control;
use super::*;

const NATIVE_CLASS: PCWSTR = w!("VoiceTypeNativeSettings");

// 控件 ID。
const ID_MODE_PASTE: i32 = 2001;
const ID_MODE_TYPE: i32 = 2002;
const ID_MODE_HYBRID: i32 = 2003;
const ID_AUTOSTART: i32 = 2004;
const ID_REPAIR: i32 = 2005;
const ID_CHECK_UPDATE: i32 = 2006;
const ID_ANNOUNCEMENT: i32 = 2007;
const ID_HOTKEY_CAPTURE: i32 = 2008;
const ID_MODEL_STATUS: i32 = 2100;
const ID_RESULT: i32 = 2101;
const ID_HOTKEY_LABEL: i32 = 2102;

thread_local! {
    static FALLBACK: RefCell<Option<HWND>> = const { RefCell::new(None) };
    /// 热键捕获模式：下一次 WM_KEYDOWN 作为新热键提交。
    static CAPTURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    let buf = wide(text);
    let _ = SetWindowTextW(hwnd, PCWSTR(buf.as_ptr()));
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    hinst: HINSTANCE,
) -> HWND {
    let label = wide(text);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(label.as_ptr()),
        WINDOW_STYLE(style | WS_CHILD.0 | WS_VISIBLE.0),
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    // 统一默认 GUI 字体（默认系统字体过粗且不随 DPI）。
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let _ = SendMessageW(
        hwnd,
        WM_SETFONT,
        Some(WPARAM(font.0 as usize)),
        Some(LPARAM(0)),
    );
    hwnd
}

/// 打开（或前置）原生备用设置窗口。
pub unsafe fn show_fallback(_main_hwnd: HWND) {
    FALLBACK.with(|slot| {
        if let Some(existing) = *slot.borrow() {
            let _ = ShowWindow(existing, SW_SHOW);
            let _ = SetForegroundWindow(existing);
            refresh_fallback();
            return;
        }
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(native_wndproc),
            hInstance: hinst,
            lpszClassName: NATIVE_CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let scale = GetDpiForSystem() as f32 / 96.0;
        let s = |v: i32| (v as f32 * scale).round() as i32;
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: s(460),
            bottom: s(560),
        };
        let _ = AdjustWindowRect(&mut rc, style, false);
        let hwnd = match CreateWindowExW(
            WS_EX_APPWINDOW,
            NATIVE_CLASS,
            w!("VoiceType（基础设置）"),
            style,
            (GetSystemMetrics(SM_CXSCREEN) - (rc.right - rc.left)) / 2,
            (GetSystemMetrics(SM_CYSCREEN) - (rc.bottom - rc.top)) / 2,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                diagnostics::record("ERROR", "settings", &format!("备用窗口创建失败：{e}"));
                return;
            }
        };

        let group = WS_GROUP.0 | BS_GROUPBOX as u32;
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "输入方式",
            group,
            0,
            s(16),
            s(14),
            s(200),
            s(96),
            hinst,
        );
        let radio = BS_AUTORADIOBUTTON as u32;
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "粘贴模式",
            radio,
            ID_MODE_PASTE,
            s(30),
            s(38),
            s(170),
            s(20),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "逐字键入",
            radio,
            ID_MODE_TYPE,
            s(30),
            s(62),
            s(170),
            s(20),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "混合模式（流式）",
            radio,
            ID_MODE_HYBRID,
            s(30),
            s(86),
            s(170),
            s(20),
            hinst,
        );

        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "开机自动启动",
            BS_AUTOCHECKBOX as u32,
            ID_AUTOSTART,
            s(16),
            s(122),
            s(200),
            s(20),
            hinst,
        );

        let _ = create_control(
            hwnd,
            w!("STATIC"),
            "快捷键：",
            0,
            0,
            s(16),
            s(152),
            s(70),
            s(18),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("STATIC"),
            "F9",
            0,
            ID_HOTKEY_LABEL,
            s(90),
            s(152),
            s(160),
            s(18),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "更改快捷键（点击后按新组合）",
            BS_PUSHBUTTON as u32,
            ID_HOTKEY_CAPTURE,
            s(16),
            s(176),
            s(240),
            s(26),
            hinst,
        );

        let _ = create_control(
            hwnd,
            w!("STATIC"),
            "检查中…",
            0,
            ID_MODEL_STATUS,
            s(16),
            s(216),
            s(428),
            s(32),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "重新检查 / 修复",
            BS_PUSHBUTTON as u32,
            ID_REPAIR,
            s(16),
            s(252),
            s(150),
            s(26),
            hinst,
        );

        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "检查更新",
            BS_PUSHBUTTON as u32,
            ID_CHECK_UPDATE,
            s(16),
            s(292),
            s(110),
            s(26),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("BUTTON"),
            "获取更新公告",
            BS_PUSHBUTTON as u32,
            ID_ANNOUNCEMENT,
            s(136),
            s(292),
            s(130),
            s(26),
            hinst,
        );
        let _ = create_control(
            hwnd,
            w!("STATIC"),
            "",
            0,
            ID_RESULT,
            s(16),
            s(326),
            s(428),
            s(190),
            hinst,
        );

        *slot.borrow_mut() = Some(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        refresh_fallback();
    });
}

/// 控制层交付结果后刷新备用窗口控件（webview 不存在时无害）。
pub fn refresh_fallback() {
    FALLBACK.with(|slot| {
        if let Some(hwnd) = *slot.borrow() {
            unsafe { refresh_controls(hwnd) };
        }
    });
}

pub fn on_fallback_destroy(hwnd: HWND) {
    FALLBACK.with(|slot| {
        if *slot.borrow() == Some(hwnd) {
            *slot.borrow_mut() = None;
        }
    });
    CAPTURING.with(|c| c.set(false));
}

unsafe fn refresh_controls(hwnd: HWND) {
    let mode = with_app(|app| app.mode).unwrap_or(InjectMode::Hybrid);
    let _ = SendMessageW(
        GetDlgItem(Some(hwnd), ID_MODE_PASTE).unwrap_or_default(),
        BM_SETCHECK,
        Some(WPARAM((mode == InjectMode::Paste) as usize)),
        Some(LPARAM(0)),
    );
    let _ = SendMessageW(
        GetDlgItem(Some(hwnd), ID_MODE_TYPE).unwrap_or_default(),
        BM_SETCHECK,
        Some(WPARAM((mode == InjectMode::Type) as usize)),
        Some(LPARAM(0)),
    );
    let _ = SendMessageW(
        GetDlgItem(Some(hwnd), ID_MODE_HYBRID).unwrap_or_default(),
        BM_SETCHECK,
        Some(WPARAM((mode == InjectMode::Hybrid) as usize)),
        Some(LPARAM(0)),
    );
    let autostart = with_app(|app| app.store.autostart_enabled()).unwrap_or(false);
    let _ = SendMessageW(
        GetDlgItem(Some(hwnd), ID_AUTOSTART).unwrap_or_default(),
        BM_SETCHECK,
        Some(WPARAM(autostart as usize)),
        Some(LPARAM(0)),
    );
    let hotkey = with_app(|app| app.hotkey_name.clone()).unwrap_or_default();
    set_text(
        GetDlgItem(Some(hwnd), ID_HOTKEY_LABEL).unwrap_or_default(),
        &hotkey,
    );
    let (status_text, busy) = match model_manager::global() {
        Some(manager) => (manager.status().summary(), manager.is_busy()),
        None => ("模型模块未初始化".to_string(), false),
    };
    let status_text = if busy {
        "正在检查 / 修复模型…".to_string()
    } else {
        status_text
    };
    set_text(
        GetDlgItem(Some(hwnd), ID_MODEL_STATUS).unwrap_or_default(),
        &status_text,
    );
    let result = with_app(|app| {
        if let Some((ok, text)) = &app.last_announcement {
            return format!("公告（{}）：{text}", if *ok { "成功" } else { "失败" });
        }
        if let Some((ok, text)) = &app.last_check {
            return format!("检查更新（{}）：{text}", if *ok { "成功" } else { "失败" });
        }
        String::new()
    })
    .unwrap_or_default();
    set_text(
        GetDlgItem(Some(hwnd), ID_RESULT).unwrap_or_default(),
        &result,
    );
}

/// 提交原生窗口发起的动作（结果经 deliver_done → refresh_fallback 回流）。
unsafe fn dispatch_native(seq: u64, action: control::UiAction) {
    control::dispatch(seq, action);
}

unsafe fn on_command(hwnd: HWND, id: i32) {
    if id == 0 {
        return;
    }
    let seq = control::next_seq();
    match id {
        ID_MODE_PASTE => dispatch_native(seq, control::UiAction::SetMode(InjectMode::Paste)),
        ID_MODE_TYPE => dispatch_native(seq, control::UiAction::SetMode(InjectMode::Type)),
        ID_MODE_HYBRID => dispatch_native(seq, control::UiAction::SetMode(InjectMode::Hybrid)),
        ID_AUTOSTART => {
            let enable = SendMessageW(
                GetDlgItem(Some(hwnd), ID_AUTOSTART).unwrap_or_default(),
                BM_GETCHECK,
                None,
                None,
            )
            .0 == 1; // BST_CHECKED
            dispatch_native(seq, control::UiAction::SetAutostart(enable));
        }
        ID_REPAIR => dispatch_native(seq, control::UiAction::RepairModels),
        ID_CHECK_UPDATE => dispatch_native(seq, control::UiAction::CheckUpdate),
        ID_ANNOUNCEMENT => dispatch_native(seq, control::UiAction::FetchAnnouncement),
        ID_HOTKEY_CAPTURE => {
            let capturing = CAPTURING.with(|c| !c.get());
            CAPTURING.with(|c| c.set(capturing));
            if capturing {
                set_text(
                    GetDlgItem(Some(hwnd), ID_HOTKEY_CAPTURE).unwrap_or_default(),
                    "请按下新的组合键（Esc 取消）",
                );
                let _ = SetFocus(Some(hwnd));
            } else {
                refresh_controls(hwnd);
            }
        }
        _ => {}
    }
}

unsafe fn on_key(hwnd: HWND, vk: u32) -> LRESULT {
    let capturing = CAPTURING.with(|c| c.get());
    if !capturing {
        return LRESULT(0);
    }
    if vk == 0x1B {
        // Esc 取消捕获。
        CAPTURING.with(|c| c.set(false));
        refresh_controls(hwnd);
        return LRESULT(1);
    }
    let mods = (((GetKeyState(VK_MENU.0 as i32) as u16) & 0x8000 != 0) as u32 * control::MOD_ALT)
        | (((GetKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0) as u32 * control::MOD_CONTROL)
        | (((GetKeyState(VK_SHIFT.0 as i32) as u16) & 0x8000 != 0) as u32 * control::MOD_SHIFT);
    match control::validate_hotkey(vk, mods) {
        Ok(()) => {
            CAPTURING.with(|c| c.set(false));
            dispatch_native(
                control::next_seq(),
                control::UiAction::SetHotkey { vk, mods },
            );
        }
        Err(reason) => {
            set_text(
                GetDlgItem(Some(hwnd), ID_MODEL_STATUS).unwrap_or_default(),
                &format!("快捷键无效：{reason}"),
            );
        }
    }
    LRESULT(1)
}

unsafe extern "system" fn native_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            on_command(hwnd, id);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => on_key(hwnd, wparam.0 as u32),
        WM_CLOSE => {
            // 托盘常驻应用：关闭=隐藏。
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_DESTROY => {
            on_fallback_destroy(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
