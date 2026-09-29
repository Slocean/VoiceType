//! UI 动作控制层（工作包 B1/B2/B3）。
//!
//! HTML/托盘只发类型化动作并显示结果；模型、录音、热键、更新的决策集中在
//! 这里。每个动作最终恰好交付一次结果（Running 先行、Succeeded/Failed 收尾），
//! 未知动作、无效数值、重复提交都返回明确失败，不静默忽略。
//!
//! 录音目标规则（B2）：热键开始录音时记录当时的外部前台窗口作为注入目标；
//! VoiceType 自身窗口、已销毁窗口不是有效目标。混合模式中间结果与定稿提交前
//! 确认目标仍是同一窗口；焦点移走后不得向新窗口键入、粘贴或退格——识别文本
//! 复制到剪贴板，由用户明确操作。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow};

use super::settings::{combo_text, SettingsStore};
use super::*;

// ---------- 类型化动作与结果 ----------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiAction {
    SetMode(InjectMode),
    SetHotkey {
        vk: u32,
        mods: u32,
    },
    SetAutostart(bool),
    /// 首页按钮：录音中=结束；空闲=隐藏窗口并提示热键，绝不直接启动录音。
    RequestRecord,
    CheckUpdate,
    FetchAnnouncement,
    RepairModels,
}

impl UiAction {
    pub fn key(self) -> &'static str {
        match self {
            UiAction::SetMode(_) => "mode",
            UiAction::SetHotkey { .. } => "hotkey",
            UiAction::SetAutostart(_) => "autostart",
            UiAction::RequestRecord => "record",
            UiAction::CheckUpdate => "checkUpdate",
            UiAction::FetchAnnouncement => "announcement",
            UiAction::RepairModels => "repairModels",
        }
    }
}

/// 异步动作的进行中去重键集合；同类动作进行中重复提交明确失败。
fn action_is_async(action: UiAction) -> bool {
    matches!(
        action,
        UiAction::CheckUpdate | UiAction::FetchAnnouncement | UiAction::RepairModels
    )
}

/// 控制层入口（主线程）。每个动作最终交付一次结果。
pub fn dispatch(seq: u64, action: UiAction) {
    let key = action.key();
    if action_is_async(action) {
        let busy = with_app(|app| app.pending.values().any(|a| a == key)).unwrap_or(false);
        if busy {
            settings_web::deliver_done(seq, key, false, "已有相同操作在进行中，请稍候");
            return;
        }
    }
    match action {
        UiAction::SetMode(mode) => {
            with_app(|app| app.mode = mode);
            settings_web::deliver_done(seq, key, true, "输入方式已更新");
        }
        UiAction::SetHotkey { vk, mods } => apply_hotkey_via_app(seq, vk, mods),
        UiAction::SetAutostart(enable) => apply_autostart(seq, enable),
        UiAction::RequestRecord => request_record(seq),
        UiAction::CheckUpdate => {
            if updater::is_checking() {
                settings_web::deliver_done(seq, key, false, "正在检查更新，请稍候");
                return;
            }
            mark_pending(seq, key);
            settings_web::deliver_running(seq, key, "正在检查更新…");
            let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
            if hwnd.is_invalid() {
                take_pending(key);
                settings_web::deliver_done(seq, key, false, "主窗口未就绪，无法检查更新");
                return;
            }
            updater::spawn_check_and_download(hwnd.0 as isize);
        }
        UiAction::FetchAnnouncement => {
            mark_pending(seq, key);
            settings_web::deliver_running(seq, key, "正在获取更新公告…");
            let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
            if hwnd.is_invalid() {
                take_pending(key);
                settings_web::deliver_done(seq, key, false, "主窗口未就绪，无法获取公告");
                return;
            }
            updater::spawn_fetch_announcement(hwnd.0 as isize);
        }
        UiAction::RepairModels => {
            let Some(manager) = model_manager::global() else {
                settings_web::deliver_done(seq, key, false, "模型模块未初始化");
                return;
            };
            if manager.is_busy() {
                settings_web::deliver_done(seq, key, false, "模型检查或修复已在进行中，请稍候");
                return;
            }
            mark_pending(seq, key);
            settings_web::deliver_running(seq, key, "模型检查与修复已在后台开始");
            manager.start_check_and_repair();
        }
    }
}

