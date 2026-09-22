//! 主线程 UI 层：托盘 / 菜单 / 窗口过程 / 入口。
//!
//! 约束：注入与窗口操作只在主线程执行；worker 事件经
//! drain_worker_events 串行消费。录音启停逻辑见 toggle_recording。

use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};

// ---------- 录音开关与事件处理 ----------

pub fn toggle_recording(main_hwnd: HWND) {
    with_app(|app| {
        if app.recording {
            // 手动停止：通知 worker 退出，同时撤销已上屏的中间结果。
            if let Some(tx) = app.cmd_tx.take() {
                let _ = tx.send(Cmd::Stop);
            }
            if let Some(h) = app.worker.take() {
                let _ = h.join();
            }
            app.ev_rx = None;
            app.recording = false;
            let _ = app.hybrid.cancel();
            unsafe {
                let _ = ShowWindow(app.overlay, SW_HIDE);
            }
            return;
        }
        // 首次使用引导：模型未就绪时直接在 UI 里发起一键下载，不让用户去找脚本。
        if !models_dl::models_ready() {
            unsafe {
                let text = wide(
                    "语音模型尚未下载（约 15MB，需联网一次）。\n\n是否立即下载？下载完成后按 F9 即可开始语音输入。",
                );
                let title = wide("VoiceType 首次使用");
                let choice = MessageBoxW(
                    Some(main_hwnd),
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_ICONQUESTION | MB_YESNO,
                );
                if choice == IDYES {
                    set_window_text(app.overlay_text, "正在下载语音模型…");
                    let _ = ShowWindow(app.overlay, SW_SHOW);
                    models_dl::spawn_model_download(main_hwnd.0 as isize);
                }
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
            cmd_tx.clone(),
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
            // VAD 断句但无语音内容（静默会话）：撤销上屏残留并静默收尾。
            Ev::Empty => {
                with_app(|app| {
                    app.recording = false;
                    app.ev_rx = None;
                    if let Some(h) = app.worker.take() {
                        let _ = h.join();
                    }
                    let _ = app.hybrid.cancel();
                    unsafe {
                        let _ = ShowWindow(app.overlay, SW_HIDE);
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
    let tip = wide("VoiceType 语音输入（F9 开始，停顿自动结束）");
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
    item(MENU_DOWNLOAD_MODELS, "下载语音模型…", false);
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
                settings::show_settings(hwnd);
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
        MENU_DOWNLOAD_MODELS => {
            if models_dl::models_ready() {
                unsafe {
                    let text = wide("语音模型已就绪，无需下载。");
                    let title = wide("VoiceType 模型");
                    let _ = MessageBoxW(
                        Some(hwnd),
                        PCWSTR(text.as_ptr()),
                        PCWSTR(title.as_ptr()),
                        MB_ICONINFORMATION,
                    );
                }
            } else {
                set_window_text(overlay_text_of(hwnd), "正在下载语音模型…");
                models_dl::spawn_model_download(hwnd.0 as isize);
            }
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

/// 模型下载后台任务：进度刷新悬浮条文本；结束时收起悬浮条、刷新主界面并弹窗。
fn handle_model_event(hwnd: HWND) {
    let Some(ev) = models_dl::take_model_event() else { return };
    if !ev.done {
        set_window_text(overlay_text_of(hwnd), &ev.text);
        return;
    }
    with_app(|app| unsafe {
        let _ = ShowWindow(app.overlay, SW_HIDE);
    });
    settings::refresh_model_status();
    unsafe {
        let title = wide(if ev.ok { "VoiceType 模型" } else { "VoiceType 模型下载失败" });
        let text = wide(&ev.text);
        let _ = MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            if ev.ok { MB_ICONINFORMATION } else { MB_ICONWARNING },
        );
    }
}

/// 更新后台任务完成：主界面开着则把公告写进窗口，否则弹窗；可应用时询问重启。
fn handle_update_event(hwnd: HWND) {
    let Some(ev) = updater::take_update_event() else { return };
    if ev.ok && !ev.ready && settings::fill_announcement(&ev.text) {
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
        WM_MODEL_EVENT => {
            handle_model_event(hwnd);
            LRESULT(0)
        }
        WM_TRAY_CALLBACK => {
            let mouse = (lparam.0 as u32) & 0xFFFF;
            if mouse == WM_RBUTTONUP {
                show_tray_menu(hwnd);
            } else if mouse == WM_LBUTTONDBLCLK {
                // 双击托盘：打开主界面（录音开关走 F9，避免与窗口操作混淆）。
                settings::show_settings(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            unsafe {
                settings::on_command(hwnd, id);
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
                cmd_tx: None,
                ev_rx: None,
                worker: None,
                overlay,
                overlay_text,
                hybrid: HybridInjector::default(),
                settings: HWND::default(),
            });
        });

        // 热键被其他应用占用时不静默退出——托盘应用没了图标用户找不着北。
        if let Err(e) = RegisterHotKey(Some(hwnd), HOTKEY_ID, MOD_NOREPEAT, VK_F9.0 as u32) {
            let text = wide(&format!(
                "F9 全局热键注册失败（可能被其他应用占用）：{e}\n\nVoiceType 将继续在托盘运行，可稍后重启本应用重试。"
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
        // 首次使用（模型未就绪）时直接打开主界面引导下载，同时冒泡提示托盘位置。
        if !models_dl::models_ready() {
            settings::show_settings(hwnd);
            show_tray_balloon(
                hwnd,
                "VoiceType 已在后台运行",
                "主界面已打开：可一键下载语音模型。按 F9 开始语音输入。",
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
