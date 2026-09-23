//! 托盘版本地诊断日志。只记录运行状态与错误，不记录识别文本或音频。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_LOCK: Mutex<()> = Mutex::new(());
const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn log_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("VoiceType").join("logs").join("voicetype.log")
}

pub fn record(level: &str, component: &str, message: &str) {
    let Ok(_guard) = LOG_LOCK.lock() else { return };
    let path = log_path();
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let old = path.with_extension("old.log");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&path, old);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let safe_message: String = message
        .chars()
        .take(1000)
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
    {
        let _ = writeln!(file, "{now} {level} {component}: {safe_message}");
    }
}