static NEXT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);

/// 原生备用窗口发起动作时取动作编号（与 HTML 的编号空间隔离）。
pub fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

fn mark_pending(seq: u64, key: &str) {
    with_app(|app| {
        app.pending.insert(seq, key.to_string());
    });
}

/// 取走指定动作的待完成编号（完成交付时调用）。
pub fn take_pending(key: &str) -> Option<u64> {
    with_app(|app| {
        let seq = app
            .pending
            .iter()
            .find(|(_, a)| a.as_str() == key)
            .map(|(s, _)| *s);
        if let Some(seq) = seq {
            app.pending.remove(&seq);
        }
        seq
    })
    .flatten()
}

// ---------- 各动作实现 ----------

fn apply_hotkey_via_app(seq: u64, vk: u32, mods: u32) {
    let outcome = with_app(|app| {
        let registrar = GlobalHotkeyRegistrar(app.main_hwnd.0 as isize);
        let current = (app.hotkey_vk, app.hotkey_mods);
        let result = apply_hotkey_transaction(&registrar, app.store.as_ref(), current, (vk, mods));
        if result.is_ok() {
            // 事务全部成功后才提交 UI 状态。
            app.hotkey_vk = vk;
            app.hotkey_mods = mods;
            app.hotkey_name = combo_text(vk, mods);
        }
        result
    });
    match outcome {
        None => settings_web::deliver_done(seq, "hotkey", false, "应用未初始化"),
        Some(Ok(())) => {
            // 托盘提示跟随新热键。
            if let Some(hwnd) = with_app(|app| app.main_hwnd) {
                if !hwnd.is_invalid() {
                    ui_main::update_tray_tip(hwnd);
                }
            }
            settings_web::deliver_done(seq, "hotkey", true, "快捷键已应用");
        }
        Some(Err(err)) => {
            diagnostics::record("ERROR", "hotkey", &err.to_string());
            settings_web::deliver_done(seq, "hotkey", false, &err.to_string());
        }
    }
}

fn apply_autostart(seq: u64, enable: bool) {
    let store = with_app(|app| app.store.clone());
    let Some(store) = store else {
        settings_web::deliver_done(seq, "autostart", false, "设置存储未初始化");
        return;
    };
    let result = store.set_autostart(enable);
    // 失败后重新读取实际状态并展示，不凭写操作结果断言。
    let actual = store.autostart_enabled();
    let actual_text = if actual { "已开启" } else { "已关闭" };
    match result {
        Ok(()) if actual == enable => settings_web::deliver_done(
            seq,
            "autostart",
            true,
            if enable {
                "开机自启已开启"
            } else {
                "开机自启已关闭"
            },
        ),
        Ok(()) => settings_web::deliver_done(
            seq,
            "autostart",
            false,
            &format!("设置未生效，当前实际状态：{actual_text}"),
        ),
        Err(e) => settings_web::deliver_done(
            seq,
            "autostart",
            false,
            &format!("{e}（当前实际状态：{actual_text}）"),
        ),
    }
}

