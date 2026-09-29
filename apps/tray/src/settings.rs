//! 用户设置：开机启动与全局录音快捷键的持久化（工作包 B3）。
//!
//! 读写经 [`SettingsStore`] 抽象：生产用 [`RegistryStore`]（HKCU 注册表 +
//! 旧快捷方式迁移），测试注入内存实现。业务事务（先试注册、保存、全部成功
//! 才提交）在 control.rs，本模块只负责存储。

use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_DWORD, REG_SZ,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC,
};

const AUTOSTART_VALUE: PCWSTR = w!("VoiceType");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const APP_KEY: PCWSTR = w!("Software\\VoiceType");
const HOTKEY_VALUE: PCWSTR = w!("Hotkey");
pub const DEFAULT_HOTKEY_VK: u32 = 0x78;

// ---------- 存储抽象 ----------

pub trait SettingsStore: Send + Sync {
    /// 读取热键；未设置时返回默认 F9。
    fn load_hotkey(&self) -> (u32, u32);
    fn save_hotkey(&self, vk: u32, mods: u32) -> Result<(), String>;
    fn autostart_enabled(&self) -> bool;
    /// 启用/禁用自启。失败返回明确错误；成功只代表注册表与旧快捷方式
    /// 迁移都达到目标状态，调用方应回读确认。
    fn set_autostart(&self, enable: bool) -> Result<(), String>;
}

// ---------- 注册表实现 ----------

pub struct RegistryStore;

fn legacy_startup_shortcut() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup\VoiceType.lnk"),
    )
}

fn werr(r: WIN32_ERROR) -> Result<(), String> {
    if r.0 == 0 {
        Ok(())
    } else {
        Err(format!("注册表操作失败（错误码 {}）", r.0))
    }
}

impl SettingsStore for RegistryStore {
    fn autostart_enabled(&self) -> bool {
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

    fn set_autostart(&self, enable: bool) -> Result<(), String> {
        unsafe {
            let mut hkey = HKEY::default();
            werr(RegCreateKeyW(HKEY_CURRENT_USER, RUN_KEY, &mut hkey))?;
            let result = if enable {
                match std::env::current_exe() {
                    Ok(exe) => {
                        let cmd = super::wide(&format!("\"{}\"", exe.display()));
                        let bytes =
                            std::slice::from_raw_parts(cmd.as_ptr().cast::<u8>(), cmd.len() * 2);
                        werr(RegSetValueExW(
                            hkey,
                            AUTOSTART_VALUE,
                            None,
                            REG_SZ,
                            Some(bytes),
                        ))
                    }
                    Err(e) => Err(format!("定位 exe 失败: {e}")),
                }
            } else {
                let status = RegDeleteValueW(hkey, AUTOSTART_VALUE);
                if status.0 == 2 {
                    Ok(())
                } else {
                    werr(status)
                }
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

    fn load_hotkey(&self) -> (u32, u32) {
        unsafe {
            let mut hkey = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, APP_KEY, None, KEY_QUERY_VALUE, &mut hkey).0 != 0 {
                return (DEFAULT_HOTKEY_VK, 0);
            }
            let mut buf = [0u8; 4];
            let mut size = buf.len() as u32;
            let mut ty = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
            let ok = RegQueryValueExW(
                hkey,
                HOTKEY_VALUE,
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr()),
                Some(&mut size),
            )
            .0 == 0
                && size == 4;
            let _ = RegCloseKey(hkey);
            if !ok {
                return (DEFAULT_HOTKEY_VK, 0);
            }
            let raw = u32::from_le_bytes(buf);
            (raw & 0xFF, (raw >> 8) & 0xFF)
        }
    }

    fn save_hotkey(&self, vk: u32, mods: u32) -> Result<(), String> {
        unsafe {
            let mut hkey = HKEY::default();
            werr(RegCreateKeyW(HKEY_CURRENT_USER, APP_KEY, &mut hkey))?;
            let bytes = (vk | (mods << 8)).to_le_bytes();
            let result = werr(RegSetValueExW(
                hkey,
                HOTKEY_VALUE,
                None,
                REG_DWORD,
                Some(&bytes),
            ));
            let _ = RegCloseKey(hkey);
            result
        }
    }
}

// ---------- 展示 ----------

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

/// 组合键显示文本（如 "Ctrl + Shift + F9"）。mods：0x1 Alt / 0x2 Ctrl / 0x4 Shift。
pub fn combo_text(vk: u32, mods: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if mods & 0x2 != 0 {
        parts.push("Ctrl".into());
    }
    if mods & 0x4 != 0 {
        parts.push("Shift".into());
    }
    if mods & 0x1 != 0 {
        parts.push("Alt".into());
    }
    parts.push(key_name(vk));
    parts.join(" + ")
}
