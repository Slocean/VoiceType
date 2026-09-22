//! 主界面：设置 + 模型 + 更新公告的原生 Win32 主窗口。
//!
//! 纯控件窗口（BUTTON/STATIC/EDIT），与托盘共用 wndproc：WM_COMMAND 在此处理，
//! WM_CLOSE 隐藏不销毁（下次打开复用）。托盘双击 / 菜单「主界面…」打开。

use super::*;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT, HGDIOBJ};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

// ---------- 控件 ID ----------

const IDC_MODE_PASTE: i32 = 2001;
const IDC_MODE_TYPE: i32 = 2002;
const IDC_MODE_HYBRID: i32 = 2003;
const IDC_MODEL_STATUS: i32 = 2010;
const IDC_BTN_DOWNLOAD: i32 = 2011;
const IDC_VER: i32 = 2012;
const IDC_BTN_UPDATE: i32 = 2013;
const IDC_BTN_ANNOUNCE: i32 = 2014;
const IDC_ANNOUNCE_EDIT: i32 = 2015;
const IDC_AUTOSTART: i32 = 2016;
const IDC_HINT: i32 = 2017;

// 原始样式常量（windows crate 未在已启用 feature 下导出这些按钮/编辑框样式）。
const BS_GROUPBOX_RAW: u32 = 0x7;
const BS_AUTOCHECKBOX_RAW: u32 = 0x3;
const BS_AUTORADIOBUTTON_RAW: u32 = 0x9;
const ES_MULTILINE_RAW: u32 = 0x4;
const ES_AUTOVSCROLL_RAW: u32 = 0x40;
const ES_READONLY_RAW: u32 = 0x800;
const BM_SETCHECK: u32 = 0xF1;
const BM_GETCHECK: u32 = 0xF2;
const BST_CHECKED: WPARAM = WPARAM(1);
const WM_SETFONT: u32 = 0x30;

const SETTINGS_CLASS: PCWSTR = w!("VoiceTypeSettings");
const CLIENT_W: i32 = 440;
const CLIENT_H: i32 = 520;

const AUTOSTART_VALUE: PCWSTR = w!("VoiceType");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

// ---------- 开机自启（HKCU\...\Run，按用户级别，与安装版权限一致） ----------

fn werr(r: WIN32_ERROR) -> Result<(), String> {
    if r.0 == 0 {
        Ok(())
    } else {
        Err(format!("注册表操作失败（错误码 {}）", r.0))
    }
}

fn autostart_enabled() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_QUERY_VALUE, &mut hkey).0 != 0 {
            return false;
        }
        let ok = RegQueryValueExW(hkey, AUTOSTART_VALUE, None, None, None, None).0 == 0;
        let _ = RegCloseKey(hkey);
        ok
    }
}

fn set_autostart(enable: bool) -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        werr(RegCreateKeyW(HKEY_CURRENT_USER, RUN_KEY, &mut hkey))?;
        let result = if enable {
            match std::env::current_exe() {
                Ok(exe) => {
                    let cmd = wide(&format!("\"{}\"", exe.display()));
                    let bytes = std::slice::from_raw_parts(cmd.as_ptr().cast::<u8>(), cmd.len() * 2);
                    werr(RegSetValueExW(hkey, AUTOSTART_VALUE, None, REG_SZ, Some(bytes)))
                }
                Err(e) => Err(format!("定位 exe 失败: {e}")),
            }
        } else {
            werr(RegDeleteValueW(hkey, AUTOSTART_VALUE))
        };
        let _ = RegCloseKey(hkey);
        result
    }
}

// ---------- 窗口与控件 ----------