fn request_record(seq: u64) {
    let recording = with_app(|app| app.recording).unwrap_or(false);
    if recording {
        // 结束录音是安全的：定稿走目标规则，不会注入到 VoiceType 自己的窗口。
        let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
        if !hwnd.is_invalid() {
            ui_main::toggle_recording(hwnd);
        }
        settings_web::deliver_done(seq, "record", true, "正在结束录音并生成定稿…");
        return;
    }
    // B2：不直接启动录音。隐藏设置窗口并提示热键，避免在自身窗口获焦时注入。
    let hotkey = with_app(|app| app.hotkey_name.clone()).unwrap_or_else(|| "F9".into());
    settings_web::hide_settings();
    ui_main::set_overlay_message(&format!("🎤 回到目标应用后按 {hotkey} 开始说话"));
    settings_web::deliver_done(
        seq,
        "record",
        true,
        "已隐藏窗口：回到目标应用，按快捷键开始",
    );
}

// ---------- 录音目标规则（B2） ----------

/// 录音开始时捕获外部前台窗口作为注入目标。
/// 必须在 with_app 可变借用之外调用（内部需要读取 App）。
/// VoiceType 自身窗口、已销毁窗口都不是有效目标。
pub fn capture_record_target() -> Option<isize> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || !IsWindow(Some(fg)).as_bool() {
            return None;
        }
        let own = with_app(|app| fg == app.main_hwnd || fg == app.settings || fg == app.overlay)
            // App 不存在时按自身窗口处理，宁可不注入。
            .unwrap_or(true);
        (!own).then_some(fg.0 as isize)
    }
}

/// 定稿/中间结果提交前确认目标仍是同一个可用窗口。
pub fn target_ready(expected: isize) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        !fg.is_invalid() && IsWindow(Some(fg)).as_bool() && fg.0 as isize == expected
    }
}

/// 目标失效时的文本出路：复制到剪贴板，绝不向新窗口注入。
pub fn deliver_text_without_target(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    diagnostics::record("INFO", "injection", "目标窗口已切换，识别文本复制到剪贴板");
    voicetype_inject::copy_to_clipboard(text).map_err(|e| e.to_string())
}

// ---------- 热键事务（B3） ----------

pub trait HotkeyRegistrar {
    fn register(&self, vk: u32, mods: u32) -> Result<(), String>;
    fn unregister(&self) -> Result<(), String>;
}

#[derive(Debug)]
pub enum HotkeyTxError {
    Invalid(String),
    Conflict { old_recovered: bool },
    SaveFailed { old_recovered: bool, error: String },
}

impl std::fmt::Display for HotkeyTxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HotkeyTxError::Invalid(msg) => write!(f, "无效快捷键：{msg}"),
            HotkeyTxError::Conflict { old_recovered } => {
                if *old_recovered {
                    write!(f, "快捷键被其他应用占用（已恢复原快捷键）")
                } else {
                    write!(
                        f,
                        "快捷键被占用，且原快捷键恢复失败：请立即重启应用以恢复录音热键"
                    )
                }
            }
            HotkeyTxError::SaveFailed {
                old_recovered,
                error,
            } => {
                if *old_recovered {
                    write!(f, "保存快捷键失败：{error}（已恢复原快捷键）")
                } else {
                    write!(
                        f,
                        "保存快捷键失败：{error}，且原快捷键恢复失败：请立即重启应用以恢复录音热键"
                    )
                }
            }
        }
    }
}

/// 修饰键协议：0x1 Alt / 0x2 Ctrl / 0x4 Shift（与设置页 JS、combo_text 一致）。
pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;

