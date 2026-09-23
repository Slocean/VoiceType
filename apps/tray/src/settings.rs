//! 主界面：设置 + 模型 + 更新公告的原生 Win32 主窗口。
//!
//! 顶部 Tab 分两页：「功能」（注入模式 / 模型 / 自启）与「更新·关于」。
//! 窗口不设 owner（带 WS_EX_APPWINDOW），任务栏有独立入口；WM_CLOSE 隐藏复用。
//! 与托盘共用 wndproc：WM_COMMAND / WM_NOTIFY 在此处理。

use super::*;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Graphics::Gdi::{GetStockObject, GetSysColorBrush, DEFAULT_GUI_FONT, HGDIOBJ, SYS_COLOR_INDEX};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyNameTextW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_DWORD, REG_SZ, REG_VALUE_TYPE,
};

// ---------- 控件 ID ----------

const IDC_MODE_PASTE: i32 = 2001;
const IDC_MODE_TYPE: i32 = 2002;
const IDC_MODE_HYBRID: i32 = 2003;
const IDC_MODEL_STATUS: i32 = 2010;
const IDC_VER: i32 = 2012;
const IDC_BTN_UPDATE: i32 = 2013;
const IDC_BTN_ANNOUNCE: i32 = 2014;
const IDC_ANNOUNCE_EDIT: i32 = 2015;
const IDC_AUTOSTART: i32 = 2016;
const IDC_HINT: i32 = 2017;
const IDC_ABOUT: i32 = 2019;
const IDC_GROUP_MODE: i32 = 2020;
const IDC_GROUP_MODEL: i32 = 2021;
const IDC_GROUP_UPDATE: i32 = 2022;
const IDC_TAB: i32 = 2030;
const IDC_HOTKEY: i32 = 2040;
const IDC_BTN_APPLY_HOTKEY: i32 = 2041;
const IDC_HOTKEY_NOW: i32 = 2042;
const IDC_GROUP_HOTKEY: i32 = 2043;

// 原始样式/消息常量（windows crate 已启用 feature 下未导出）。
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
const TCM_INSERTITEMW: u32 = 0x133E;
const TCM_GETCURSEL: u32 = 0x130B;
const TCIF_TEXT: u32 = 1;
const TCN_SELCHANGE: i32 = -551;
const HKM_SETHOTKEY: u32 = 0x0401;
const HKM_GETHOTKEY: u32 = 0x0402;
// 热键控件的修饰键标志（HOTKEYF_*，与 MOD_* 编码不同需转换）。
const HOTKEYF_SHIFT: u32 = 0x1;
const HOTKEYF_CONTROL: u32 = 0x2;
const HOTKEYF_ALT: u32 = 0x4;

/// 两页各自包含的控件（切 tab 时整组显隐）。
const PAGE_FUNCTION: &[i32] = &[
    IDC_GROUP_MODE, IDC_MODE_PASTE, IDC_MODE_TYPE, IDC_MODE_HYBRID,
    IDC_GROUP_MODEL, IDC_MODEL_STATUS,
    IDC_GROUP_HOTKEY, IDC_HOTKEY, IDC_BTN_APPLY_HOTKEY, IDC_HOTKEY_NOW,
    IDC_AUTOSTART, IDC_HINT,
];
const PAGE_UPDATE: &[i32] = &[
    IDC_GROUP_UPDATE, IDC_VER, IDC_BTN_UPDATE, IDC_BTN_ANNOUNCE,
    IDC_ANNOUNCE_EDIT, IDC_ABOUT,
];

const SETTINGS_CLASS: PCWSTR = w!("VoiceTypeSettings");
const CLIENT_W: i32 = 440;
const CLIENT_H: i32 = 450;

const AUTOSTART_VALUE: PCWSTR = w!("VoiceType");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const APP_KEY: PCWSTR = w!("Software\\VoiceType");
const HOTKEY_VALUE: PCWSTR = w!("Hotkey");
const DEFAULT_HOTKEY_VK: u32 = 0x78;

fn legacy_startup_shortcut() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup\VoiceType.lnk"),
    )
}

/// WM_NOTIFY 的 NMHDR 头（windows crate 该类型门控在未启用 feature 下，按内存布局读取）。
#[repr(C)]
struct NotifyHeader {
    hwnd_from: HWND,
    id_from: usize,
    code: i32,
}

