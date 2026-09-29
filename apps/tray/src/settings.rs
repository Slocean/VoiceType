//! 用户设置：开机启动与全局录音快捷键的注册表持久化。

use super::*;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_DWORD, REG_SZ, REG_VALUE_TYPE,
};

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
// ---------- 开机自启（HKCU\...\Run，按用户级别，与安装版权限一致） ----------

fn werr(r: WIN32_ERROR) -> Result<(), String> {
    if r.0 == 0 {
        Ok(())
    } else {
        Err(format!("注册表操作失败（错误码 {}）", r.0))
    }
}

pub fn autostart_enabled() -> bool {
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

pub fn set_autostart(enable: bool) -> Result<(), String> {
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

pub fn save_hotkey(vk: u32, mods: u32) -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        werr(RegCreateKeyW(HKEY_CURRENT_USER, APP_KEY, &mut hkey))?;
        let bytes = (vk | (mods << 8)).to_le_bytes();
        let result = werr(RegSetValueExW(hkey, HOTKEY_VALUE, None, REG_DWORD, Some(&bytes)));
        let _ = RegCloseKey(hkey);
        result
    }
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