/// 校验热键组合。规则：
/// - vk 必须在 0x08..=0xFE；修饰键只允许 Alt/Ctrl/Shift；
/// - 拒绝鼠标键、纯修饰键、Win 键、保留区；
/// - 拒绝系统保留组合（Alt+Tab、Ctrl+Esc、Ctrl+Shift+Esc、Alt+Esc、Alt+Space）；
/// - 可打印键（字母/数字/标点/空格/回车/退格/Tab）必须带修饰键，防止劫持输入；
///   功能键与方向/编辑键允许单键（默认 F9 即此类）；
/// - Ctrl+Alt 组合（AltGr）允许使用，界面说明里注明可能与部分欧洲键盘布局冲突。
pub fn validate_hotkey(vk: u32, mods: u32) -> Result<(), String> {
    if !(0x08..=0xFE).contains(&vk) {
        return Err(format!("无效的键码 0x{vk:02X}"));
    }
    if mods & !(MOD_ALT | MOD_CONTROL | MOD_SHIFT) != 0 {
        return Err("修饰键只支持 Ctrl / Shift / Alt".into());
    }
    const REJECTED: &[(u32, u32)] = &[
        (0x01, 0x07), // 鼠标键与未定义
        (0x0A, 0x0F), // 保留
        (0x10, 0x15), // Shift/Ctrl/Alt/Pause/CapsLock/IME Kanji
        (0x16, 0x19), // IME 相关
        (0x1A, 0x1F), // 保留
        (0x5B, 0x5C), // LWin/RWin
        (0xA0, 0xA5), // 左右 Shift/Ctrl/Alt
        (0xE7, 0xF5), // 保留/未分配
    ];
    if in_ranges(vk, REJECTED) {
        return Err(format!("键 0x{vk:02X} 不能用作快捷键"));
    }
    if let Some(reason) = reserved_combo(vk, mods) {
        return Err(reason.into());
    }
    fn in_ranges(vk: u32, ranges: &[(u32, u32)]) -> bool {
        ranges.iter().any(|(lo, hi)| vk >= *lo && vk <= *hi)
    }
    // 可打印键单键会劫持正常输入，必须搭配修饰键；方向/编辑键与功能键允许单键。
    const TYPEABLE: &[(u32, u32)] = &[
        (0x08, 0x09), // Backspace / Tab
        (0x0D, 0x0D), // Enter
        (0x20, 0x20), // Space
        (0x30, 0x39), // 主键盘数字
        (0x41, 0x5A), // 字母
        (0x60, 0x69), // 小键盘数字
        (0xBA, 0xC0), // ;=,-./` 等 OEM
        (0xDB, 0xDE), // [ ] \ '
    ];
    if mods == 0 && in_ranges(vk, TYPEABLE) {
        return Err("该键需要搭配 Ctrl / Shift / Alt 使用（单键会劫持正常输入）".into());
    }
    Ok(())
}

fn reserved_combo(vk: u32, mods: u32) -> Option<&'static str> {
    match (mods, vk) {
        (m, 0x09) if m & MOD_ALT != 0 => Some("Alt+Tab 是系统保留组合"),
        (m, 0x1B) if m & (MOD_CONTROL | MOD_ALT) != 0 => Some("系统保留组合（Ctrl/Alt+Esc）"),
        (m, 0x20) if m & MOD_ALT != 0 => Some("Alt+Space 是系统窗口菜单快捷键"),
        (m, 0x2E) if m & (MOD_CONTROL | MOD_ALT) != 0 => Some("该组合是系统保留快捷键"),
        _ => None,
    }
}

/// 热键变更事务：校验 → 试注册新组合 → 保存注册表 → 全部成功才返回 Ok。
/// 任一步失败恢复旧注册与旧存储状态；旧热键恢复失败在错误中标明。
/// 内存状态与显示的提交由调用方在 Ok 后完成。
pub fn apply_hotkey_transaction(
    reg: &dyn HotkeyRegistrar,
    store: &dyn SettingsStore,
    current: (u32, u32),
    next: (u32, u32),
) -> Result<(), HotkeyTxError> {
    validate_hotkey(next.0, next.1).map_err(HotkeyTxError::Invalid)?;
    if next == current {
        return Ok(());
    }
    let _ = reg.unregister();
    if reg.register(next.0, next.1).is_err() {
        let old_recovered = reg.register(current.0, current.1).is_ok();
        return Err(HotkeyTxError::Conflict { old_recovered });
    }
    if let Err(error) = store.save_hotkey(next.0, next.1) {
        let old_recovered = reg.register(current.0, current.1).is_ok();
        return Err(HotkeyTxError::SaveFailed {
            old_recovered,
            error,
        });
    }
    Ok(())
}

