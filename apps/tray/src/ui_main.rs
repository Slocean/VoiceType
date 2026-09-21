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
        // 启动新的识别会话。
        let model_dir = std::env::var("VOICETYPE_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_MODEL_DIR));
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
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