/// TCM_INSERTITEMW 的 TCITEMW。
#[repr(C)]
struct TabItemW {
    mask: u32,
    reserved1: u32,
    reserved2: u32,
    psz_text: *mut u16,
    cch_text_max: i32,
    i_image: i32,
    l_param: isize,
}

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
            return legacy_startup_shortcut().is_some_and(|path| path.is_file());
        }
        let ok = RegQueryValueExW(hkey, AUTOSTART_VALUE, None, None, None, None).0 == 0;
        let _ = RegCloseKey(hkey);
        ok || legacy_startup_shortcut().is_some_and(|path| path.is_file())
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
            let status = RegDeleteValueW(hkey, AUTOSTART_VALUE);
            if status.0 == 2 { Ok(()) } else { werr(status) }
        };
        let _ = RegCloseKey(hkey);
        result?;
        if let Some(path) = legacy_startup_shortcut() {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("移除旧版开机启动快捷方式失败: {e}"))?;
            }
        }
        Ok(())
    }
}

// ---------- 录音热键：持久化与展示 ----------

/// 读取用户自定义热键；未设置时默认 F9 单键。存储格式：vk | (mod << 8)。
pub fn load_hotkey() -> (u32, u32) {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, APP_KEY, None, KEY_QUERY_VALUE, &mut hkey).0 != 0 {
            return (DEFAULT_HOTKEY_VK, 0);
        }
        let mut buf = [0u8; 4];
        let mut size = buf.len() as u32;
        let mut ty = REG_VALUE_TYPE::default();
        let ok = RegQueryValueExW(
            hkey,
            HOTKEY_VALUE,
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
        .0
            == 0
            && size == 4;
        let _ = RegCloseKey(hkey);
        if !ok {
            return (DEFAULT_HOTKEY_VK, 0);
        }
        let raw = u32::from_le_bytes(buf);
        (raw & 0xFF, (raw >> 8) & 0xFF)
    }
}

fn save_hotkey(vk: u32, mods: u32) -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        werr(RegCreateKeyW(HKEY_CURRENT_USER, APP_KEY, &mut hkey))?;
        let bytes = (vk | (mods << 8)).to_le_bytes();
        let result = werr(RegSetValueExW(hkey, HOTKEY_VALUE, None, REG_DWORD, Some(&bytes)));
        let _ = RegCloseKey(hkey);
        result
    }
}

/// 热键控件 HOTKEYF_* 修饰键 → RegisterHotKey 的 MOD_*。
fn hotkeyf_to_mod(f: u32) -> u32 {
    let mut m = 0;
    if f & HOTKEYF_CONTROL != 0 { m |= 0x2; } // MOD_CONTROL
    if f & HOTKEYF_SHIFT != 0 { m |= 0x4; } // MOD_SHIFT
    if f & HOTKEYF_ALT != 0 { m |= 0x1; } // MOD_ALT
    m
}

fn mod_to_hotkeyf(m: u32) -> u32 {
    let mut f = 0;
    if m & 0x2 != 0 { f |= HOTKEYF_CONTROL; }
    if m & 0x4 != 0 { f |= HOTKEYF_SHIFT; }
    if m & 0x1 != 0 { f |= HOTKEYF_ALT; }
    f
}

/// 虚拟键码的本地化键名（如 "F9"）。
fn key_name(vk: u32) -> String {
    unsafe {
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let mut buf = [0u16; 32];
        let n = GetKeyNameTextW((sc << 16) as i32, &mut buf);
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            format!("Key 0x{vk:02X}")
        }
    }
}

