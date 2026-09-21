//! 应用内一键下载语音模型（便携版免脚本流程）：
//! silero_vad.onnx（约 0.6MB）+ 流式中文识别模型 tar.bz2（约 14MB，下载后自动解压）。
//! 模型来自 sherpa-onnx 官方 Release，与 scripts/setup-models.ps1 同源同布局：
//! models\<识别模型子目录>\*.onnx + models\silero_vad.onnx。
//!
//! 交互模型与 updater 一致：后台线程下载解压，经 WM_MODEL_EVENT 唤醒主线程
//! 取走 ModelEvent 展示进度/结果。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::WM_MODEL_EVENT;

const USER_AGENT: &str = concat!("VoiceType/", env!("CARGO_PKG_VERSION"));

/// 识别模型子目录名（tar 包解压后的目录名，与 setup-models.ps1 一致）。
pub const MODEL_SUBDIR: &str = "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23";

const VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";
const ASR_URL: &str = concat!(
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/",
    "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23.tar.bz2"
);

/// 进度事件（主线程消费）。
pub struct ModelEvent {
    pub done: bool,
    pub ok: bool,
    pub text: String,
}

pub static MODEL_STATE: Mutex<Option<ModelEvent>> = Mutex::new(None);

/// 可跨线程发送的窗口句柄包装，用于唤醒主线程。
#[derive(Clone, Copy)]
struct NotifyHwnd(isize);
unsafe impl Send for NotifyHwnd {}
impl NotifyHwnd {
    fn post(self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.0 as *mut _)),
                WM_MODEL_EVENT,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// 模型根目录（下载目标 / 运行时查找的一致口径）：
/// VOICETYPE_MODEL_DIR 显式指定 > exe 旁 models\（安装版/便携包布局）> 工作目录 models\（仓库布局）。
pub fn models_root() -> PathBuf {
    if let Ok(d) = std::env::var("VOICETYPE_MODEL_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join("models");
            if p.is_dir() {
                return p;
            }
        }
    }
    PathBuf::from("models")
}

fn vad_path(root: &Path) -> PathBuf {
    root.join("silero_vad.onnx")
}

fn asr_dir(root: &Path) -> PathBuf {
    root.join(MODEL_SUBDIR)
}

/// 识别模型四件套 + VAD 是否齐备（tokens.txt 是 tar 包解压完成的标志）。
pub fn models_ready() -> bool {
    let root = models_root();
    vad_path(&root).is_file() && asr_dir(&root).join("tokens.txt").is_file()
}

fn emit(notify: NotifyHwnd, done: bool, ok: bool, text: String) {
    if let Ok(mut st) = MODEL_STATE.lock() {
        *st = Some(ModelEvent { done, ok, text });
    }
    notify.post();
}

/// 带进度的 HTTPS 下载（Content-Length 缺失时 total 为 None）。
fn http_download(
    url: &str,
    dest: &Path,
    on_progress: &dyn Fn(u64, Option<u64>),
) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(600))
        .call()
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let total: Option<u64> = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok());
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("创建文件失败: {e}"))?;
    let mut buf = [0u8; 256 * 1024];
    let mut read: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载中断: {e}"))?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        file.write_all(&buf[..n]).map_err(|e| format!("写入失败: {e}"))?;
        read += n as u64;
        on_progress(read, total);
    }
    Ok(())
}

/// 后台线程入口：检查 → 下载 → 解压，完成后 PostMessage 通知主线程。
pub fn spawn_model_download(hwnd: isize) {
    let notify = NotifyHwnd(hwnd);
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let root = models_root();
            std::fs::create_dir_all(&root).map_err(|e| format!("创建模型目录失败: {e}"))?;

            // VAD（约 0.6MB）
            let vad = vad_path(&root);
            if !vad.is_file() {
                emit(notify, false, true, "正在下载 VAD 模型（0.6MB）…".into());
                http_download(VAD_URL, &vad, &|_, _| {})?;
            }

            // 识别模型（约 14MB 压缩包）
            let dir = asr_dir(&root);
            if !dir.join("tokens.txt").is_file() {
                let archive = root.join(format!("{MODEL_SUBDIR}.tar.bz2"));
                let partial = root.join(format!("{MODEL_SUBDIR}.tar.bz2.partial"));
                let last_pct = std::cell::Cell::new(-1i32);
                emit(notify, false, true, "正在下载识别模型（14MB）…".into());
                http_download(ASR_URL, &partial, &|read, total| {
                    if let Some(total) = total {
                        if total > 0 {
                            let pct = (read * 100 / total) as i32;
                            if pct != last_pct.get() {
                                last_pct.set(pct);
                                emit(
                                    notify,
                                    false,
                                    true,
                                    format!("正在下载识别模型… {pct}%"),
                                );
                            }
                        }
                    }
                })?;
                std::fs::rename(&partial, &archive)
                    .map_err(|e| format!("落盘失败: {e}"))?;
                if std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0) < 1024 * 1024 {
                    return Err("下载的模型压缩包过小，内容可能不完整".into());
                }
                emit(notify, false, true, "正在解压识别模型…".into());
                let file = std::fs::File::open(&archive).map_err(|e| format!("打开压缩包失败: {e}"))?;
                let bz = bzip2_rs::DecoderReader::new(file);
                tar::Archive::new(bz)
                    .unpack(&root)
                    .map_err(|e| format!("解压失败: {e}"))?;
                let _ = std::fs::remove_file(&archive);
            }
            Ok(())
        })();

        match result {
            Ok(()) => emit(notify, true, true, "语音模型下载完成，按 F9 即可开始使用".into()),
            Err(e) => emit(notify, true, false, format!("模型下载失败：{e}")),
        }
    });
}

pub fn take_model_event() -> Option<ModelEvent> {
    MODEL_STATE.lock().ok()?.take()
}
