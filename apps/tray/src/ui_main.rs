//! 主线程 UI 层：托盘 / 菜单 / 窗口过程 / 入口。
//!
//! 约束：注入与窗口操作只在主线程执行；worker 事件经
//! drain_worker_events 串行消费。录音启停逻辑见 toggle_recording。

use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};

// ---------- 录音开关与事件处理 ----------

pub fn toggle_recording(main_hwnd: HWND) {
    // 在可变借用之外捕获前台目标（内部需要读取 App，避免 RefCell 嵌套借用）。
    let fresh_target = control::capture_record_target();
    with_app(|app| {
        if app.recording {
            // 手动停止：由 worker 冲刷识别结果，随后按正常 Final/Empty 路径收尾。
            if app.stopping {
                return;
            }
            app.stopping = true;
            if let Some(tx) = &app.cmd_tx {
                let _ = tx.send(Cmd::Stop);
            }
            set_window_text(app.overlay_text, "正在结束录音并生成定稿…");
            return;
        }
        // B2：记录录音开始时的外部前台窗口；自身窗口等无效目标记为 None，
        // 定稿时文本复制到剪贴板而不是注入。
        app.record_target = fresh_target;
        app.interim_paused = false;
        // 录音前校验模型：主线程只做存在性/长度检查（不散列、不写模型），
        // 检查失败时 manager 会在后台修复，界面随状态刷新。
        let Some(manager) = model_manager::global() else {
            return;
        };
        let (model_dir, _kind) = match manager.ensure_ready_for_record() {
            Ok(result) => result,
            Err(message) => {
                diagnostics::record("WARN", "models", &message);
                set_window_text(app.overlay_text, &format!("⚠ {message}"));
                unsafe {
                    let _ = ShowWindow(app.overlay, SW_SHOW);
                }
                return;
            }
        };
        let Some(asr_paths) = manager.scan_asr_files(&model_dir) else {
            set_window_text(
                app.overlay_text,
                &format!("ASR 模型缺失: {}", model_dir.display()),
            );
            unsafe {
                let _ = ShowWindow(app.overlay, SW_SHOW);
            }
            return;
        };
        let Some(vad_model) =
            manager.find_vad_model(&model_dir, model_dir.parent().unwrap_or(Path::new(".")))
        else {
            set_window_text(
                app.overlay_text,
                &format!("VAD 模型缺失: {}", model_dir.display()),
            );
            unsafe {
                let _ = ShowWindow(app.overlay, SW_SHOW);
            }
            return;
        };
        let (cmd_tx, cmd_rx) = mpsc::channel();
        match start_worker(
            NotifyHwnd(main_hwnd),
            super::engine_config(asr_paths),
            vad_model,
            cmd_rx,
        ) {
            Ok(handle) => {
                app.cmd_tx = Some(cmd_tx);
                app.ev_rx = Some(handle.ev_rx);
                app.worker = Some(handle.join);
                app.recording = true;
                set_window_text(app.overlay_text, "🎤 说话吧，停顿自动结束…");
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
    });
    settings_web::refresh_all();
}

// ---------- 事件消费 ----------

pub fn drain_worker_events() {
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
    let session_changed = events.iter().any(|ev| !matches!(ev, Ev::Interim(_)));

    for ev in events {
        match ev {
            Ev::Interim(text) => {
                with_app(|app| {
                    if !app.recording {
                        return;
                    }
                    if app.mode == InjectMode::Hybrid {
                        // B2：中间结果上屏前确认目标仍是录音开始时的窗口。
                        let ready = app.record_target.is_some_and(control::target_ready);
                        if ready {
                            app.interim_paused = false;
                            set_window_text(app.overlay_text, &format!("🎤 {text}"));
                            if let Err(e) = app.hybrid.update_interim(&text) {
                                diagnostics::record("ERROR", "injection", &e.to_string());
                                set_window_text(app.overlay_text, &format!("实时上屏失败：{e}"));
                            }
                        } else if !app.interim_paused {
                            // 焦点移走：立即停止向任何窗口键入，保持提示可见。
                            app.interim_paused = true;
                            diagnostics::record(
                                "INFO",
                                "injection",
                                "目标窗口已切换，暂停实时上屏",
                            );
                            set_window_text(
                                app.overlay_text,
                                "⚠ 目标窗口已切换，暂停实时上屏；结束后文本将复制到剪贴板",
                            );
                        }
                    } else {
                        set_window_text(app.overlay_text, &format!("🎤 {text}"));
                    }
                });
            }
            Ev::Final(text) => {
                with_app(|app| {
                    app.recording = false;
                    app.stopping = false;
                    app.cmd_tx = None;
                    app.ev_rx = None;
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                    set_overlay_idle(app.overlay_text, &app.hotkey_name);
                    // B2：定稿前确认目标窗口。失效时绝不注入/退格，文本复制到剪贴板。
                    let target_ok = app.record_target.is_some_and(control::target_ready);
                    let result: Result<(), String> = if !target_ok {
                        app.hybrid.reset();
                        control::deliver_text_without_target(&text)
                    } else {
                        match app.mode {
                            InjectMode::Paste => ClipboardInjector::default()
                                .inject(&text)
                                .map_err(|e| e.to_string()),
                            InjectMode::Type => TypingInjector::default()
                                .inject(&text)
                                .map_err(|e| e.to_string()),
                            InjectMode::Hybrid => {
                                let r = if text.is_empty() {
                                    app.hybrid.cancel()
                                } else {
                                    app.hybrid.commit(&text)
                                };
                                r.map_err(|e| e.to_string())
                            }
                        }
                    };
                    match result {
                        Ok(()) if !target_ok && !text.is_empty() => set_window_text(
                            app.overlay_text,
                            "目标窗口已切换：识别文本已复制到剪贴板，请自行粘贴",
                        ),
                        Err(e) => {
                            diagnostics::record("ERROR", "injection", &e.to_string());
                            set_window_text(app.overlay_text, &format!("文本注入失败：{e}"));
                        }
                        _ => {}
                    }
                });
            }
            // VAD 断句但无语音内容：撤销上屏残留并提示（悬浮条常驻可见）。
            Ev::Empty => {
                with_app(|app| {
                    app.recording = false;
                    app.stopping = false;
                    app.cmd_tx = None;
                    app.ev_rx = None;
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                    // 目标失效时退格会落入新窗口：只清状态，不发按键。
                    let target_ok = app.record_target.is_some_and(control::target_ready);
                    if target_ok {
                        let _ = app.hybrid.cancel();
                    } else {
                        app.hybrid.reset();
                    }
                    set_window_text(app.overlay_text, "未检测到语音，已自动停止（按快捷键重试）");
                });
            }
            Ev::Error(msg) => {
                diagnostics::record("ERROR", "recording", &msg);
                with_app(|app| {
                    app.recording = false;
                    app.stopping = false;
                    app.cmd_tx = None;
                    app.ev_rx = None;
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                    let target_ok = app.record_target.is_some_and(control::target_ready);
                    if target_ok {
                        let _ = app.hybrid.cancel();
                    } else {
                        app.hybrid.reset();
                    }
                    set_window_text(app.overlay_text, &format!("⚠ {msg}"));
                });
            }
        }
    }
    if session_changed {
        settings_web::refresh_all();
    }
}

// ---------- 托盘 ----------

/// 加载 exe 资源段里的内嵌图标（build.rs 经 winresource 写入，资源 ID 1）。
/// 找不到时回退系统默认应用图标（开发构建等场景）。
unsafe fn load_app_icon(hinst: HINSTANCE, small: bool) -> HICON {
    let (cx, cy) = if small {
        (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))
    } else {
        (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
    };
    LoadImageW(
        Some(hinst),
        PCWSTR(1 as _),
        IMAGE_ICON,
        cx,
        cy,
        LR_DEFAULTSIZE,
    )
    .map(|h| HICON(h.0))
    .ok()
    .unwrap_or_else(|| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let icon = load_app_icon(hinst, true);
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    let name = with_app(|app| app.hotkey_name.clone()).unwrap_or_else(|| "F9".into());
    let tip = wide(&format!(
        "VoiceType 语音输入（按 {name} 开始，停顿自动结束）"
    ));
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

/// 托盘气泡通知：Win11 默认把新图标折叠进「隐藏图标」溢出区，启动时
/// 主动冒个泡，告诉用户入口在托盘。
unsafe fn show_tray_balloon(hwnd: HWND, title: &str, text: &str) {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    let t = wide(title);
    let b = wide(text);
    let tn = (t.len() - 1).min(data.szInfoTitle.len());
    data.szInfoTitle[..tn].copy_from_slice(&t[..tn]);
    let bn = (b.len() - 1).min(data.szInfo.len());
    data.szInfo[..bn].copy_from_slice(&b[..bn]);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let mode = with_app(|app| app.mode).unwrap_or(InjectMode::Hybrid);
    let Ok(menu) = CreatePopupMenu() else { return };
    let item = |id: i32, label: &str, checked: bool| {
        let flags = if checked {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let text = wide(label);
        let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(text.as_ptr()));
    };
    item(MENU_MODE_PASTE, "粘贴模式", mode == InjectMode::Paste);
    item(MENU_MODE_TYPE, "键入模式", mode == InjectMode::Type);
    item(
        MENU_MODE_HYBRID,
        "混合模式（流式）",
        mode == InjectMode::Hybrid,
    );
    let sep = wide("");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR(sep.as_ptr()));
    item(MENU_MAIN, "主界面…", false);
    // 置灰的版本行，方便确认热更后的实际版本。
    {
        let text = wide(&format!("当前版本 v{}", updater::APP_VERSION));
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(text.as_ptr()));
    }
    item(MENU_UPDATE_CHECK, "检查更新…", false);
    item(MENU_ANNOUNCEMENT, "更新公告", false);
    let sep = wide("");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR(sep.as_ptr()));
    let quit = wide("退出");
    let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, PCWSTR(quit.as_ptr()));

    // B2：菜单会抢走前台焦点；关闭后恢复之前的外部前台窗口。
    let previous_fg = GetForegroundWindow();
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

    let mut opened_settings = false;
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
        MENU_MAIN => {
            unsafe {
                settings_web::show_settings(hwnd);
            }
            opened_settings = true;
        }
        MENU_UPDATE_CHECK => {
            set_window_text(
                overlay_text_of(hwnd),
                &format!("检查更新中…（当前 v{}）", updater::APP_VERSION),
            );
            updater::spawn_check_and_download(hwnd.0 as isize);
        }
        MENU_ANNOUNCEMENT => {
            updater::spawn_fetch_announcement(hwnd.0 as isize);
        }
        MENU_QUIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
    // 主界面自己要获得前台；其余情况把焦点还给用户原来所在的窗口。
    if !opened_settings && !previous_fg.is_invalid() && IsWindow(Some(previous_fg)).as_bool() {
        let _ = SetForegroundWindow(previous_fg);
    }
}