/// 组合键显示文本（如 "Ctrl + Shift + F9"）。
pub fn combo_text(vk: u32, mods: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if mods & 0x2 != 0 { parts.push("Ctrl".into()); }
    if mods & 0x4 != 0 { parts.push("Shift".into()); }
    if mods & 0x1 != 0 { parts.push("Alt".into()); }
    parts.push(key_name(vk));
    parts.join(" + ")
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
pub unsafe fn show_settings(_main_hwnd: HWND) {
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
        // COLOR_BTNFACE（15）：控件对话框标准底色，缺省会黑底。
        hbrBackground: GetSysColorBrush(SYS_COLOR_INDEX(15)),
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
        // WS_EX_APPWINDOW + 不设 owner：任务栏有独立入口。
        WINDOW_EX_STYLE(WS_EX_APPWINDOW.0),
        SETTINGS_CLASS,
        w!("VoiceType"),
        style,
        (screen_w - w) / 2,
        (screen_h - h) / 2,
        w,
        h,
        None,
        None,
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    if hwnd.is_invalid() {
        return;
    }

    let font = GetStockObject(DEFAULT_GUI_FONT);
    let group = |text: &str, x, y, w, h, id| {
        add_control(hwnd, w!("BUTTON"), text, BS_GROUPBOX_RAW, x, y, w, h, id, font);
    };

    // Tab：功能 / 更新·关于
    add_control(hwnd, w!("SysTabControl32"), "", 0, 10, 10, CLIENT_W - 20, 28, IDC_TAB, font);
    if let Some(tab) = get_ctrl(hwnd, IDC_TAB) {
        for (i, name) in ["功能", "更新·关于"].iter().enumerate() {
            let mut text = wide(name);
            let mut item = TabItemW {
                mask: TCIF_TEXT,
                reserved1: 0,
                reserved2: 0,
                psz_text: text.as_mut_ptr(),
                cch_text_max: (text.len() - 1) as i32,
                i_image: 0,
                l_param: 0,
            };
            SendMessageW(
                tab,
                TCM_INSERTITEMW,
                Some(WPARAM(i)),
                Some(LPARAM(&mut item as *mut TabItemW as isize)),
            );
        }
    }

    // ---- 第 1 页：功能 ----
    group("注入模式（F9 定稿后如何上屏）", 10, 48, CLIENT_W - 20, 104, IDC_GROUP_MODE);
    add_control(hwnd, w!("BUTTON"), "粘贴模式", BS_AUTORADIOBUTTON_RAW, 30, 72, 180, 22, IDC_MODE_PASTE, font);
    add_control(hwnd, w!("BUTTON"), "键入模式", BS_AUTORADIOBUTTON_RAW, 30, 96, 180, 22, IDC_MODE_TYPE, font);
    add_control(hwnd, w!("BUTTON"), "混合模式（边说边出字）", BS_AUTORADIOBUTTON_RAW, 30, 120, 240, 22, IDC_MODE_HYBRID, font);

    group("语音模型（已内置，随 exe 分发）", 10, 160, CLIENT_W - 20, 104, IDC_GROUP_MODEL);
    add_control(hwnd, w!("STATIC"), "", 0, 30, 190, 380, 22, IDC_MODEL_STATUS, font);
    group("录音快捷键（点击框内按下组合键，再点应用）", 10, 272, CLIENT_W - 20, 76, IDC_GROUP_HOTKEY);
    add_control(hwnd, w!("msctls_hotkey32"), "", 0, 30, 298, 200, 24, IDC_HOTKEY, font);
    add_control(hwnd, w!("BUTTON"), "应用", 0, 240, 296, 90, 26, IDC_BTN_APPLY_HOTKEY, font);
    add_control(hwnd, w!("STATIC"), "", 0, 30, 326, 360, 20, IDC_HOTKEY_NOW, font);
    add_control(hwnd, w!("BUTTON"), "开机自动启动（托盘驻留）", BS_AUTOCHECKBOX_RAW, 30, 356, 260, 22, IDC_AUTOSTART, font);
    add_control(hwnd, w!("STATIC"), "说完停顿自动结束并注入；托盘右键有更多操作", 0, 30, 386, 380, 22, IDC_HINT, font);

    // ---- 第 2 页：更新·关于 ----
    group("更新与公告", 10, 48, CLIENT_W - 20, 300, IDC_GROUP_UPDATE);
    add_control(hwnd, w!("STATIC"), "", 0, 30, 76, 200, 22, IDC_VER, font);
    add_control(hwnd, w!("BUTTON"), "检查更新", 0, 240, 72, 90, 28, IDC_BTN_UPDATE, font);
    add_control(hwnd, w!("BUTTON"), "更新公告", 0, 336, 72, 80, 28, IDC_BTN_ANNOUNCE, font);
    add_control(
        hwnd,
        w!("EDIT"),
        "",
        ES_MULTILINE_RAW | ES_AUTOVSCROLL_RAW | ES_READONLY_RAW | WS_VSCROLL.0,
        25, 108, CLIENT_W - 50, 190, IDC_ANNOUNCE_EDIT, font,
    );
    add_control(
        hwnd,
        w!("STATIC"),
        "VoiceType — 离线中文语音输入\n开源仓库：github.com/Slocean/VoiceType",
        0, 306, 380, 36, 0, IDC_ABOUT, font,
    );

    with_app(|app| app.settings = hwnd);
    set_page(hwnd, 0);
    refresh_all(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

fn set_page(hwnd: HWND, page: usize) {
    let (show, hide) = if page == 0 {
        (PAGE_FUNCTION, PAGE_UPDATE)
    } else {
        (PAGE_UPDATE, PAGE_FUNCTION)
    };
    unsafe {
        for id in show {
            if let Some(c) = get_ctrl(hwnd, *id) {
                let _ = ShowWindow(c, SW_SHOW);
            }
        }
        for id in hide {
            if let Some(c) = get_ctrl(hwnd, *id) {
                let _ = ShowWindow(c, SW_HIDE);
            }
        }
    }
}

// ---------- 状态回写 ----------

fn get_ctrl(hwnd: HWND, id: i32) -> Option<HWND> {
    unsafe { GetDlgItem(Some(hwnd), id).ok() }
}

pub fn is_open() -> bool {
    with_app(|app| !app.settings.is_invalid()).unwrap_or(false)
}

fn model_status_text() -> String {
    if models_dl::models_ready() {
        "模型：已就绪（中英双语流式识别）".into()
    } else {
        "模型：未就绪（内置模型解压失败，请检查磁盘）".into()
    }
}

/// 全量刷新控件状态（打开窗口 / 事件回调后调用）。
pub fn refresh_all(hwnd: HWND) {
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
    if let Some(ctrl) = get_ctrl(hwnd, IDC_MODEL_STATUS) {
        set_window_text(ctrl, &model_status_text());
    }
    if let Some(ctrl) = get_ctrl(hwnd, IDC_VER) {
        set_window_text(ctrl, &format!("当前版本 v{}", updater::APP_VERSION));
    }
    let (vk, mods) = with_app(|app| (app.hotkey_vk, app.hotkey_mods)).unwrap_or((DEFAULT_HOTKEY_VK, 0));
    if let Some(ctrl) = get_ctrl(hwnd, IDC_HOTKEY) {
        unsafe {
            let _ = SendMessageW(
                ctrl,
                HKM_SETHOTKEY,
                Some(WPARAM(((mod_to_hotkeyf(mods) << 8) | vk) as usize)),
                Some(LPARAM(0)),
            );
        }
    }
    if let Some(ctrl) = get_ctrl(hwnd, IDC_HOTKEY_NOW) {
        set_window_text(ctrl, &format!("当前生效：{}", combo_text(vk, mods)));
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
    let visible_update_page = unsafe {
        IsWindowVisible(hwnd).as_bool()
            && get_ctrl(hwnd, IDC_TAB).is_some_and(|tab| {
                SendMessageW(tab, TCM_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1
            })
    };
    if !visible_update_page {
        return false;
    }
    match get_ctrl(hwnd, IDC_ANNOUNCE_EDIT) {
        Some(ctrl) => {
            set_window_text(ctrl, text);
            true
        }
        None => false,
    }
}

pub fn refresh_update_status() {
    if !is_open() {
        return;
    }
    let hwnd = with_app(|app| app.settings).unwrap_or_default();
    if let Some(ctrl) = get_ctrl(hwnd, IDC_VER) {
        set_window_text(ctrl, &format!("当前版本 v{}", updater::APP_VERSION));
    }
}

// ---------- WM_COMMAND / WM_NOTIFY ----------

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
        IDC_BTN_APPLY_HOTKEY => {
            let raw = get_ctrl(hwnd, IDC_HOTKEY)
                .map(|ctrl| SendMessageW(ctrl, HKM_GETHOTKEY, Some(WPARAM(0)), Some(LPARAM(0))).0)
                .unwrap_or(0);
            let vk = (raw & 0xFF) as u32;
            let hotkeyf = ((raw >> 8) & 0xFF) as u32;
            if vk == 0 {
                let text = wide("请先点击左侧输入框，按下想要设置的组合键。");
                let _ = MessageBoxW(
                    Some(hwnd),
                    PCWSTR(text.as_ptr()),
                    w!("VoiceType"),
                    MB_ICONINFORMATION,
                );
                return;
            }
            let mods = hotkeyf_to_mod(hotkeyf);
            if ui_main::apply_new_hotkey(vk, mods) {
                // 成功：持久化 + 刷新显示。
                let _ = save_hotkey(vk, mods);
                if let Some(ctrl) = get_ctrl(hwnd, IDC_HOTKEY_NOW) {
                    set_window_text(ctrl, &format!("当前生效：{}", combo_text(vk, mods)));
                }
            } else {
                // 冲突：提示并回退控件显示为原热键。
                let (ovk, omods) =
                    with_app(|app| (app.hotkey_vk, app.hotkey_mods)).unwrap_or((DEFAULT_HOTKEY_VK, 0));
                if let Some(ctrl) = get_ctrl(hwnd, IDC_HOTKEY) {
                    let _ = SendMessageW(
                        ctrl,
                        HKM_SETHOTKEY,
                        Some(WPARAM(((mod_to_hotkeyf(omods) << 8) | ovk) as usize)),
                        Some(LPARAM(0)),
                    );
                }
                let text = wide(&format!(
                    "快捷键 {} 注册失败：可能已被其他程序占用。

已恢复原快捷键 {}，请换一个组合再试。",
                    combo_text(vk, mods),
                    combo_text(ovk, omods)
                ));
                let _ = MessageBoxW(
                    Some(hwnd),
                    PCWSTR(text.as_ptr()),
                    w!("VoiceType"),
                    MB_ICONWARNING,
                );
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

/// Tab 切换：TCN_SELCHANGE 时整页显隐。
pub unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) {
    let hdr = lparam.0 as *const NotifyHeader;
    if hdr.is_null() {
        return;
    }
    let hdr = std::ptr::read_unaligned(hdr);
    if hdr.code == TCN_SELCHANGE && hdr.id_from == IDC_TAB as usize {
        let page = SendMessageW(hdr.hwnd_from, TCM_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0;
        set_page(hwnd, page as usize);
    }
}
