//! 注入层自测：自建 EDIT 控件作为注入目标，自动验证三种策略。
//!
//! 为什么不依赖外部窗口：自动化环境中本进程是后台进程，Windows 前台锁定
//! （foreground lock）会拒绝 SetForegroundWindow，外部窗口（如记事本）
//! 拿不到焦点，注入事件不知去向。自建窗口 + Alt 解锁技巧可稳定拿到前台。
//!
//! 运行：cargo run -p voicetype-demo --bin selftest
//! 通过标准：三种策略注入后 EDIT 内容与期望一致，进程退出码 0。

use std::thread::sleep;
use std::time::Duration;
use voicetype_inject::{
    ClipboardInjector, HybridInjector, StreamingInjector, TextInjector, TypingInjector,
};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::*;

const EXPECTED: &str = "你好VoiceType测试123";
// EDIT 控件消息/样式常量，直接定义避免引入额外 feature。
const ES_MULTILINE: u32 = 0x0004;
const EM_SETSEL: u32 = 0x00B1;
const WM_CLEAR_MSG: u32 = 0x0303;

unsafe fn pump_messages(duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    let mut msg = MSG::default();
    while std::time::Instant::now() < deadline {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        sleep(Duration::from_millis(10));
    }
}

unsafe fn create_edit_window() -> HWND {
    let hinst: windows::Win32::Foundation::HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"), // 系统预注册类，无需 RegisterClass
        w!(""),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE | WINDOW_STYLE(ES_MULTILINE),
        200,
        200,
        520,
        220,
        None,
        None,
        Some(hinst),
        None,
    )
    .expect("创建 EDIT 窗口失败");

    // 前台锁定解锁技巧：先发一次 Alt 键，获得 SetForegroundWindow 权限。
    keybd_event_alt();
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    pump_messages(Duration::from_millis(300));
    hwnd
}

fn keybd_event_alt() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    };
    let mut events = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    unsafe {
        SendInput(&mut events, std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::INPUT>() as i32)
    };
}

unsafe fn read_edit_text(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 4096];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn clear_edit(hwnd: HWND) {
    // 全选并删除，为下一种策略清空目标。
    SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    SendMessageW(hwnd, WM_CLEAR_MSG, Some(WPARAM(0)), Some(LPARAM(0)));
    pump_messages(Duration::from_millis(100));
}

fn check(name: &str, actual: String) -> bool {
    if actual == EXPECTED {
        println!("[PASS] {name}: 内容一致");
        true
    } else {
        println!("[FAIL] {name}: 期望 {EXPECTED:?}，实际 {actual:?}");
        false
    }
}

fn main() {
    let mut pass = true;

    unsafe {
        let hwnd = create_edit_window();
        let foreground_ok = GetForegroundWindow() == hwnd;
        println!("前台窗口获取: {}", if foreground_ok { "成功" } else { "失败（结果可能不可信）" });

        // 策略一：Unicode 逐字键入
        TypingInjector::default().inject(EXPECTED).expect("type 注入失败");
        pump_messages(Duration::from_millis(500));
        pass &= check("type  ", read_edit_text(hwnd));
        clear_edit(hwnd);

        // 策略二：保留用户原有剪贴板内容，不注入测试标记。
        let original_clipboard = arboard::Clipboard::new()
            .and_then(|mut cb| cb.get_text())
            .ok();
        ClipboardInjector::default().inject(EXPECTED).expect("paste 注入失败");
        pump_messages(Duration::from_millis(500));
        pass &= check("paste ", read_edit_text(hwnd));
        // 等待异步恢复完成后校验剪贴板内容。
        pump_messages(Duration::from_millis(500));
        let restored = arboard::Clipboard::new()
            .and_then(|mut cb| cb.get_text())
            .ok();
        if restored == original_clipboard {
            println!("[PASS] 剪贴板文本保持原样");
        } else {
            println!("[FAIL] 剪贴板文本发生变化");
            pass = false;
        }
        clear_edit(hwnd);

        // 策略三：混合流式（模拟两段中间结果后提交定稿）
        // 分步输出中间内容用于诊断：逐字上屏 → 撤销 → 粘贴定稿。
        let mut injector = HybridInjector::default();
        let chars: Vec<char> = EXPECTED.chars().collect();
        let mid = chars.len() / 2;
        let part1: String = chars[..mid].iter().collect();
        injector
            .update_interim(&part1)
            .expect("hybrid 中间结果1失败");
        pump_messages(Duration::from_millis(300));
        println!("[diag] hybrid 上屏第一段后: {:?}", read_edit_text(hwnd));
        injector.update_interim(EXPECTED).expect("hybrid 中间结果2失败");
        pump_messages(Duration::from_millis(300));
        println!("[diag] hybrid 上屏全量后: {:?}", read_edit_text(hwnd));
        injector.cancel().expect("hybrid 撤销失败");
        pump_messages(Duration::from_millis(300));
        println!("[diag] hybrid 撤销后: {:?}", read_edit_text(hwnd));
        ClipboardInjector::default()
            .inject(EXPECTED)
            .expect("hybrid 定稿粘贴失败");
        pump_messages(Duration::from_millis(500));
        pass &= check("hybrid", read_edit_text(hwnd));

        let _ = DestroyWindow(hwnd);
    }

    if pass {
        println!("全部通过");
    } else {
        println!("存在失败项");
        std::process::exit(1);
    }
}