fn overlay_text_of(hwnd: HWND) -> HWND {
    with_app(|app| app.overlay_text).unwrap_or(hwnd)
}

/// 在悬浮条上显示一条用户可见消息（控制层提示用）。
pub fn set_overlay_message(text: &str) {
    let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
    if hwnd.is_invalid() {
        return;
    }
    set_window_text(overlay_text_of(hwnd), text);
    unsafe {
        if let Some(overlay) = with_app(|app| app.overlay) {
            let _ = ShowWindow(overlay, SW_SHOW);
        }
    }
}

/// 托盘气泡提示跟随当前热键。
pub fn update_tray_tip(hwnd: HWND) {
    let name = with_app(|app| app.hotkey_name.clone()).unwrap_or_default();
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    let tip = wide(&format!(
        "VoiceType 语音输入（按 {name} 开始，停顿自动结束）"
    ));
    let n = tip.len().min(data.szTip.len());
    data.szTip[..n].copy_from_slice(&tip[..n]);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn set_overlay_idle(overlay_text: HWND, name: &str) {
    set_window_text(overlay_text, &format!("🎤 就绪 — 按 {name} 开始说话"));
}

/// 更新后台任务完成：按事件类别（检查更新/公告）分发；结果持久保存并在
/// 界面展示；托盘触发且页面不可见时用气泡兜底；新版就绪时保留确认重启流程。
fn handle_update_event(hwnd: HWND) {
    let Some(ev) = updater::take_update_event() else {
        return;
    };
    if !ev.ok {
        diagnostics::record("ERROR", "updater", &ev.text);
    }
    let first_line: String = ev.text.lines().next().unwrap_or_default().to_string();
    match ev.kind {
        updater::UpdateEventKind::Check => {
            with_app(|app| app.last_check = Some((ev.ok, ev.text.clone())));
            settings_web::refresh_update_status();
            let seq = control::take_pending("checkUpdate");
            if let Some(seq) = seq {
                settings_web::deliver_done(seq, "checkUpdate", ev.ok, &ev.text);
            }
            if ev.ready {
                // 新版已下载校验完毕：询问后重启（托盘与页面触发都走这里）。
                unsafe {
                    let title = wide("VoiceType 更新");
                    let text = wide(&ev.text);
                    let choice = MessageBoxW(
                        Some(hwnd),
                        PCWSTR(text.as_ptr()),
                        PCWSTR(title.as_ptr()),
                        MB_ICONINFORMATION | MB_YESNO,
                    );
                    if choice == IDYES {
                        // 助手已拉起：等本进程退出后（便携）换 exe 重启 /（安装版）静默重装。
                        match updater::apply_update_and_restart() {
                            Ok(()) => {
                                let _ = DestroyWindow(hwnd);
                            }
                            Err(e) => {
                                let t = wide(&format!("应用更新失败：{e}"));
                                let _ = MessageBoxW(
                                    Some(hwnd),
                                    PCWSTR(t.as_ptr()),
                                    PCWSTR(title.as_ptr()),
                                    MB_ICONWARNING,
                                );
                            }
                        }
                    }
                }
            } else if seq.is_none() {
                // 托盘触发（无界面动作编号）：气泡兜底；持久结果在更新页可查。
                let title = if ev.ok {
                    "VoiceType 更新"
                } else {
                    "VoiceType 更新失败"
                };
                unsafe {
                    show_tray_balloon(hwnd, title, &first_line);
                }
            }
        }
        updater::UpdateEventKind::Announcement => {
            with_app(|app| app.last_announcement = Some((ev.ok, ev.text.clone())));
            let delivered = settings_web::fill_announcement(&ev.text);
            if let Some(seq) = control::take_pending("announcement") {
                settings_web::deliver_done(seq, "announcement", ev.ok, &ev.text);
            }
            if !delivered {
                // 页面不可见：气泡兜底；持久结果保存在「更新与关于」页。
                let title = if ev.ok {
                    "更新公告"
                } else {
                    "获取公告失败"
                };
                unsafe {
                    show_tray_balloon(hwnd, title, &first_line);
                }
            }
        }
    }
}

// ---------- 窗口过程 ----------

pub(crate) unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
        WM_UPDATE_EVENT => {
            handle_update_event(hwnd);
            LRESULT(0)
        }
        WM_SETTINGS_READY => {
            settings_web::refresh_all();
            LRESULT(0)
        }
        WM_SIZE => {
            // C2：设置窗口尺寸变化（含 DPI/显示器切换）时同步 WebView 子窗口。
            if with_app(|app| app.settings == hwnd).unwrap_or(false) {
                settings_web::resize_webview(hwnd);
            }
            LRESULT(0)
        }
        WM_TRAY_CALLBACK => {
            let mouse = (lparam.0 as u32) & 0xFFFF;
            if mouse == WM_RBUTTONUP {
                show_tray_menu(hwnd);
            } else if mouse == WM_LBUTTONDBLCLK {
                // 双击托盘：打开主界面（录音开关走 F9，避免与窗口操作混淆）。
                settings_web::show_settings(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 主界面窗口：关闭=隐藏（托盘常驻应用不真正退出）。
            if with_app(|app| app.settings == hwnd).unwrap_or(false) {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            } else {
                LRESULT(0)
            }
        }
        WM_DESTROY => {
            if with_app(|app| app.main_hwnd != hwnd).unwrap_or(true) {
                return LRESULT(0);
            }
            // 退出前收尾：移除托盘、停掉进行中的录音。
            remove_tray_icon(hwnd);
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        // 任务栏 / Alt-Tab / 资源管理器显示内嵌图标。
        let big_icon = load_app_icon(hinst, false);

        // 主窗口：隐藏的消息窗口（热键/托盘回调的载体）。
        let class = w!("VoiceTypeMainWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            hIcon: big_icon,
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
            hIcon: big_icon,
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
                stopping: false,
                cmd_tx: None,
                ev_rx: None,
                worker: None,
                overlay,
                overlay_text,
                hybrid: HybridInjector::default(),
                main_hwnd: hwnd,
                settings: HWND::default(),
                hotkey_vk: 0x78,
                hotkey_mods: 0,
                hotkey_name: "F9".to_string(),
                store: std::sync::Arc::new(settings::RegistryStore),
                record_target: None,
                interim_paused: false,
                pending: std::collections::HashMap::new(),
                last_check: None,
                last_announcement: None,
            });
        });

        // 加载用户自定义热键（默认 F9），注册失败时提示不退出。
        let (hotkey_vk, hotkey_mods) =
            with_app(|app| app.store.load_hotkey()).unwrap_or((settings::DEFAULT_HOTKEY_VK, 0));
        let hotkey_name = settings::combo_text(hotkey_vk, hotkey_mods);
        with_app(|app| {
            app.hotkey_vk = hotkey_vk;
            app.hotkey_mods = hotkey_mods;
            app.hotkey_name = hotkey_name.clone();
        });
        // 热键被其他应用占用时不静默退出——托盘应用没了图标用户找不着北。
        if let Err(e) = RegisterHotKey(
            Some(hwnd),
            HOTKEY_ID,
            HOT_KEY_MODIFIERS(hotkey_mods | MOD_NOREPEAT.0),
            hotkey_vk,
        ) {
            let text = wide(&format!(
                "全局热键注册失败（可能被其他应用占用）：{e}\n\nVoiceType 将继续在托盘运行，可在主界面重新设置快捷键。"
            ));
            let title = wide("VoiceType");
            let _ = MessageBoxW(
                Some(hwnd),
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_ICONWARNING,
            );
        }
        add_tray_icon(hwnd);
        // 悬浮条常驻：就绪态常显，录音时显示识别文本。
        set_window_text(
            overlay_text,
            &format!("🎤 就绪 — 按 {} 开始说话", hotkey_name),
        );
        let _ = ShowWindow(overlay, SW_SHOW);
        // 后台修复结果要能刷新界面：把主窗口句柄交给模型管理器。
        if let Some(manager) = model_manager::global() {
            manager.set_notify_hwnd(hwnd.0 as isize);
            // 首次使用（快速检查模型未就绪）时打开主界面引导，同时冒泡提示托盘位置。
            if !manager.quick_ready() {
                settings_web::show_settings(hwnd);
                show_tray_balloon(
                    hwnd,
                    "VoiceType 已在后台运行",
                    "主界面已打开：内置模型未就绪，正在后台修复，请查看模型状态。",
                );
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