/// 真实注册器：绑定主窗口 HOTKEY_ID 的全局热键。
pub struct GlobalHotkeyRegistrar(pub isize);

impl HotkeyRegistrar for GlobalHotkeyRegistrar {
    fn register(&self, vk: u32, mods: u32) -> Result<(), String> {
        unsafe {
            let hwnd = HWND(self.0 as *mut _);
            RegisterHotKey(
                Some(hwnd),
                super::HOTKEY_ID,
                HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0),
                vk,
            )
            .map_err(|e| format!("注册失败（可能被其他应用占用）：{e}"))
        }
    }

    fn unregister(&self) -> Result<(), String> {
        unsafe {
            UnregisterHotKey(Some(HWND(self.0 as *mut _)), super::HOTKEY_ID)
                .map_err(|e| e.to_string())
        }
    }
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeRegistrar {
        /// 注册会失败组合（模拟被占用）。
        fail_combos: Mutex<Vec<(u32, u32)>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRegistrar {
        fn new(fail_combos: Vec<(u32, u32)>) -> Self {
            Self {
                fail_combos: Mutex::new(fail_combos),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HotkeyRegistrar for FakeRegistrar {
        fn register(&self, vk: u32, mods: u32) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("register({vk},{mods})"));
            if self.fail_combos.lock().unwrap().contains(&(vk, mods)) {
                return Err("占用".into());
            }
            Ok(())
        }
        fn unregister(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("unregister".into());
            Ok(())
        }
    }

    struct FakeStore {
        hotkey: Mutex<(u32, u32)>,
        fail_save: bool,
    }

    impl FakeStore {
        fn new() -> Self {
            Self {
                hotkey: Mutex::new((0x78, 0)),
                fail_save: false,
            }
        }
    }

    impl SettingsStore for FakeStore {
        fn load_hotkey(&self) -> (u32, u32) {
            *self.hotkey.lock().unwrap()
        }
        fn save_hotkey(&self, vk: u32, mods: u32) -> Result<(), String> {
            if self.fail_save {
                return Err("注册表写入失败（错误码 5）".into());
            }
            *self.hotkey.lock().unwrap() = (vk, mods);
            Ok(())
        }
        fn autostart_enabled(&self) -> bool {
            false
        }
        fn set_autostart(&self, _enable: bool) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn hotkey_transaction_success_saves_and_leaves_registration_on_new_combo() {
        let reg = FakeRegistrar::new(vec![]);
        let store = FakeStore::new();
        let result = apply_hotkey_transaction(&reg, &store, (0x78, 0), (0x70, MOD_CONTROL));
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(*store.hotkey.lock().unwrap(), (0x70, MOD_CONTROL));
        let calls = reg.calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec!["unregister".to_string(), "register(112,2)".to_string()]
        );
    }

    #[test]
    fn hotkey_conflict_rolls_back_to_old_registration() {
        let reg = FakeRegistrar::new(vec![(0x71, 0)]);
        let store = FakeStore::new();
        let result = apply_hotkey_transaction(&reg, &store, (0x78, 0), (0x71, 0));
        match result {
            Err(HotkeyTxError::Conflict {
                old_recovered: true,
            }) => {}
            other => panic!("期望 Conflict{{old_recovered:true}}，实际 {other:?}"),
        }
        // 存储未被改动。
        assert_eq!(*store.hotkey.lock().unwrap(), (0x78, 0));
        // 旧热键已重新注册。
        assert!(reg
            .calls
            .lock()
            .unwrap()
            .contains(&"register(120,0)".to_string()));
    }

    #[test]
    fn save_failure_restores_old_registration_and_keeps_old_value() {
        let reg = FakeRegistrar::new(vec![]);
        let mut store = FakeStore::new();
        store.fail_save = true;
        let result = apply_hotkey_transaction(&reg, &store, (0x78, 0), (0x71, 0));
        match result {
            Err(HotkeyTxError::SaveFailed {
                old_recovered: true,
                ..
            }) => {}
            other => panic!("期望 SaveFailed{{old_recovered:true}}，实际 {other:?}"),
        }
        assert!(reg
            .calls
            .lock()
            .unwrap()
            .contains(&"register(120,0)".to_string()));
    }

    #[test]
    fn failed_old_key_recovery_is_reported_critically() {
        // 新组合和旧组合都被占用：恢复失败必须显式标明。
        let reg = FakeRegistrar::new(vec![(0x71, 0), (0x78, 0)]);
        let store = FakeStore::new();
        let result = apply_hotkey_transaction(&reg, &store, (0x78, 0), (0x71, 0));
        match result {
            Err(HotkeyTxError::Conflict {
                old_recovered: false,
            }) => {}
            other => panic!("期望 Conflict{{old_recovered:false}}，实际 {other:?}"),
        }
    }

    #[test]
    fn invalid_combo_never_touches_registration() {
        let reg = FakeRegistrar::new(vec![]);
        let store = FakeStore::new();
        for (vk, mods) in [
            (0x02, 0),
            (0x41, 0),
            (0x71, 0x8),
            (0x5B, 0),
            (0x09, MOD_ALT),
            (0x1B, MOD_CONTROL),
        ] {
            let result = apply_hotkey_transaction(&reg, &store, (0x78, 0), (vk, mods));
            assert!(
                matches!(result, Err(HotkeyTxError::Invalid(_))),
                "{vk}/{mods}: {result:?}"
            );
        }
        assert!(reg.calls.lock().unwrap().is_empty(), "校验失败不得触碰注册");
        assert_eq!(*store.hotkey.lock().unwrap(), (0x78, 0));
    }

    #[test]
    fn same_combo_is_a_noop() {
        let reg = FakeRegistrar::new(vec![]);
        let store = FakeStore::new();
        assert!(apply_hotkey_transaction(&reg, &store, (0x78, 0), (0x78, 0)).is_ok());
        assert!(reg.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn hotkey_validation_rejects_dangerous_combos_and_accepts_normal_ones() {
        assert!(validate_hotkey(0x78, 0).is_ok()); // F9 单键
        assert!(validate_hotkey(0x70, MOD_CONTROL | MOD_SHIFT).is_ok());
        assert!(validate_hotkey(0x41, MOD_ALT).is_ok()); // Alt+A
        assert!(validate_hotkey(0x41, 0).is_err()); // 裸字母劫持输入
        assert!(validate_hotkey(0x31, 0).is_err()); // 裸数字
        assert!(validate_hotkey(0x02, 0).is_err()); // 鼠标键
        assert!(validate_hotkey(0x10, 0).is_err()); // 纯修饰键
        assert!(validate_hotkey(0x5B, MOD_CONTROL).is_err()); // Win 键
        assert!(validate_hotkey(0x09, MOD_ALT).is_err()); // Alt+Tab
        assert!(validate_hotkey(0x1B, MOD_CONTROL).is_err()); // Ctrl+Esc
        assert!(validate_hotkey(0x1B, MOD_CONTROL | MOD_SHIFT).is_err()); // 任务管理器
        assert!(validate_hotkey(0x20, MOD_ALT).is_err()); // Alt+Space
        assert!(validate_hotkey(0xFF, 0).is_err()); // 超范围
        assert!(validate_hotkey(0x70, 0x8).is_err()); // 非法修饰键
    }

    #[test]
    fn hotkey_persistence_roundtrip_through_store() {
        let store = FakeStore::new();
        store.save_hotkey(0x72, MOD_CONTROL).unwrap();
        assert_eq!(store.load_hotkey(), (0x72, MOD_CONTROL));
    }
}
