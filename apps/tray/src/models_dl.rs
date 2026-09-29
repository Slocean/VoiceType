//! 内置模型：模型文件经 include_bytes! 编译进 exe（feature = embedded-models，
//! 打包构建启用），首次启动自动解压到模型目录。不再提供在线下载——
//! 每个 exe 都自带模型，开箱即用。
//!
//! 模型：sherpa-onnx-streaming-zipformer-small-bilingual-zh-en-2023-02-16
//! （中英双语流式 int8，准确率显著优于早期 14M 单语模型）+ silero VAD。

use std::path::PathBuf;
#[cfg(feature = "embedded-models")]
use std::sync::OnceLock;
#[cfg(any(feature = "embedded-models", test))]
use sha2::{Digest, Sha256};

/// 识别模型子目录名（应用内布局：models\<子目录>\*.onnx）。
pub const MODEL_SUBDIR: &str = "sherpa-onnx-streaming-zipformer-small-bilingual-zh-en-2023-02-16";
pub const LEGACY_MODEL_SUBDIR: &str = "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23";

#[cfg(feature = "embedded-models")]
static PREPARED_ROOT: OnceLock<PathBuf> = OnceLock::new();

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

fn configured_models_root() -> Option<PathBuf> {
    std::env::var("VOICETYPE_MODEL_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn root_has_model(root: &std::path::Path, subdir: &str) -> bool {
    let dir = if root.join("tokens.txt").is_file() {
        root.to_path_buf()
    } else {
        root.join(subdir)
    };
    super::find_model_files(&dir).is_some() && super::find_vad_model(&dir).is_some()
}

/// 模型根目录（运行时查找的一致口径）：
/// VOICETYPE_MODEL_DIR 显式指定 > exe 旁 models\ > %LOCALAPPDATA%\VoiceType\models
/// > 工作目录 models\（仓库开发布局）。
pub fn models_root() -> PathBuf {
    if let Some(root) = configured_models_root() {
        return root;
    }
    #[cfg(feature = "embedded-models")]
    if let Some(root) = PREPARED_ROOT.get() {
        return root.clone();
    }
    let beside_exe = exe_dir().map(|dir| dir.join("models"));
    let local = localappdata_models();
    let working = PathBuf::from("models");
    for candidate in [beside_exe.as_deref(), local.as_deref(), Some(working.as_path())]
        .into_iter()
        .flatten()
    {
        for subdir in [MODEL_SUBDIR, LEGACY_MODEL_SUBDIR] {
            if root_has_model(candidate, subdir) {
                return candidate.to_path_buf();
            }
        }
    }
    for candidate in [beside_exe, local, Some(working)].into_iter().flatten() {
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from("models")
}

#[cfg(feature = "embedded-models")]
fn vad_path(root: &std::path::Path) -> PathBuf {
    root.join("silero_vad.onnx")
}

#[cfg(feature = "embedded-models")]
fn asr_dir(root: &std::path::Path) -> PathBuf {
    root.join(MODEL_SUBDIR)
}

/// 与录音入口使用相同的检查：识别模型四件套和 VAD 均须存在。
pub fn models_ready() -> bool {
    let dir = super::resolve_model_dir();
    super::find_model_files(&dir).is_some() && super::find_vad_model(&dir).is_some()
}

#[cfg(any(feature = "embedded-models", test))]
fn file_matches(path: &std::path::Path, bytes: &[u8]) -> bool {
    use std::io::Read;
    if std::fs::metadata(path).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(_) => return false,
        }
    }
    hasher.finalize() == Sha256::digest(bytes)
}

#[cfg(any(feature = "embedded-models", test))]
fn write_if_missing(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if file_matches(path, bytes) {
        return Ok(());
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), unique));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("创建临时模型文件 {} 失败: {e}", tmp.display()))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("写入模型文件 {} 失败: {e}", tmp.display()))?;
        drop(file);
        // Windows 的 rename 不会覆盖已存在的目标。旧模型校验失败时先移走它，
        // 否则每次启动都会在这里失败，最终表现为“模型缺失”。
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| format!("移除损坏模型文件 {} 失败: {e}", path.display()))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("替换模型文件 {} 失败: {e}", path.display()))?;
        if !file_matches(path, bytes) {
            return Err(format!("模型文件校验失败: {}", path.display()));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(feature = "embedded-models")]
