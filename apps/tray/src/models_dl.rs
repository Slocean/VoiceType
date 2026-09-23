//! 内置模型：模型文件经 include_bytes! 编译进 exe（feature = embedded-models，
//! 打包构建启用），首次启动自动解压到模型目录。不再提供在线下载——
//! 每个 exe 都自带模型，开箱即用。
//!
//! 模型：sherpa-onnx-streaming-zipformer-small-bilingual-zh-en-2023-02-16
//! （中英双语流式 int8，准确率显著优于早期 14M 单语模型）+ silero VAD。

use std::path::PathBuf;

/// 识别模型子目录名（应用内布局：models\<子目录>\*.onnx）。
pub const MODEL_SUBDIR: &str = "sherpa-onnx-streaming-zipformer-small-bilingual-zh-en-2023-02-16";

#[cfg(feature = "embedded-models")]
mod embedded {
    pub const ENCODER: &[u8] = include_bytes!("../embedded-models/encoder-epoch-99-avg-1.int8.onnx");
    pub const DECODER: &[u8] = include_bytes!("../embedded-models/decoder-epoch-99-avg-1.int8.onnx");
    pub const JOINER: &[u8] = include_bytes!("../embedded-models/joiner-epoch-99-avg-1.int8.onnx");
    pub const TOKENS: &[u8] = include_bytes!("../embedded-models/tokens.txt");
    pub const VAD: &[u8] = include_bytes!("../embedded-models/silero_vad.onnx");
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(PathBuf::from)
}

fn localappdata_models() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("VoiceType").join("models"))
}

/// 模型根目录（运行时查找的一致口径）：
/// VOICETYPE_MODEL_DIR 显式指定 > exe 旁 models\ > %LOCALAPPDATA%\VoiceType\models
/// > 工作目录 models\（仓库开发布局）。
pub fn models_root() -> PathBuf {
    if let Ok(d) = std::env::var("VOICETYPE_MODEL_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(dir) = exe_dir() {
        let p = dir.join("models");
        if p.is_dir() {
            return p;
        }
    }
    if let Some(local) = localappdata_models() {
        if local.is_dir() {
            return local;
        }
    }
    PathBuf::from("models")
}

fn vad_path(root: &std::path::Path) -> PathBuf {
    root.join("silero_vad.onnx")
}

fn asr_dir(root: &std::path::Path) -> PathBuf {
    root.join(MODEL_SUBDIR)
}

/// 识别模型四件套 + VAD 是否齐备（tokens.txt 是解压完成的标志）。
pub fn models_ready() -> bool {
    let root = models_root();
    vad_path(&root).is_file() && asr_dir(&root).join("tokens.txt").is_file()
}

#[cfg(feature = "embedded-models")]
fn write_if_missing(path: &std::path::Path, bytes: &'static [u8]) -> Result<(), String> {
    if path.is_file()
        && std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) == bytes.len() as u64
    {
        return Ok(());
    }
    let tmp = path.with_extension("download");
    std::fs::write(&tmp, bytes).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("落盘 {} 失败: {e}", path.display()))
}

/// 首次启动把内置模型解压到模型目录（幂等：已存在且大小一致的文件跳过）。
/// 目标目录优先 exe 旁 models\（便携布局），不可写时回落 %LOCALAPPDATA%\VoiceType\models。
#[cfg(feature = "embedded-models")]
pub fn ensure_models_extracted() -> Result<PathBuf, String> {
    let target = match exe_dir() {
        Some(dir) => {
            let p = dir.join("models");
            if std::fs::create_dir_all(&p).is_ok() {
                p
            } else {
                localappdata_models().ok_or("无法确定模型目录（LOCALAPPDATA 缺失）")?
            }
        }
        None => localappdata_models().ok_or("无法确定模型目录（LOCALAPPDATA 缺失）")?,
    };
    std::fs::create_dir_all(asr_dir(&target)).map_err(|e| format!("创建模型目录失败: {e}"))?;

    write_if_missing(
        &asr_dir(&target).join("encoder-epoch-99-avg-1.int8.onnx"),
        embedded::ENCODER,
    )?;
    write_if_missing(
        &asr_dir(&target).join("decoder-epoch-99-avg-1.int8.onnx"),
        embedded::DECODER,
    )?;
    write_if_missing(
        &asr_dir(&target).join("joiner-epoch-99-avg-1.int8.onnx"),
        embedded::JOINER,
    )?;
    write_if_missing(&asr_dir(&target).join("tokens.txt"), embedded::TOKENS)?;
    write_if_missing(&vad_path(&target), embedded::VAD)?;
    Ok(target)
}

#[cfg(not(feature = "embedded-models"))]
pub fn ensure_models_extracted() -> Result<PathBuf, String> {
    // 开发构建：不内嵌模型，沿用 models\ 目录（仓库布局）。
    Ok(models_root())
}
