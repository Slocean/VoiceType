//! 模型状态契约与事务修复（工作包 A）。
//!
//! 全部"模型是否可用"的判断收敛到 [`ModelStatus`] 与 inspect/repair 两条路径；
//! UI、录音入口、自检不再自行拼装文件存在性检查。状态语义：
//!
//! - [`ModelStatus::Checking`] —— 校验进行中，旧结果不得当作新结论展示；
//! - [`ModelStatus::Ready`] —— 文件存在且长度/SHA-256 与清单匹配（含实际目录与模型代次）；
//! - [`ModelStatus::Missing`] —— 当前模型至少一个文件不存在；
//! - [`ModelStatus::Corrupt`] —— 文件存在但长度或内容与清单不符（含具体文件与原因）；
//! - [`ModelStatus::PermissionDenied`] —— 目录不可写且修复失败；
//! - [`ModelStatus::Repairing`] —— 后台修复进行中；
//! - [`ModelStatus::Unavailable`] —— 无法确定模型目录或修复锁等待超时。
//!
//! 修复是事务：临时文件 → 写入 → sync → 哈希校验 → 备份旧文件 → 替换 → 失败回滚，
//! 任何一步失败都不破坏旧文件；目录级锁文件防止两个进程互相删除文件。
//! 文件操作经 [`FsOps`] 注入，测试可模拟创建/写入/同步/校验/替换/清理各步失败。

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::WM_SETTINGS_READY;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// 当前模型子目录（应用内布局：models\<子目录>\*.onnx，VAD 在根）。
pub const MODEL_SUBDIR: &str = "sherpa-onnx-streaming-zipformer-small-bilingual-zh-en-2023-02-16";
/// 旧版单语模型子目录，仅作兼容回退，当前模型优先。
pub const LEGACY_MODEL_SUBDIR: &str = "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23";

/// 当前模型四个识别文件（VAD 单独在根目录）。
const CURRENT_FILE_NAMES: [&str; 4] = [
    "encoder-epoch-99-avg-1.int8.onnx",
    "decoder-epoch-99-avg-1.int8.onnx",
    "joiner-epoch-99-avg-1.int8.onnx",
    "tokens.txt",
];
const VAD_FILE_NAME: &str = "silero_vad.onnx";
/// 目录级修复锁文件名（位于模型根目录）。
const LOCK_FILE_NAME: &str = ".voicetype-repair.lock";
/// 事务临时文件/备份后缀；扫描模型文件时这两种文件不可能是模型。
const TMP_SUFFIX: &str = ".voicetype-tmp";
const BAK_SUFFIX: &str = ".voicetype-bak";
/// 第二个进程等待修复锁的默认时长；等待后复核结果而不是动手删文件。
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

// ---------- 内嵌模型源 ----------

#[cfg(feature = "embedded-models")]
mod embedded {
    pub const ENCODER: &[u8] =
        include_bytes!("../embedded-models/encoder-epoch-99-avg-1.int8.onnx");
    pub const DECODER: &[u8] =
        include_bytes!("../embedded-models/decoder-epoch-99-avg-1.int8.onnx");
    pub const JOINER: &[u8] = include_bytes!("../embedded-models/joiner-epoch-99-avg-1.int8.onnx");
    pub const TOKENS: &[u8] = include_bytes!("../embedded-models/tokens.txt");
    pub const VAD: &[u8] = include_bytes!("../embedded-models/silero_vad.onnx");
}

// ---------- 文件操作注入 ----------

/// 失败类别：修复失败后用于区分 PermissionDenied 与一般错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsErrorKind {
    Access,
    Other,
}

#[derive(Debug, Clone)]
pub struct FsError {
    pub kind: FsErrorKind,
    pub message: String,
}

impl FsError {
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            kind: FsErrorKind::Other,
            message: message.into(),
        }
    }
    #[allow(dead_code)] // 生产路径由 from_io 构造；测试注入用
    pub fn access(message: impl Into<String>) -> Self {
        Self {
            kind: FsErrorKind::Access,
            message: message.into(),
        }
    }
    fn from_io(prefix: &str, e: &std::io::Error) -> Self {
        let kind = if e.kind() == std::io::ErrorKind::PermissionDenied {
            FsErrorKind::Access
        } else {
            FsErrorKind::Other
        };
        Self {
            kind,
            message: format!("{prefix}: {e}"),
        }
    }
}

/// 模型目录的文件操作接口。生产用 [`RealFs`]，测试注入失败与内存文件系统。
pub trait FsOps: Send + Sync {
    fn exists(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
    fn file_len(&self, path: &Path) -> Option<u64>;
    fn create_dir_all(&self, path: &Path) -> Result<(), FsError>;
    /// create_new 语义：已存在即失败（修复锁与临时文件都靠它保证独占）。
    fn create_new(&self, path: &Path) -> Result<(), FsError>;
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError>;
    fn sync(&self, path: &Path) -> Result<(), FsError>;
    /// 流式 SHA-256（十六进制小写）。
    fn hash(&self, path: &Path) -> Result<String, FsError>;
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError>;
    fn remove(&self, path: &Path) -> Result<(), FsError>;
    /// 列出目录内的文件名（不含子目录）。
    fn read_dir_files(&self, dir: &Path) -> Result<Vec<String>, FsError>;
}

pub struct RealFs;

impl FsOps for RealFs {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn file_len(&self, path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok().map(|m| m.len())
    }
    fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        std::fs::create_dir_all(path).map_err(|e| FsError::from_io("创建目录失败", &e))
    }
    fn create_new(&self, path: &Path) -> Result<(), FsError> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| ())
            .map_err(|e| FsError::from_io("创建文件失败", &e))
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        std::fs::write(path, bytes).map_err(|e| FsError::from_io("写入文件失败", &e))
    }
    fn sync(&self, path: &Path) -> Result<(), FsError> {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|f| f.sync_all())
            .map_err(|e| FsError::from_io("同步文件失败", &e))
    }
    fn hash(&self, path: &Path) -> Result<String, FsError> {
        use std::io::Read;
        let mut file =
            std::fs::File::open(path).map_err(|e| FsError::from_io("读取文件失败", &e))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|e| FsError::from_io("读取文件失败", &e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(hex_lower(&hasher.finalize()))
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        std::fs::rename(from, to).map_err(|e| FsError::from_io("替换文件失败", &e))
    }
    fn remove(&self, path: &Path) -> Result<(), FsError> {
        std::fs::remove_file(path).map_err(|e| FsError::from_io("删除文件失败", &e))
    }
    fn read_dir_files(&self, dir: &Path) -> Result<Vec<String>, FsError> {
        let rd = std::fs::read_dir(dir).map_err(|e| FsError::from_io("读取目录失败", &e))?;
        Ok(rd
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect())
    }
}