unsafe fn add_control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    font: HGDIOBJ,
) -> HWND {
    let t = wide(text);
    let ctrl = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(t.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as isize as *mut _)),
        Some(GetModuleHandleW(None).unwrap_or_default().into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(ctrl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    ctrl
}

/// 打开（或前置）主窗口。窗口只建一次，WM_CLOSE 时隐藏复用。
pub unsafe fn show_settings(main_hwnd: HWND) {
    let existing = with_app(|app| app.settings).unwrap_or_default();
    if !existing.is_invalid() {
        let _ = ShowWindow(existing, SW_SHOW);
        let _ = SetForegroundWindow(existing);
        refresh_all(existing);
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
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        SETTINGS_CLASS,
        w!("VoiceType"),
        style,
        (screen_w - w) / 2,
        (screen_h - h) / 2,
        w,
        h,
        Some(main_hwnd),
        None,
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    if hwnd.is_invalid() {
        return;
    }

    let font = GetStockObject(DEFAULT_GUI_FONT);
    let group = |text: &str, x, y, w, h| {
        add_control(hwnd, w!("BUTTON"), text, BS_GROUPBOX_RAW, x, y, w, h, -1, font);
    };
    let radio_flags = BS_AUTORADIOBUTTON_RAW;

    // 注入模式
    group("注入模式（F9 定稿后如何上屏）", 10, 10, CLIENT_W - 20, 104);
    add_control(hwnd, w!("BUTTON"), "粘贴模式", radio_flags, 30, 34, 180, 22, IDC_MODE_PASTE, font);
    add_control(hwnd, w!("BUTTON"), "键入模式", radio_flags, 30, 58, 180, 22, IDC_MODE_TYPE, font);
    add_control(hwnd, w!("BUTTON"), "混合模式（边说边出字）", radio_flags, 30, 82, 220, 22, IDC_MODE_HYBRID, font);

    // 语音模型
    group("语音模型", 10, 122, CLIENT_W - 20, 78);
    add_control(hwnd, w!("STATIC"), "", 0, 30, 148, 240, 22, IDC_MODEL_STATUS, font);
    add_control(hwnd, w!("BUTTON"), "下载语音模型", 0, 290, 144, 130, 28, IDC_BTN_DOWNLOAD, font);

    // 更新与公告
    group("更新与公告", 10, 208, CLIENT_W - 20, 246);
    add_control(hwnd, w!("STATIC"), "", 0, 30, 234, 200, 22, IDC_VER, font);
    add_control(hwnd, w!("BUTTON"), "检查更新", 0, 240, 230, 90, 28, IDC_BTN_UPDATE, font);
    add_control(hwnd, w!("BUTTON"), "更新公告", 0, 336, 230, 90, 28, IDC_BTN_ANNOUNCE, font);
    add_control(
        hwnd,
        w!("EDIT"),
        "",
        ES_MULTILINE_RAW | ES_AUTOVSCROLL_RAW | ES_READONLY_RAW | WS_VSCROLL.0,
        25,
        264,
        CLIENT_W - 50,
        180,
        IDC_ANNOUNCE_EDIT,
        font,
    );

    // 自启与提示
    add_control(hwnd, w!("BUTTON"), "开机自动启动（托盘驻留）", BS_AUTOCHECKBOX_RAW, 30, 462, 240, 22, IDC_AUTOSTART, font);
    add_control(hwnd, w!("STATIC"), "F9 开始 / 结束录音；托盘右键有更多操作", 0, 30, 490, 380, 22, IDC_HINT, font);

    with_app(|app| app.settings = hwnd);
    refresh_all(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

// ---------- 状态回写 ----------

fn get_ctrl(hwnd: HWND, id: i32) -> Option<HWND> {
    unsafe { GetDlgItem(Some(hwnd), id).ok() }
}

pub fn is_open() -> bool {
    with_app(|app| !app.settings.is_invalid()).unwrap_or(false)
}

/// 全量刷新控件状态（打开窗口 / 事件回调后调用）。
pub fn refresh_all(hwnd: HWND) {
    // 注入模式单选
    let mode = with_app(|app| app.mode).unwrap_or(InjectMode::Hybrid);
    let checks = [
        (IDC_MODE_PASTE, mode == InjectMode::Paste),
        (IDC_MODE_TYPE, mode == InjectMode::Type),
        (IDC_MODE_HYBRID, mode == InjectMode::Hybrid),
    ];
    for (id, checked) in checks {
        if let Some(ctrl) = get_ctrl(hwnd, id) {
            unsafe {
                let _ = SendMessageW(
                    ctrl,
                    BM_SETCHECK,
                    Some(if checked { BST_CHECKED } else { WPARAM(0) }),
                    Some(LPARAM(0)),
                );
            }
        }
    }
    // 模型状态
    let model_text = if models_dl::models_ready() {
        "模型：已就绪"
    } else {
        "模型：未下载（约 15MB）"
    };
    if let Some(ctrl) = get_ctrl(hwnd, IDC_MODEL_STATUS) {
        set_window_text(ctrl, model_text);
    }
    // 版本与自启
    if let Some(ctrl) = get_ctrl(hwnd, IDC_VER) {
        set_window_text(ctrl, &format!("当前版本 v{}", updater::APP_VERSION));
    }
    if let Some(ctrl) = get_ctrl(hwnd, IDC_AUTOSTART) {
        unsafe {
            let _ = SendMessageW(
                ctrl,
                BM_SETCHECK,
                Some(if autostart_enabled() { BST_CHECKED } else { WPARAM(0) }),
                Some(LPARAM(0)),
            );
        }
    }
}

/// 公告文本写进主窗口的公告区（窗口未开时由调用方回退 MessageBox）。
pub fn fill_announcement(text: &str) -> bool {
    if !is_open() {
        return false;
    }
    let hwnd = with_app(|app| app.settings).unwrap_or_default();
    match get_ctrl(hwnd, IDC_ANNOUNCE_EDIT) {
        Some(ctrl) => {
            set_window_text(ctrl, text);
            true
        }
        None => false,
    }
}

pub fn refresh_model_status() {
    if !is_open() {
        return;
    }
    let hwnd = with_app(|app| app.settings).unwrap_or_default();
    if let Some(ctrl) = get_ctrl(hwnd, IDC_MODEL_STATUS) {
        set_window_text(
            ctrl,
            if models_dl::models_ready() {
                "模型：已就绪"
            } else {
                "模型：未下载（约 15MB）"
            },
        );
    }
}

// ---------- WM_COMMAND ----------

pub unsafe fn on_command(hwnd: HWND, id: i32) {
    match id {
        IDC_MODE_PASTE | IDC_MODE_TYPE | IDC_MODE_HYBRID => {
            let mode = match id {
                IDC_MODE_PASTE => InjectMode::Paste,
                IDC_MODE_TYPE => InjectMode::Type,
                _ => InjectMode::Hybrid,
            };
            with_app(|app| app.mode = mode);
        }
        IDC_BTN_DOWNLOAD => {
            if models_dl::models_ready() {
                refresh_model_status();
            } else {
                if let Some(ctrl) = get_ctrl(hwnd, IDC_MODEL_STATUS) {
                    set_window_text(ctrl, "正在下载语音模型…");
                }
                models_dl::spawn_model_download(hwnd.0 as isize);
            }
        }
        IDC_BTN_UPDATE => {
            if let Some(ctrl) = get_ctrl(hwnd, IDC_VER) {
                set_window_text(ctrl, "正在检查更新…");
            }
            updater::spawn_check_and_download(hwnd.0 as isize);
        }
        IDC_BTN_ANNOUNCE => {
            updater::spawn_fetch_announcement(hwnd.0 as isize);
        }
        IDC_AUTOSTART => {
            let checked = get_ctrl(hwnd, IDC_AUTOSTART)
                .map(|ctrl| {
                    unsafe {
                        SendMessageW(ctrl, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0
                            == BST_CHECKED.0 as isize
                    }
                })
                .unwrap_or(false);
            if let Err(e) = set_autostart(checked) {
                let text = wide(&format!("设置开机自启失败：{e}"));
                let _ = MessageBoxW(
                    Some(hwnd),
                    PCWSTR(text.as_ptr()),
                    w!("VoiceType"),
                    MB_ICONWARNING,
                );
            }
        }
        _ => {}
    }
}
