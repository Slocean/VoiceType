//! 主线程 UI 层：托盘 / 菜单 / 窗口过程 / 入口。
//!
//! 约束：注入与窗口操作只在主线程执行；worker 事件经
//! drain_worker_events 串行消费。录音启停逻辑见 toggle_recording。

use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey};

// ---------- 录音开关与事件处理 ----------

pub fn toggle_recording(main_hwnd: HWND) {
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
        // 模型随 exe 内置，正常情况下首次启动已解压；走到这里说明磁盘不可写等异常。
        if !models_dl::models_ready() {
            let recovery = models_dl::ensure_models_extracted();
            if let Err(ref e) = recovery {
                diagnostics::record("ERROR", "models", e);
                settings_web::set_model_error(e);
            }
        }
        if !models_dl::models_ready() {
            unsafe {
                let text = wide(
                    "语音模型未就绪（内置模型解压可能失败，请检查磁盘空间或安装目录权限）。

排除后重启应用即可。",
                );
                let title = wide("VoiceType 模型异常");
                let _ = MessageBoxW(
                    Some(main_hwnd),
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_ICONWARNING,
                );
            }
            return;
        }
        // 启动新的识别会话。
        let model_dir = resolve_model_dir();
        let Some(asr_cfg) = find_model_files(&model_dir) else {
            set_window_text(
                app.overlay_text,
                &format!("ASR 模型缺失: {}", model_dir.display()),
            );
            unsafe {
                let _ = ShowWindow(app.overlay, SW_SHOW);
            }
            return;
        };
        let Some(vad_model) = find_vad_model(&model_dir) else {
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
            asr_cfg,
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
                    set_window_text(app.overlay_text, &format!("🎤 {text}"));
                    if app.mode == InjectMode::Hybrid {
                        if let Err(e) = app.hybrid.update_interim(&text) {
                            diagnostics::record("ERROR", "injection", &e.to_string());
                            set_window_text(app.overlay_text, &format!("实时上屏失败：{e}"));
                        }
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
                    let result = match app.mode {
                        InjectMode::Paste => {
                            ClipboardInjector::default().inject(&text)
                        }
                        InjectMode::Type => {
                            TypingInjector::default().inject(&text)
                        }
                        InjectMode::Hybrid => {
                            if text.is_empty() {
                                app.hybrid.cancel()
                            } else {
                                app.hybrid.commit(&text)
                            }
                        }
                    };
                    if let Err(e) = result {
                        diagnostics::record("ERROR", "injection", &e.to_string());
                        set_window_text(app.overlay_text, &format!("文本注入失败：{e}"));
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
                    let _ = app.hybrid.cancel();
                    set_window_text(
                        app.overlay_text,
                        "未检测到语音，已自动停止（按 F9 重试）",
                    );
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
                    let _ = app.hybrid.cancel();
                    set_window_text(app.overlay_text, &format!("⚠ {msg}"));
                });
            }
        }
    }
    if session_changed { settings_web::refresh_all(); }
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
        Some(hinst.into()),
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
    let tip = wide(&format!("VoiceType 语音输入（按 {name} 开始，停顿自动结束）"));
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
    data.szInfoTitle[..t.len() - 1].copy_from_slice(&t[..t.len() - 1]);
    data.szInfo[..b.len() - 1].copy_from_slice(&b[..b.len() - 1]);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
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
        MENU_MAIN => {
            unsafe {
                settings_web::show_settings(hwnd);
            }
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
}

fn overlay_text_of(hwnd: HWND) -> HWND {
    with_app(|app| app.overlay_text).unwrap_or(hwnd)
}

/// 应用新的录音热键：先注销旧的，注册成功才更新状态与托盘提示；
/// 失败（冲突/被占用）时回退注册原热键，返回 false 供 UI 提示。
pub fn apply_new_hotkey(vk: u32, mods: u32) -> bool {
    let hwnd = with_app(|app| app.main_hwnd).unwrap_or_default();
    if hwnd.is_invalid() {
        return false;
    }
    unsafe {
        // 注册和注销必须使用同一窗口句柄，否则旧热键会继续生效。
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
        if RegisterHotKey(Some(hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0), vk).is_ok() {
            with_app(|app| {
                app.hotkey_vk = vk;
                app.hotkey_mods = mods;
                app.hotkey_name = settings::combo_text(vk, mods);
            });
            update_tray_tip(hwnd);
            true
        } else {
            let (ovk, omods) =
                with_app(|app| (app.hotkey_vk, app.hotkey_mods)).unwrap_or((0x78, 0));
            let _ = RegisterHotKey(
                Some(hwnd),
                HOTKEY_ID,
                HOT_KEY_MODIFIERS(omods | MOD_NOREPEAT.0),
                ovk,
            );
            false
        }
    }
}

/// 托盘气泡提示跟随当前热键。
fn update_tray_tip(hwnd: HWND) {
    let name = with_app(|app| app.hotkey_name.clone()).unwrap_or_default();
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    let tip = wide(&format!("VoiceType 语音输入（按 {name} 开始，停顿自动结束）"));
    let n = tip.len().min(data.szTip.len());
    data.szTip[..n].copy_from_slice(&tip[..n]);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn set_overlay_idle(overlay_text: HWND, name: &str) {
    set_window_text(overlay_text, &format!("🎤 就绪 — 按 {name} 开始说话"));
}

/// 更新后台任务完成：主界面开着则把公告写进窗口，否则弹窗；可应用时询问重启。
fn handle_update_event(hwnd: HWND) {
    let Some(ev) = updater::take_update_event() else { return };
    if !ev.ok {
        diagnostics::record("ERROR", "updater", &ev.text);
    }
    settings_web::refresh_update_status();
    if ev.ok && !ev.ready && settings_web::fill_announcement(&ev.text) {
        return;
    }
    unsafe {
        let title = wide(if ev.ok { "VoiceType 更新" } else { "VoiceType 更新失败" });
        let text = wide(&ev.text);
        let flags = if ev.ready {
            MB_ICONINFORMATION | MB_YESNO
        } else if ev.ok {
            MB_ICONINFORMATION
        } else {
            MB_ICONWARNING
        };
        let choice = MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            flags,
        );
        if ev.ready && choice == IDYES {
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
}

// ---------- 窗口过程 ----------

pub(crate) unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
            });
        });

        // 加载用户自定义热键（默认 F9），注册失败时提示不退出。
        let (hotkey_vk, hotkey_mods) = settings::load_hotkey();
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
        set_window_text(overlay_text, &format!("🎤 就绪 — 按 {} 开始说话", hotkey_name));
        let _ = ShowWindow(overlay, SW_SHOW);
        // 首次使用（模型未就绪）时直接打开主界面引导下载，同时冒泡提示托盘位置。
        if !models_dl::models_ready() {
            settings_web::show_settings(hwnd);
            show_tray_balloon(
                hwnd,
                "VoiceType 已在后台运行",
                "主界面已打开：内置模型未就绪，请检查磁盘空间或安装目录权限。",
            );
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