fn hex_lower(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

// ---------- 状态契约 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    /// 当前内置模型（清单校验）。
    Current,
    /// 旧版模型，仅兼容回退；最终以录音时的真实引擎初始化为准。
    Legacy,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelStatus {
    Checking,
    /// dir 为实际使用的识别模型目录。
    Ready {
        dir: PathBuf,
        kind: ModelKind,
    },
    Missing {
        dir: PathBuf,
        missing: Vec<String>,
    },
    Corrupt {
        dir: PathBuf,
        file: PathBuf,
        reason: String,
    },
    PermissionDenied {
        dir: PathBuf,
        detail: String,
    },
    Repairing {
        dir: PathBuf,
    },
    Unavailable {
        message: String,
    },
}

impl ModelStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, ModelStatus::Ready { .. })
    }

    /// 用户可见的一句话状态。
    pub fn summary(&self) -> String {
        match self {
            ModelStatus::Checking => "正在检查模型…".into(),
            ModelStatus::Ready { dir, kind } => format!(
                "模型已就绪（{}）：{}",
                match kind {
                    ModelKind::Current => "当前版本",
                    ModelKind::Legacy => "旧版本兼容",
                },
                dir.display()
            ),
            ModelStatus::Missing { dir, missing } => {
                format!("模型缺失：{} 缺少 {}", dir.display(), missing.join(", "))
            }
            ModelStatus::Corrupt { file, reason, .. } => {
                format!("模型损坏：{}（{reason}）", file.display())
            }
            ModelStatus::PermissionDenied { dir, detail } => {
                format!("模型目录不可写：{}（{detail}）", dir.display())
            }
            ModelStatus::Repairing { dir } => format!("正在修复模型：{}", dir.display()),
            ModelStatus::Unavailable { message } => message.clone(),
        }
    }
}

/// 检查深度：Quick 只做存在性/长度（录音入口，主线程可承担）；
/// Full 追加 SHA-256 内容校验（启动/手动检查/修复后，后台线程执行）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum InspectLevel {
    Quick,
    Full,
}

/// 当前模型的一个文件规格；bytes 有值时按内容校验（内嵌构建）。
#[derive(Clone)]
struct FileSpec {
    name: &'static str,
    /// 相对模型根的子目录；空串表示直接在根（VAD）。
    subdir: &'static str,
    bytes: Option<Cow<'static, [u8]>>,
}

impl FileSpec {
    fn expected_len(&self) -> Option<u64> {
        self.bytes.as_ref().map(|b| b.len() as u64)
    }
    fn expected_hash(&self) -> Option<String> {
        self.bytes.as_deref().map(sha256_hex)
    }
}

/// 扫描得到的识别模型文件路径（构造引擎配置用）。
pub struct AsrFilePaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

// ---------- 模型管理器 ----------

pub struct ModelManager {
    fs: Box<dyn FsOps>,
    /// 当前模型的文件规格（embedded 构建带期望内容；开发构建仅文件名）。
    specs: Vec<FileSpec>,
    /// 显式模型目录（VOICETYPE_MODEL_DIR）；构造时读一次，测试直接注入。
    explicit: Option<PathBuf>,
    /// 默认候选根（exe 旁 > LOCALAPPDATA > 工作目录）；测试直接注入。
    defaults: Vec<PathBuf>,
    /// 进程内防抖：后台检查/修复同时只跑一个。
    busy: AtomicBool,
    status: Mutex<ModelStatus>,
    /// 本次运行确认可用的模型根（解压/修复目标），后续检查优先。
    preferred_root: Mutex<Option<PathBuf>>,
    /// 主窗口句柄；后台任务完成时投递 WM_SETTINGS_READY 刷新界面。
    notify_hwnd: AtomicIsize,
    /// 第二进程等待修复锁的时间（测试注入缩短）。
    lock_wait: Mutex<std::time::Duration>,
}

impl ModelManager {
    fn with_fs(fs: Box<dyn FsOps>, specs: Vec<FileSpec>) -> Self {
        Self {
            fs,
            specs,
            explicit: None,
            defaults: Vec::new(),
            busy: AtomicBool::new(false),
            status: Mutex::new(ModelStatus::Checking),
            preferred_root: Mutex::new(None),
            notify_hwnd: AtomicIsize::new(0),
            lock_wait: Mutex::new(LOCK_WAIT),
        }
    }