fn extract_into(target: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(asr_dir(target)).map_err(|e| format!("创建模型目录失败: {e}"))?;

    write_if_missing(
        &asr_dir(target).join("encoder-epoch-99-avg-1.int8.onnx"),
        embedded::ENCODER,
    )?;
    write_if_missing(
        &asr_dir(target).join("decoder-epoch-99-avg-1.int8.onnx"),
        embedded::DECODER,
    )?;
    write_if_missing(
        &asr_dir(target).join("joiner-epoch-99-avg-1.int8.onnx"),
        embedded::JOINER,
    )?;
    write_if_missing(&asr_dir(target).join("tokens.txt"), embedded::TOKENS)?;
    write_if_missing(&vad_path(target), embedded::VAD)?;
    Ok(())
}

/// 首次启动解压内置模型；exe 旁实际写入失败时改用用户数据目录。
#[cfg(feature = "embedded-models")]
pub fn ensure_models_extracted() -> Result<PathBuf, String> {
    if let Some(root) = configured_models_root() {
        if !root_has_model(&root, MODEL_SUBDIR) {
            extract_into(&root)?;
        }
        let _ = PREPARED_ROOT.set(root.clone());
        return Ok(root);
    }
    if let Some(root) = exe_dir().map(|dir| dir.join("models")) {
        if extract_into(&root).is_ok() {
            let _ = PREPARED_ROOT.set(root.clone());
            return Ok(root);
        }
    }
    let local = localappdata_models().ok_or("无法确定模型目录（LOCALAPPDATA 缺失）")?;
    extract_into(&local)?;
    let _ = PREPARED_ROOT.set(local.clone());
    Ok(local)
}

#[cfg(not(feature = "embedded-models"))]
pub fn ensure_models_extracted() -> Result<PathBuf, String> {
    // 开发构建：不内嵌模型，沿用 models\ 目录（仓库布局）。
    Ok(models_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_status_matches_recording_model_lookup() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("voicetype-model-test-{unique}"));
        let asr = root.join(MODEL_SUBDIR);
        std::fs::create_dir_all(&asr).unwrap();
        let previous = std::env::var_os("VOICETYPE_MODEL_DIR");
        std::env::set_var("VOICETYPE_MODEL_DIR", &root);

        assert!(!models_ready());
        let names = [
            "encoder-epoch-99-avg-1.int8.onnx",
            "decoder-epoch-99-avg-1.int8.onnx",
            "joiner-epoch-99-avg-1.int8.onnx",
            "tokens.txt",
        ];
        for name in names {
            std::fs::write(asr.join(name), b"test").unwrap();
        }
        assert!(!models_ready());
        std::fs::write(root.join("silero_vad.onnx"), b"test").unwrap();
        assert!(models_ready());
        assert_eq!(super::super::resolve_model_dir(), asr);

        let legacy = root.join(LEGACY_MODEL_SUBDIR);
        std::fs::create_dir(&legacy).unwrap();
        for name in names {
            std::fs::write(legacy.join(name), b"test").unwrap();
        }
        assert_eq!(super::super::resolve_model_dir(), asr);
        for name in names {
            std::fs::remove_file(asr.join(name)).unwrap();
        }
        assert!(models_ready());
        assert_eq!(super::super::resolve_model_dir(), legacy);

        match previous {
            Some(value) => std::env::set_var("VOICETYPE_MODEL_DIR", value),
            None => std::env::remove_var("VOICETYPE_MODEL_DIR"),
        }
        for name in names {
            std::fs::remove_file(legacy.join(name)).unwrap();
        }
        std::fs::remove_file(root.join("silero_vad.onnx")).unwrap();
        std::fs::remove_dir(asr).unwrap();
        std::fs::remove_dir(legacy).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn same_size_corrupt_model_is_repaired() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("voicetype-file-test-{unique}.onnx"));
        std::fs::write(&path, b"bad!").unwrap();
        assert!(!file_matches(&path, b"good"));
        write_if_missing(&path, b"good").unwrap();
        assert!(file_matches(&path, b"good"));
        std::fs::remove_file(path).unwrap();
    }
}