    /// 生产构造：内嵌模型清单（embedded 构建带内容校验，开发构建按文件名），
    /// 显式目录与默认目录在构造时解析一次。
    pub fn new_real() -> Self {
        let explicit = std::env::var("VOICETYPE_MODEL_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let defaults = [
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join("models"))),
            std::env::var("LOCALAPPDATA")
                .ok()
                .map(|base| PathBuf::from(base).join("VoiceType").join("models")),
            Some(PathBuf::from("models")),
        ]
        .into_iter()
        .flatten()
        .collect();
        #[cfg(feature = "embedded-models")]
        let specs = vec![
            FileSpec {
                name: CURRENT_FILE_NAMES[0],
                subdir: MODEL_SUBDIR,
                bytes: Some(Cow::Borrowed(embedded::ENCODER)),
            },
            FileSpec {
                name: CURRENT_FILE_NAMES[1],
                subdir: MODEL_SUBDIR,
                bytes: Some(Cow::Borrowed(embedded::DECODER)),
            },
            FileSpec {
                name: CURRENT_FILE_NAMES[2],
                subdir: MODEL_SUBDIR,
                bytes: Some(Cow::Borrowed(embedded::JOINER)),
            },
            FileSpec {
                name: CURRENT_FILE_NAMES[3],
                subdir: MODEL_SUBDIR,
                bytes: Some(Cow::Borrowed(embedded::TOKENS)),
            },
            FileSpec {
                name: VAD_FILE_NAME,
                subdir: "",
                bytes: Some(Cow::Borrowed(embedded::VAD)),
            },
        ];
        #[cfg(not(feature = "embedded-models"))]
        let specs = CURRENT_FILE_NAMES
            .into_iter()
            .map(|name| FileSpec {
                name,
                subdir: MODEL_SUBDIR,
                bytes: None,
            })
            .chain(std::iter::once(FileSpec {
                name: VAD_FILE_NAME,
                subdir: "",
                bytes: None,
            }))
            .collect();
        Self {
            explicit,
            defaults,
            ..Self::with_fs(Box::new(RealFs), specs)
        }
    }

    pub fn set_notify_hwnd(&self, hwnd: isize) {
        self.notify_hwnd.store(hwnd, Ordering::SeqCst);
    }

    pub fn status(&self) -> ModelStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(ModelStatus::Unavailable {
                message: "模型状态锁损坏".into(),
            })
    }

    /// 是否有后台检查/修复在进行（UI 重复提交防护与完成判定用）。
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    fn set_status(&self, status: ModelStatus) {
        if let Ok(mut slot) = self.status.lock() {
            *slot = status;
        }
    }

    fn notify_ui(&self) {
        let hwnd = self.notify_hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    WM_SETTINGS_READY,
                    Default::default(),
                    Default::default(),
                );
            }
        }
    }

    // ---------- 检查 ----------

    /// 在目录里扫描识别模型四件套（旧模型与开发布局使用，int8 优先）。
    /// 事务临时文件/备份后缀不匹配扫描口径，不会被当作模型。
    pub fn scan_asr_files(&self, dir: &Path) -> Option<AsrFilePaths> {
        let rd = self.fs.read_dir_files(dir).ok()?;
        let pick = |prefix: &str| -> Option<PathBuf> {
            let mut names: Vec<&String> = rd
                .iter()
                .filter(|n| {
                    n.starts_with(prefix)
                        && n.ends_with(".onnx")
                        && !n.ends_with(TMP_SUFFIX)
                        && !n.ends_with(BAK_SUFFIX)
                })
                .collect();
            names.sort_by_key(|n| if n.contains(".int8.") { 0 } else { 1 });
            names.first().map(|n| dir.join(n))
        };
        let tokens = dir.join("tokens.txt");
        if !self.fs.is_file(&tokens) {
            return None;
        }
        Some(AsrFilePaths {
            encoder: pick("encoder")?,
            decoder: pick("decoder")?,
            joiner: pick("joiner")?,
            tokens,
        })
    }

    /// VAD 模型：识别目录与模型根均可。
    pub fn find_vad_model(&self, asr_dir: &Path, root: &Path) -> Option<PathBuf> {
        for dir in [asr_dir, root] {
            for name in [VAD_FILE_NAME, "vad.onnx"] {
                let p = dir.join(name);
                if self.fs.is_file(&p) {
                    return Some(p);
                }
            }
        }
        None
    }

    /// 当前模型在 root 下的识别目录；root 本身含 tokens.txt 时视为直接指向模型目录。
    fn current_asr_dir(&self, root: &Path) -> PathBuf {
        if self.fs.is_file(&root.join("tokens.txt")) {
            root.to_path_buf()
        } else {
            root.join(MODEL_SUBDIR)
        }
    }

    /// 校验当前模型（清单口径）。Ok(识别目录) 或 Err(具体状态)。
    /// 内嵌构建按"文件名+长度+SHA-256"校验；开发构建无内嵌内容，按扫描口径
    /// （encoder/decoder/joiner 前缀 + tokens.txt + VAD）校验，内容以录音时
    /// 的真实引擎初始化兜底。
    fn check_current(&self, root: &Path, level: InspectLevel) -> Result<PathBuf, ModelStatus> {
        let asr_dir = self.current_asr_dir(root);
        let has_content = self.specs.iter().any(|s| s.bytes.is_some());
        if !has_content {
            if self.scan_asr_files(&asr_dir).is_none() {
                return Err(ModelStatus::Missing {
                    dir: asr_dir,
                    missing: vec!["识别模型四件套".into()],
                });
            }
            if self.find_vad_model(&asr_dir, root).is_none() {
                return Err(ModelStatus::Missing {
                    dir: asr_dir,
                    missing: vec!["VAD 模型".into()],
                });
            }
            return Ok(asr_dir);
        }
        let mut missing = Vec::new();
        for spec in &self.specs {
            let path = if spec.subdir.is_empty() {
                root.join(spec.name)
            } else {
                asr_dir.join(spec.name)
            };
            if !self.fs.is_file(&path) {
                missing.push(spec.name.to_string());
                continue;
            }
            if let Some(len) = spec.expected_len() {
                let actual = self.fs.file_len(&path);
                if actual != Some(len) {
                    return Err(ModelStatus::Corrupt {
                        dir: asr_dir.clone(),
                        file: path,
                        reason: format!("长度不匹配（期望 {len}，实际 {actual:?}）"),
                    });
                }
            }
            if level == InspectLevel::Full {
                if let (Some(expected), Some(actual)) =
                    (spec.expected_hash(), self.fs.hash(&path).ok())
                {
                    if actual != expected {
                        return Err(ModelStatus::Corrupt {
                            dir: asr_dir.clone(),
                            file: path,
                            reason: format!("SHA-256 不匹配（期望 {expected}，实际 {actual}）"),
                        });
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Err(ModelStatus::Missing {
                dir: asr_dir,
                missing,
            });
        }
        if !self.fs.is_file(&root.join(VAD_FILE_NAME))
            && !self.fs.is_file(&asr_dir.join(VAD_FILE_NAME))
        {
            return Err(ModelStatus::Missing {
                dir: asr_dir,
                missing: vec![VAD_FILE_NAME.to_string()],
            });
        }
        Ok(asr_dir)
    }

    /// 旧模型按扫描口径存在性校验（内容以录音时的真实引擎初始化兜底）。
    fn legacy_ready(&self, root: &Path) -> Option<PathBuf> {
        let asr_dir = root.join(LEGACY_MODEL_SUBDIR);
        self.scan_asr_files(&asr_dir)?;
        self.find_vad_model(&asr_dir, root)?;
        Some(asr_dir)
    }

    // ---------- 事务修复 ----------

    fn unique_sibling(path: &Path, suffix: &str) -> PathBuf {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.with_file_name(format!("{name}.{}.{}{suffix}", std::process::id(), unique))
    }

    /// 单文件事务替换。任何一步失败都保证旧文件可读、临时文件不残留为模型。
    fn repair_file(&self, path: &Path, spec: &FileSpec) -> Result<(), FsError> {
        let bytes = spec
            .bytes
            .as_deref()
            .ok_or_else(|| FsError::other("该构建没有内嵌模型内容，无法修复"))?;
        let expected = spec.expected_hash().unwrap_or_else(|| sha256_hex(bytes));
        self.fs
            .create_dir_all(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| FsError {
                kind: e.kind,
                message: format!("create: {}", e.message),
            })?;

        let tmp = Self::unique_sibling(path, TMP_SUFFIX);
        let step = |label: &str, r: Result<(), FsError>| {
            r.map_err(|e| FsError {
                kind: e.kind,
                message: format!("{label}: {}", e.message),
            })
        };
        step("create", self.fs.create_new(&tmp))?;
        if let Err(e) =
            step("write", self.fs.write(&tmp, bytes)).and_then(|_| step("sync", self.fs.sync(&tmp)))
        {
            let _ = self.fs.remove(&tmp);
            return Err(e);
        }
        if let Err(e) = step("verify", self.fs.hash(&tmp).map(|_| ())) {
            let _ = self.fs.remove(&tmp);
            return Err(e);
        }
        if self.fs.hash(&tmp).ok().as_deref() != Some(expected.as_str()) {
            let _ = self.fs.remove(&tmp);
            return Err(FsError::other(format!(
                "verify: 临时文件 SHA-256 校验失败：{}",
                tmp.display()
            )));
        }

        // 先备份旧文件再替换；替换失败必须回滚备份，绝不让旧文件丢失。
        let backup = if self.fs.exists(path) {
            let backup = Self::unique_sibling(path, BAK_SUFFIX);
            if let Err(e) = step("backup", self.fs.rename(path, &backup)) {
                let _ = self.fs.remove(&tmp);
                return Err(e);
            }
            Some(backup)
        } else {
            None
        };
        if let Err(e) = step("replace", self.fs.rename(&tmp, path)) {
            if let Some(backup) = &backup {
                let _ = self.fs.rename(backup, path);
            }
            let _ = self.fs.remove(&tmp);
            return Err(e);
        }
        if self.fs.hash(path).ok().as_deref() != Some(expected.as_str()) {
            if let Some(backup) = &backup {
                let _ = self.fs.rename(backup, path);
            }
            return Err(FsError::other(format!(
                "final_verify: 替换后 SHA-256 校验失败：{}",
                path.display()
            )));
        }
        if let Some(backup) = &backup {
            // 清理失败不致命：备份文件名不匹配任何模型扫描口径，只记诊断。
            if let Err(e) = self.fs.remove(backup) {
                super::diagnostics::record(
                    "WARN",
                    "models",
                    &format!("清理备份文件 {} 失败：{}", backup.display(), e.message),
                );
            }
        }
        Ok(())
    }

    /// 目录级修复锁。Ok(true)=已持锁；Ok(false)=锁被他人持有/等待后需复核；
    /// Err=目录不可用。
    fn acquire_repair_lock(&self, root: &Path) -> Result<bool, FsError> {
        let lock = root.join(LOCK_FILE_NAME);
        let wait = self.lock_wait.lock().map(|g| *g).unwrap_or(LOCK_WAIT);
        let deadline = std::time::Instant::now() + wait;
        loop {
            if self.fs.create_new(&lock).is_ok() {
                return Ok(true);
            }
            // 已存在（含他人持锁、权限异常等）：等待后复核，不强行抢删。
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn release_repair_lock(&self, root: &Path) {
        let _ = self.fs.remove(&root.join(LOCK_FILE_NAME));
    }

    /// 修复当前模型在 root 下的全部缺失/损坏文件（需已持锁）。
    fn repair_current_at(&self, root: &Path) -> Result<(), ModelStatus> {
        for spec in &self.specs {
            let path = if spec.subdir.is_empty() {
                root.join(spec.name)
            } else {
                root.join(spec.subdir).join(spec.name)
            };
            let needs_fix = if self.fs.is_file(&path) {
                if let Some(expected) = spec.expected_hash() {
                    // 有期望内容：按 SHA-256 判断（同尺寸损坏也算损坏）。
                    self.fs.hash(&path).ok().as_deref() != Some(expected.as_str())
                } else {
                    // 无期望内容（开发构建）：按长度判断。
                    matches!(spec.expected_len(), Some(len) if self.fs.file_len(&path) != Some(len))
                }
            } else {
                true
            };
            if !needs_fix {
                continue;
            }
            if let Err(e) = self.repair_file(&path, spec) {
                let dir = root.join(spec.subdir);
                return Err(match e.kind {
                    FsErrorKind::Access => ModelStatus::PermissionDenied {
                        dir,
                        detail: e.message,
                    },
                    _ => ModelStatus::Unavailable { message: e.message },
                });
            }
        }
        Ok(())
    }

    /// 在一个根目录上执行"检查→修复→复核"。
    fn repair_at_root(&self, root: &Path) -> ModelStatus {
        if let Err(e) = self.fs.create_dir_all(root) {
            return ModelStatus::PermissionDenied {
                dir: root.to_path_buf(),
                detail: format!("create: {}", e.message),
            };
        }
        // 修复进行中：界面展示 Repairing 而不是旧的检查结果。
        self.set_status(ModelStatus::Repairing {
            dir: root.to_path_buf(),
        });
        self.notify_ui();
        let locked = match self.acquire_repair_lock(root) {
            Ok(locked) => locked,
            Err(e) => {
                return ModelStatus::PermissionDenied {
                    dir: root.to_path_buf(),
                    detail: format!("create: {}", e.message),
                }
            }
        };
        if !locked {
            // 等待超时：复核现有结果，不重复动手。
            return self
                .check_current(root, InspectLevel::Full)
                .map(|dir| ModelStatus::Ready {
                    dir,
                    kind: ModelKind::Current,
                })
                .unwrap_or_else(|status| status);
        }
        let status = match self.repair_current_at(root) {
            Ok(()) => self
                .check_current(root, InspectLevel::Full)
                .map(|dir| ModelStatus::Ready {
                    dir,
                    kind: ModelKind::Current,
                })
                .unwrap_or_else(|status| status),
            Err(status) => status,
        };
        self.release_repair_lock(root);
        status
    }

    /// 同步"检查并修复"主流程。只允许后台线程或自检进程调用；
    /// 主线程不做散列、不写模型。
    pub fn run_check_and_repair(&self) -> ModelStatus {
        self.set_status(ModelStatus::Checking);
        let result = self.run_check_and_repair_inner();
        self.set_status(result.clone());
        self.notify_ui();
        result
    }

    fn run_check_and_repair_inner(&self) -> ModelStatus {
        // 显式目录：只修这里，成败都归属该目录，不改用别的目录。
        if let Some(root) = self.explicit.clone() {
            if let Ok(dir) = self.check_current(&root, InspectLevel::Full) {
                let _ = self.preferred_root.lock().map(|mut g| *g = Some(root));
                return ModelStatus::Ready {
                    dir,
                    kind: ModelKind::Current,
                };
            }
            let status = self.repair_at_root(&root);
            let _ = self.preferred_root.lock().map(|mut g| *g = Some(root));
            return status;
        }
        // 默认路径：按优先级，当前模型就绪即用；否则逐个尝试修复（不可写换下一级）；
        // 全部失败再看旧模型回退。
        let mut roots = Vec::new();
        if let Ok(Some(prepared)) = self.preferred_root.lock().map(|g| g.clone()) {
            roots.push(prepared);
        }
        for root in &self.defaults {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        let mut first_error: Option<ModelStatus> = None;
        for root in &roots {
            if let Ok(dir) = self.check_current(root, InspectLevel::Full) {
                let _ = self
                    .preferred_root
                    .lock()
                    .map(|mut g| *g = Some(root.clone()));
                return ModelStatus::Ready {
                    dir,
                    kind: ModelKind::Current,
                };
            }
            let status = self.repair_at_root(root);
            if status.is_ready() {
                let _ = self
                    .preferred_root
                    .lock()
                    .map(|mut g| *g = Some(root.clone()));
                return status;
            }
            // 权限问题换下一级目录；其余失败已是根因，不必再试。
            let is_denied = matches!(status, ModelStatus::PermissionDenied { .. });
            if first_error.is_none() {
                first_error = Some(status);
            }
            if !is_denied {
                break;
            }
        }
        for root in &roots {
            if let Some(legacy) = self.legacy_ready(root) {
                let _ = self
                    .preferred_root
                    .lock()
                    .map(|mut g| *g = Some(root.clone()));
                return ModelStatus::Ready {
                    dir: legacy,
                    kind: ModelKind::Legacy,
                };
            }
        }
        first_error.unwrap_or(ModelStatus::Unavailable {
            message: "没有可用的模型目录".into(),
        })
    }

    // ---------- 后台入口 ----------

    /// 启动/手动触发的后台"检查并修复"。进行中重复调用直接忽略。
    pub fn start_check_and_repair(&'static self) {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let manager: &'static Self = self;
        std::thread::spawn(move || {
            manager.run_check_and_repair();
            manager.busy.store(false, Ordering::SeqCst);
        });
    }

    /// 主线程可用的快速存在性检查（不散列、不写盘、不改状态）。
    /// 启动引导用它判断是否打开主界面。
    pub fn quick_ready(&self) -> bool {
        if let Some(root) = &self.explicit {
            return self.check_current(root, InspectLevel::Quick).is_ok();
        }
        let mut roots = self.defaults.clone();
        if let Ok(Some(prepared)) = self.preferred_root.lock().map(|g| g.clone()) {
            roots.insert(0, prepared);
        }
        roots.iter().any(|root| {
            self.check_current(root, InspectLevel::Quick).is_ok()
                || self.legacy_ready(root).is_some()
        })
    }

    /// 触发后台检查/修复：全局实例走后台线程；测试实例同步执行。
    fn trigger_repair(&self) {
        if let Some(global) = MANAGER.get() {
            if std::ptr::eq(global, self) {
                ModelManager::start_check_and_repair(global);
                return;
            }
        }
        let _ = self.run_check_and_repair();
    }

    /// 录音入口的快速校验（主线程，只做存在性/长度，不做散列）。
    /// 失败时触发后台修复并返回用户可见原因。
    pub fn ensure_ready_for_record(&self) -> Result<(PathBuf, ModelKind), String> {
        let status = self.status();
        let ready = match &status {
            ModelStatus::Ready {
                dir,
                kind: ModelKind::Current,
            } => {
                let prepared = self.preferred_root.lock().ok().and_then(|g| g.clone());
                let root = match prepared {
                    Some(path) => path,
                    None => match dir.parent() {
                        Some(parent) => parent.to_path_buf(),
                        None => dir.clone(),
                    },
                };
                self.check_current(&root, InspectLevel::Quick).is_ok()
            }
            ModelStatus::Ready {
                dir,
                kind: ModelKind::Legacy,
            } => {
                self.scan_asr_files(dir).is_some()
                    && self
                        .find_vad_model(dir, dir.parent().unwrap_or(Path::new(".")))
                        .is_some()
            }
            _ => false,
        };
        if ready {
            if let ModelStatus::Ready { dir, kind } = status {
                return Ok((dir, kind));
            }
        }
        match &status {
            ModelStatus::Checking => Err("模型正在检查，请稍候".into()),
            ModelStatus::Repairing { .. } => Err("模型正在修复，请稍候".into()),
            ModelStatus::PermissionDenied { dir, .. } => Err(format!(
                "模型目录不可写：{}。请检查磁盘权限，或删除 VOICETYPE_MODEL_DIR 环境变量后重启",
                dir.display()
            )),
            ModelStatus::Unavailable { message } => Err(message.clone()),
            _ => {
                self.trigger_repair();
                Err(format!("{}，已自动开始后台修复", status.summary()))
            }
        }
    }
}

// ---------- 全局实例 ----------

static MANAGER: std::sync::OnceLock<ModelManager> = std::sync::OnceLock::new();

/// 初始化进程级模型管理器（幂等）。
pub fn init_global() -> &'static ModelManager {
    MANAGER.get_or_init(ModelManager::new_real)
}

pub fn global() -> Option<&'static ModelManager> {
    MANAGER.get()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    const SUB: &str = MODEL_SUBDIR;

    /// 内存文件系统 + 可注入失败：fail_on 为事务步骤名，命中即在该步返回错误；
    /// denied 目录下一切写操作返回 Access（模拟只读目录）。
    /// 内部状态用 Arc 共享，manager 与测试各持一份句柄。
    #[derive(Clone)]
    struct FakeFs(Arc<FakeFsInner>);

    struct FakeFsInner {
        files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
        dirs: Mutex<BTreeSet<PathBuf>>,
        fail_on: Mutex<Option<&'static str>>,
        denied: Mutex<Vec<PathBuf>>,
    }

    impl FakeFs {
        fn new() -> Self {
            Self(Arc::new(FakeFsInner {
                files: Mutex::new(BTreeMap::new()),
                dirs: Mutex::new(BTreeSet::new()),
                fail_on: Mutex::new(None),
                denied: Mutex::new(Vec::new()),
            }))
        }
        fn fail_at(&self, step: &'static str) {
            *self.0.fail_on.lock().unwrap() = Some(step);
        }
        fn clear_failures(&self) {
            *self.0.fail_on.lock().unwrap() = None;
        }
        fn deny(&self, dir: &Path) {
            self.0.denied.lock().unwrap().push(dir.to_path_buf());
        }
        fn denied_at(&self, path: &Path) -> bool {
            self.0
                .denied
                .lock()
                .unwrap()
                .iter()
                .any(|d| path.starts_with(d))
        }
        fn put(&self, path: &Path, bytes: &[u8]) {
            self.0
                .files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
        }
        fn get(&self, path: &Path) -> Option<Vec<u8>> {
            self.0.files.lock().unwrap().get(path).cloned()
        }
        fn has(&self, path: &Path) -> bool {
            self.0.files.lock().unwrap().contains_key(path)
        }
        fn files_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            self.0.files.lock().unwrap().clone()
        }
        fn leftover_transaction_files(&self) -> Vec<PathBuf> {
            self.0
                .files
                .lock()
                .unwrap()
                .keys()
                .filter(|p| {
                    p.to_string_lossy().ends_with(TMP_SUFFIX)
                        || p.to_string_lossy().ends_with(BAK_SUFFIX)
                })
                .cloned()
                .collect()
        }
        fn check(&self, step: &'static str) -> Result<(), FsError> {
            if self.0.fail_on.lock().unwrap().as_deref() == Some(step) {
                return Err(FsError::other(format!("注入失败：{step}")));
            }
            Ok(())
        }
    }

    impl FsOps for FakeFs {
        fn exists(&self, path: &Path) -> bool {
            self.has(path) || self.0.dirs.lock().unwrap().contains(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.has(path)
        }
        fn file_len(&self, path: &Path) -> Option<u64> {
            self.get(path).map(|v| v.len() as u64)
        }
        fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
            self.check("create_dir")?;
            if self.denied_at(path) {
                return Err(FsError::access(format!(
                    "create_dir: {} 拒绝访问",
                    path.display()
                )));
            }
            self.0.dirs.lock().unwrap().insert(path.to_path_buf());
            Ok(())
        }
        fn create_new(&self, path: &Path) -> Result<(), FsError> {
            if self.denied_at(path) {
                return Err(FsError::access(format!(
                    "create: {} 拒绝访问",
                    path.display()
                )));
            }
            if self.has(path) {
                return Err(FsError::other("create: 已存在"));
            }
            self.put(path, b"");
            Ok(())
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
            self.check("write")?;
            if self.denied_at(path) {
                return Err(FsError::access(format!(
                    "write: {} 拒绝访问",
                    path.display()
                )));
            }
            self.0
                .files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }
        fn sync(&self, path: &Path) -> Result<(), FsError> {
            self.check("sync")?;
            let _ = path;
            Ok(())
        }
        fn hash(&self, path: &Path) -> Result<String, FsError> {
            self.check("verify")?;
            Ok(sha256_hex(&self.get(path).unwrap_or_default()))
        }
        fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
            self.check("backup")?;
            self.check("replace")?;
            if self.denied_at(from) || self.denied_at(to) {
                return Err(FsError::access("rename: 拒绝访问"));
            }
            let data = self
                .get(from)
                .ok_or_else(|| FsError::other("replace: 源不存在"))?;
            self.0.files.lock().unwrap().remove(from);
            self.0.files.lock().unwrap().insert(to.to_path_buf(), data);
            Ok(())
        }
        fn remove(&self, path: &Path) -> Result<(), FsError> {
            self.check("cleanup")?;
            if self.denied_at(path) {
                return Err(FsError::access(format!(
                    "remove: {} 拒绝访问",
                    path.display()
                )));
            }
            self.0.files.lock().unwrap().remove(path);
            Ok(())
        }
        fn read_dir_files(&self, dir: &Path) -> Result<Vec<String>, FsError> {
            Ok(self
                .0
                .files
                .lock()
                .unwrap()
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect())
        }
    }

    /// 测试规格：encoder/decoder/joiner/tokens + vad，内容各不相同。
    fn test_specs() -> Vec<FileSpec> {
        vec![
            FileSpec {
                name: CURRENT_FILE_NAMES[0],
                subdir: SUB,
                bytes: Some(Cow::Owned(b"encoder-bytes".to_vec())),
            },
            FileSpec {
                name: CURRENT_FILE_NAMES[1],
                subdir: SUB,
                bytes: Some(Cow::Owned(b"decoder-bytes".to_vec())),
            },
            FileSpec {
                name: CURRENT_FILE_NAMES[2],
                subdir: SUB,
                bytes: Some(Cow::Owned(b"joiner-bytes".to_vec())),
            },
            FileSpec {
                name: CURRENT_FILE_NAMES[3],
                subdir: SUB,
                bytes: Some(Cow::Owned(b"tokens-bytes".to_vec())),
            },
            FileSpec {
                name: VAD_FILE_NAME,
                subdir: "",
                bytes: Some(Cow::Owned(b"vad-bytes".to_vec())),
            },
        ]
    }

    fn manager_with(fs: &FakeFs, specs: Vec<FileSpec>) -> ModelManager {
        let mut m = ModelManager::with_fs(Box::new(fs.clone()), specs);
        m.lock_wait = Mutex::new(std::time::Duration::from_millis(300));
        m
    }

    fn write_valid_model(fs: &FakeFs, specs: &[FileSpec], root: &Path) {
        fs.create_dir_all(&root.join(SUB)).unwrap();
        for spec in specs {
            let path = if spec.subdir.is_empty() {
                root.join(spec.name)
            } else {
                root.join(spec.subdir).join(spec.name)
            };
            fs.write(&path, spec.bytes.as_deref().unwrap()).unwrap();
        }
    }

    fn ready_dir(status: &ModelStatus) -> PathBuf {
        match status {
            ModelStatus::Ready { dir, .. } => dir.clone(),
            other => panic!("期望 Ready，实际 {other:?}"),
        }
    }

    #[test]
    fn empty_dir_is_repaired_and_second_run_is_idempotent() {
        let fs = FakeFs::new();
        let mut m = manager_with(&fs, test_specs());
        let root = PathBuf::from("C:\\models");
        m.defaults = vec![root.clone()];
        let first = m.run_check_and_repair();
        assert!(first.is_ready(), "{first:?}");
        // 第二次运行幂等：文件集合与内容不再变化（没有新事务文件）。
        let before = fs.files_snapshot();
        let second = m.run_check_and_repair();
        assert_eq!(ready_dir(&second), ready_dir(&first));
        assert_eq!(fs.files_snapshot(), before);
    }

    #[test]
    fn missing_single_file_reports_missing_then_repairs() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs.clone());
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &specs, &root);
        m.defaults = vec![root.clone()];
        let asr = root.join(SUB);
        fs.remove(&asr.join(CURRENT_FILE_NAMES[2])).unwrap();
        // 修复前：能发现缺失；修复后：就绪且文件补齐。
        assert!(matches!(
            m.check_current(&root, InspectLevel::Full),
            Err(ModelStatus::Missing { .. })
        ));
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "{status:?}");
        assert!(fs.get(&asr.join(CURRENT_FILE_NAMES[2])).is_some());
    }

    #[test]
    fn same_size_corrupt_content_is_detected_by_hash_and_repaired() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs.clone());
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &specs, &root);
        m.defaults = vec![root.clone()];
        let asr = root.join(SUB);
        // 同尺寸损坏：长度相同、内容错误。
        fs.write(&asr.join(CURRENT_FILE_NAMES[0]), b"ENCODER-byteZ")
            .unwrap();
        assert!(matches!(
            m.check_current(&root, InspectLevel::Full),
            Err(ModelStatus::Corrupt { .. })
        ));
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "{status:?}");
        assert_eq!(
            fs.get(&asr.join(CURRENT_FILE_NAMES[0])).unwrap(),
            b"encoder-bytes".to_vec()
        );
    }

    #[test]
    fn quick_inspect_does_not_hash() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let m = manager_with(&fs, specs.clone());
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &specs, &root);
        // 同尺寸损坏：Quick 检查发现不了（长度一致），Full 才能发现。
        let asr = root.join(SUB);
        fs.write(&asr.join(CURRENT_FILE_NAMES[1]), b"DECODER-byteZ")
            .unwrap();
        assert!(m.check_current(&root, InspectLevel::Quick).is_ok());
        assert!(m.check_current(&root, InspectLevel::Full).is_err());
    }

    #[test]
    fn explicit_dir_corrupt_gives_explicit_error_and_repairs_there() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs.clone());
        let explicit = PathBuf::from("D:\\custom-models");
        let other = PathBuf::from("C:\\models");
        write_valid_model(&fs, &specs, &other); // 别处有完好模型，也不许偷偷改用
        write_valid_model(&fs, &specs, &explicit);
        let asr = explicit.join(SUB);
        fs.write(&asr.join(CURRENT_FILE_NAMES[3]), b"TOKENS-byteZ")
            .unwrap();
        m.explicit = Some(explicit.clone());
        // 检查：错误必须归属显式目录，不得回落到 C:\models。
        match m.check_current(&explicit, InspectLevel::Full) {
            Err(ModelStatus::Corrupt { dir, .. }) => assert_eq!(dir, asr),
            other => panic!("期望 Corrupt@explicit，实际 {other:?}"),
        }
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "{status:?}");
        assert_eq!(ready_dir(&status), asr);
    }

    #[test]
    fn read_only_exe_side_dir_falls_through_to_user_dir() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let exe_side = PathBuf::from("C:\\Program Files\\VoiceType\\models");
        let user_side = PathBuf::from("C:\\Users\\u\\AppData\\Local\\VoiceType\\models");
        m.defaults = vec![exe_side.clone(), user_side.clone()];
        // exe 侧存在但只读（损坏文件无法替换）：修复失败后换用户目录。
        fs.create_dir_all(&exe_side.join(SUB)).unwrap();
        fs.write(
            &exe_side.join(SUB).join(CURRENT_FILE_NAMES[0]),
            b"CORRUPted",
        )
        .unwrap();
        fs.deny(&exe_side);
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "{status:?}");
        assert_eq!(ready_dir(&status), user_side.join(SUB));
    }

    #[test]
    fn read_only_explicit_dir_reports_permission_denied_without_fallback() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let explicit = PathBuf::from("D:\\readonly-models");
        let user_side = PathBuf::from("C:\\Users\\u\\AppData\\Local\\VoiceType\\models");
        m.defaults = vec![user_side.clone()];
        fs.create_dir_all(&explicit.join(SUB)).unwrap();
        fs.write(
            &explicit.join(SUB).join(CURRENT_FILE_NAMES[0]),
            b"CORRUPted",
        )
        .unwrap();
        fs.deny(&explicit);
        m.explicit = Some(explicit.clone());
        let status = m.run_check_and_repair();
        // 显式目录不可写：给出该目录的明确错误，不得偷偷改用别的目录。
        match status {
            ModelStatus::PermissionDenied { dir, .. } => assert_eq!(dir, explicit),
            other => panic!("期望 PermissionDenied@explicit，实际 {other:?}"),
        }
    }

    #[test]
    fn write_interruption_keeps_old_file_and_leaves_no_tmp_as_model() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &test_specs(), &root);
        m.defaults = vec![root.clone()];
        // 损坏 encoder 后在 write 步骤注入失败。
        let path = root.join(SUB).join(CURRENT_FILE_NAMES[0]);
        fs.write(&path, b"CORRUPTED").unwrap();
        fs.fail_at("write");
        let status = m.run_check_and_repair();
        assert!(!status.is_ready(), "修复失败不得返回 Ready：{status:?}");
        // 旧文件仍可读（损坏但保留），没有残留的事务文件。
        assert!(fs.get(&path).is_some());
        let leftover = fs.leftover_transaction_files();
        assert!(leftover.is_empty(), "残留事务文件：{leftover:?}");
        // 解除注入后可修复。
        fs.clear_failures();
        assert!(m.run_check_and_repair().is_ready());
    }

    #[test]
    fn replace_failure_restores_backup_and_reports_failure() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &test_specs(), &root);
        m.defaults = vec![root.clone()];
        let path = root.join(SUB).join(CURRENT_FILE_NAMES[1]);
        fs.write(&path, b"CORRUPted").unwrap();
        fs.fail_at("replace");
        let status = m.run_check_and_repair();
        assert!(!status.is_ready(), "{status:?}");
        // 备份已回滚：旧文件仍在（内容为损坏后的旧内容）。
        assert_eq!(fs.get(&path).unwrap(), b"CORRUPted".to_vec());
        fs.clear_failures();
        assert!(m.run_check_and_repair().is_ready());
    }

    #[test]
    fn verify_failure_discards_tmp_and_keeps_old_file() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &test_specs(), &root);
        m.defaults = vec![root.clone()];
        // 先损坏一个文件触发修复，再让校验步骤失败。
        fs.write(&root.join(SUB).join(CURRENT_FILE_NAMES[0]), b"CORRUPted")
            .unwrap();
        fs.fail_at("verify");
        let status = m.run_check_and_repair();
        assert!(!status.is_ready(), "{status:?}");
        // 临时文件已清理。
        let tmp_left = fs
            .files_snapshot()
            .keys()
            .filter(|p| p.to_string_lossy().ends_with(TMP_SUFFIX))
            .count();
        assert_eq!(tmp_left, 0);
        fs.clear_failures();
        assert!(m.run_check_and_repair().is_ready());
    }

    #[test]
    fn cleanup_failure_still_reports_ready_and_bak_is_not_a_model() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &test_specs(), &root);
        m.defaults = vec![root.clone()];
        let path = root.join(SUB).join(CURRENT_FILE_NAMES[0]);
        fs.write(&path, b"CORRUPted").unwrap();
        fs.fail_at("cleanup");
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "清理失败不影响就绪：{status:?}");
        // 残留备份不匹配模型扫描口径，扫描仍能定位模型文件。
        assert!(m.scan_asr_files(&root.join(SUB)).is_some());
        let names =
            fs.0.files
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
        assert!(names
            .iter()
            .any(|n| n.to_string_lossy().ends_with(BAK_SUFFIX)));
    }

    #[test]
    fn target_file_in_use_reports_failure_and_keeps_old_file_readable() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs);
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &test_specs(), &root);
        m.defaults = vec![root.clone()];
        // 损坏文件被占用：backup 步骤（rename）返回错误。
        let path = root.join(SUB).join(CURRENT_FILE_NAMES[0]);
        fs.write(&path, b"CORRUPted").unwrap();
        fs.fail_at("backup");
        let status = m.run_check_and_repair();
        assert!(!status.is_ready(), "{status:?}");
        assert!(fs.get(&path).is_some(), "旧文件必须保留");
        fs.clear_failures();
        assert!(m.run_check_and_repair().is_ready());
    }

    #[test]
    fn concurrent_repair_second_process_waits_and_rechecks() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs.clone());
        let root = PathBuf::from("C:\\models");
        write_valid_model(&fs, &specs, &root);
        m.defaults = vec![root.clone()];
        let path = root.join(SUB).join(CURRENT_FILE_NAMES[0]);
        fs.write(&path, b"CORRUPted").unwrap();
        // 模拟另一进程持有修复锁：预创建锁文件。
        fs.put(&root.join(LOCK_FILE_NAME), b"other-process");
        let status = m.run_check_and_repair();
        // 等待超时后复核：仍是损坏（对方没修好），不得动手删文件，也不得误报 Ready。
        assert!(!status.is_ready(), "{status:?}");
        assert_eq!(fs.get(&path).unwrap(), b"CORRUPted".to_vec());
        // 对方修完并释放锁后，复核即就绪（我们没有再写任何模型文件）。
        fs.remove(&root.join(LOCK_FILE_NAME)).unwrap();
        fs.write(&path, b"encoder-bytes").unwrap();
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "{status:?}");
    }

    #[test]
    fn current_model_takes_priority_over_legacy_and_legacy_is_fallback() {
        let fs = FakeFs::new();
        let specs = test_specs();
        let mut m = manager_with(&fs, specs.clone());
        let root = PathBuf::from("C:\\models");
        // 只有旧模型。
        let legacy = root.join(LEGACY_MODEL_SUBDIR);
        fs.create_dir_all(&legacy).unwrap();
        for (i, name) in [
            "encoder-epoch-99-avg-1.onnx",
            "decoder-epoch-99-avg-1.onnx",
            "joiner-epoch-99-avg-1.onnx",
        ]
        .iter()
        .enumerate()
        {
            fs.write(&legacy.join(name), format!("legacy-{i}").as_bytes())
                .unwrap();
        }
        fs.write(&legacy.join("tokens.txt"), b"legacy-tokens")
            .unwrap();
        fs.write(&root.join(VAD_FILE_NAME), b"legacy-vad").unwrap();
        m.defaults = vec![root.clone()];
        // 内嵌规格存在但文件缺失：先修复当前模型，不得停在旧模型上。
        let status = m.run_check_and_repair();
        assert!(
            matches!(
                status,
                ModelStatus::Ready {
                    kind: ModelKind::Current,
                    ..
                }
            ),
            "{status:?}"
        );
        // 无内嵌内容（开发构建口径）：当前缺失、旧模型齐全 → 回退旧模型。
        let fs2 = FakeFs::new();
        let dev_specs: Vec<FileSpec> = CURRENT_FILE_NAMES
            .into_iter()
            .map(|n| FileSpec {
                name: n,
                subdir: SUB,
                bytes: None,
            })
            .chain(std::iter::once(FileSpec {
                name: VAD_FILE_NAME,
                subdir: "",
                bytes: None,
            }))
            .collect();
        let mut m2 = manager_with(&fs2, dev_specs);
        let root2 = PathBuf::from("C:\\dev-models");
        let legacy2 = root2.join(LEGACY_MODEL_SUBDIR);
        fs2.create_dir_all(&legacy2).unwrap();
        fs2.write(&legacy2.join("encoder-epoch-99-avg-1.onnx"), b"e")
            .unwrap();
        fs2.write(&legacy2.join("decoder-epoch-99-avg-1.onnx"), b"d")
            .unwrap();
        fs2.write(&legacy2.join("joiner-epoch-99-avg-1.onnx"), b"j")
            .unwrap();
        fs2.write(&legacy2.join("tokens.txt"), b"t").unwrap();
        fs2.write(&root2.join(VAD_FILE_NAME), b"v").unwrap();
        m2.defaults = vec![root2.clone()];
        let status = m2.run_check_and_repair();
        assert_eq!(
            ready_dir(&status),
            root2.join(LEGACY_MODEL_SUBDIR),
            "仅旧模型存在时应加载旧模型"
        );
    }

    #[test]
    fn tmp_files_are_never_treated_as_models() {
        let fs = FakeFs::new();
        let mut m = manager_with(&fs, test_specs());
        let root = PathBuf::from("C:\\models");
        m.defaults = vec![root.clone()];
        let asr = root.join(SUB);
        fs.create_dir_all(&asr).unwrap();
        // 伪装成 encoder 的临时文件与备份。
        fs.write(
            &asr.join("encoder-epoch-99-avg-1.int8.onnx.123.456.voicetype-tmp"),
            b"tmp",
        )
        .unwrap();
        fs.write(
            &asr.join("encoder-epoch-99-avg-1.int8.onnx.123.456.voicetype-bak"),
            b"bak",
        )
        .unwrap();
        let status = m.run_check_and_repair();
        assert!(status.is_ready(), "临时文件不得阻止修复：{status:?}");
        assert_eq!(ready_dir(&status), asr);
    }

    #[test]
    fn status_summary_is_user_readable_for_each_state() {
        let dir = PathBuf::from("C:\\models\\sub");
        assert!(!ModelStatus::Checking.summary().is_empty());
        assert!(ModelStatus::Ready {
            dir: dir.clone(),
            kind: ModelKind::Current
        }
        .summary()
        .contains("就绪"));
        assert!(ModelStatus::Missing {
            dir: dir.clone(),
            missing: vec!["a.onnx".into()]
        }
        .summary()
        .contains("a.onnx"));
        assert!(ModelStatus::Corrupt {
            dir: dir.clone(),
            file: dir.join("a.onnx"),
            reason: "SHA-256 不匹配".into()
        }
        .summary()
        .contains("损坏"));
        assert!(ModelStatus::PermissionDenied {
            dir: dir.clone(),
            detail: "Access".into()
        }
        .summary()
        .contains("不可写"));
        assert!(!ModelStatus::Unavailable {
            message: "x".into()
        }
        .summary()
        .is_empty());
    }
}
