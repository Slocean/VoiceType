//! 热更新（参照 Nexuz 的整包替换机制）：
//! 1. 更新通道：仓库根 `app_update.json`（版本 + 更新公告），远端优先、exe 旁本地兜底；
//! 2. 检查：通道最新版本 > 内置版本即有更新，按安装形态（便携/安装版）从
//!    GitHub Release tag 里解析对应资产（裸 exe / 安装包）及其 .sha256 清单；
//! 3. 下载：校验 SHA-256 后存为 `*_update.exe` + 校验记录 json（半成品用 .partial 落盘）；
//! 4. 应用：Windows 锁住正在运行的 exe，所以交给 PowerShell 助手——等本进程退出后
//!    （便携）删旧改名再启动 /（安装版）静默跑新安装包，由安装包完成替换并重启。
//!
//! 便携版与安装版的区分：安装版目录里有 Inno Setup 写入的 `install.mode` 标记。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::WM_UPDATE_EVENT;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const GITHUB_OWNER: &str = "Slocean";
const GITHUB_REPO: &str = "VoiceType";
/// 更新通道：版本与更新公告的唯一事实来源（main 分支直读，绕过 Release 缓存）。
const APP_UPDATE_URL: &str =
    "https://raw.githubusercontent.com/Slocean/VoiceType/main/app_update.json";
const RELEASES_PAGE_URL: &str = "https://github.com/Slocean/VoiceType/releases";
const USER_AGENT: &str = concat!("VoiceType/", env!("CARGO_PKG_VERSION"));

/// 便携版热更资产：裸 exe（模型单独下载，不进热更范围）。
const PORTABLE_ASSET: &str = "voicetype-tray.exe";
/// 安装版热更资产：Inno Setup 安装包。
const INSTALLER_ASSET: &str = "voicetype-setup-x64.exe";
const VERIFY_METADATA_NAME: &str = "voicetype_update.verify.json";
/// 安装版标记（由安装包写入 exe 目录）。
const INSTALL_MODE_MARKER: &str = "install.mode";
const MIN_ASSET_BYTES: u64 = 100 * 1024;

// ---------- 更新通道 ----------

#[derive(Deserialize)]
struct Channel {
    #[serde(default)]
    history: Vec<ChannelEntry>,
}

#[derive(Deserialize)]
struct ChannelEntry {
    #[serde(default)]
    version: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    /// 更新公告标题与正文（通道里该版本的 entry）。
    pub title: String,
    pub body: String,
    pub download_url: String,
    pub checksum_url: String,
    pub mode: UpdateMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    Portable,
    Installer,
}

impl UpdateMode {
    fn detect(exe_dir: &Path) -> Self {
        if exe_dir.join(INSTALL_MODE_MARKER).is_file() {
            UpdateMode::Installer
        } else {
            UpdateMode::Portable
        }
    }

    fn asset_name(self) -> &'static str {
        match self {
            UpdateMode::Portable => PORTABLE_ASSET,
            UpdateMode::Installer => INSTALLER_ASSET,
        }
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn version_tuple(text: &str) -> Vec<u32> {
    let t = text.trim().trim_start_matches('v');
    t.split('.')
        .map(|p| p.trim().parse().unwrap_or(0))
        .collect()
}

fn version_gt(remote: &str, local: &str) -> bool {
    version_tuple(remote) > version_tuple(local)
}

fn http_get(url: &str, timeout: std::time::Duration) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(timeout)
        .call()
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let mut body = Vec::new();
    resp.into_reader()
        .take(512 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取响应失败: {e}"))?;
    Ok(body)
}

/// 远端通道优先，exe 旁的本地 app_update.json 兜底（离线/断网时仍有公告可看）。
fn fetch_channel() -> Result<Channel, String> {
    match http_get(APP_UPDATE_URL, std::time::Duration::from_secs(15)) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            match serde_json::from_str::<Channel>(&text) {
                Ok(ch) => return Ok(ch),
                Err(_) => {} // 远端格式异常，走本地兜底
            }
        }
        Err(_) => {} // 断网/超时，走本地兜底
    }
    if let Some(dir) = exe_dir() {
        let path = dir.join("app_update.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(ch) = serde_json::from_str::<Channel>(&text) {
                return Ok(ch);
            }
        }
    }
    Err("无法读取更新通道（远端与本地均失败）".into())
}

/// 从 Release tag 的资产列表里解析当前形态的下载地址与校验清单地址。
fn resolve_assets(release_json: &[u8], mode: UpdateMode) -> Result<(String, String), String> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct Release {
        #[serde(default)]
        assets: Vec<Asset>,
    }
    let release: Release = serde_json::from_slice(release_json)
        .map_err(|e| format!("Release 信息解析失败: {e}"))?;
    let asset_name = mode.asset_name();
    let checksum_name = format!("{asset_name}.sha256");
    let mut checksum_url = None;
    let mut asset_url = None;
    for a in &release.assets {
        if a.name.eq_ignore_ascii_case(asset_name) {
            asset_url = Some(a.browser_download_url.clone());
        } else if a.name.eq_ignore_ascii_case(&checksum_name) {
            checksum_url = Some(a.browser_download_url.clone());
        }
    }
    match (asset_url, checksum_url) {
        (Some(u), Some(c)) => {
            validate_github_url(&u)?;
            validate_github_url(&c)?;
            Ok((u, c))
        }
        _ => Err(format!(
            "Release 缺少 {asset_name}（或其 .sha256 清单），打包可能仍在进行，请稍后再试"
        )),
    }
}

/// 只接受本仓库的 GitHub HTTPS Release 资源，防更新地址被劫持到第三方。
fn validate_github_url(url: &str) -> Result<(), String> {
    let prefix = format!(
        "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/download/"
    );
    if url.starts_with(&prefix) {
        Ok(())
    } else {
        Err("更新地址不是本仓库的 GitHub Release 资源，已拒绝".into())
    }
}

// ---------- 检查 / 下载 / 应用 ----------

pub fn check_update() -> Result<CheckResult, String> {
    let dir = exe_dir().ok_or("无法定位 exe 目录")?;
    let mode = UpdateMode::detect(&dir);
    let channel = fetch_channel()?;
    let Some(entry) = channel.history.first() else {
        return Err("更新通道为空".into());
    };
    let latest = entry.version.trim().trim_start_matches('v').to_string();
    let available = version_gt(&latest, APP_VERSION);
    let (download_url, checksum_url) = if available {
        let api = format!(
            "https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tags/v{latest}"
        );
        let release = http_get(&api, std::time::Duration::from_secs(20))?;
        resolve_assets(&release, mode)?
    } else {
        (String::new(), String::new())
    };
    Ok(CheckResult {
        update_available: available,
        current_version: APP_VERSION.to_string(),
        latest_version: latest,
        title: entry.title.clone(),
        body: entry.body.clone(),
        download_url,
        checksum_url,
        mode,
    })
}

/// 拉取更新公告（不比对版本，菜单「更新公告」用）。
pub fn fetch_announcement() -> Result<(String, String), String> {
    let channel = fetch_channel()?;
    let Some(entry) = channel.history.first() else {
        return Err("更新通道为空".into());
    };
    let title = if entry.title.is_empty() {
        "更新公告".to_string()
    } else {
        entry.title.clone()
    };
    let body = if entry.body.is_empty() {
        "暂无公告内容".to_string()
    } else {
        entry.body.clone()
    };
    Ok((title, body))
}

fn parse_checksum_manifest(bytes: &[u8], asset_name: &str) -> Result<String, String> {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(filename)) = (parts.next(), parts.next()) else {
            continue;
        };
        if digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
            && Path::new(filename)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(asset_name))
        {
            return Ok(digest.to_lowercase());
        }
    }
    Err(format!(".sha256 清单里没有 {asset_name} 的有效记录"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
    }
    out
}

/// 下载 + SHA-256 校验，成功后落盘 `*_update.exe` 与校验记录。
pub fn download_update(check: &CheckResult) -> Result<PathBuf, String> {
    let dir = exe_dir().ok_or("无法定位 exe 目录")?;
    let asset_name = check.mode.asset_name();
    let partial = dir.join(format!("{asset_name}.partial"));
    let dest = dir.join(format!(
        "{}_update.exe",
        asset_name.trim_end_matches(".exe")
    ));

    let expected = parse_checksum_manifest(
        &http_get(&check.checksum_url, std::time::Duration::from_secs(30))?,
        asset_name,
    )?;
    let blob = http_get(&check.download_url, std::time::Duration::from_secs(600))?;
    let size = blob.len() as u64;
    if size < MIN_ASSET_BYTES {
        let _ = std::fs::remove_file(&partial);
        return Err("下载文件过小，可能不是有效的更新包".into());
    }
    let actual = sha256_hex(&blob);
    if actual != expected {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("SHA-256 校验失败，已拒绝更新（期望 {expected}，实际 {actual}）"));
    }
    std::fs::write(&partial, &blob).map_err(|e| format!("写入更新包失败: {e}"))?;
    if dest.exists() {
        let _ = std::fs::remove_file(&dest);
    }
    std::fs::rename(&partial, &dest).map_err(|e| format!("更新包落盘失败: {e}"))?;
    let meta = serde_json::json!({
        "version": check.latest_version,
        "sha256": expected,
        "mode": if check.mode == UpdateMode::Installer { "installer" } else { "portable" },
    });
    std::fs::write(
        dir.join(VERIFY_METADATA_NAME),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("写入校验记录失败: {e}"))?;
    Ok(dest)
}

fn ps_escape(path: &str) -> String {
    path.replace('\'', "''")
}

/// 生成并拉起 PowerShell 更新助手，然后返回（调用方随后退出进程）。
pub fn apply_update_and_restart() -> Result<(), String> {
    let dir = exe_dir().ok_or("无法定位 exe 目录")?;
    let meta_path = dir.join(VERIFY_METADATA_NAME);
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&meta_path).map_err(|_| "未找到已下载的更新包，请先检查更新".to_string())?,
    )
    .map_err(|_| "校验记录损坏，请删除后重新下载".to_string())?;
    let mode = match meta.get("mode").and_then(|v| v.as_str()) {
        Some("installer") => UpdateMode::Installer,
        _ => UpdateMode::Portable,
    };
    let asset_name = mode.asset_name();
    let update_path = dir.join(format!(
        "{}_update.exe",
        asset_name.trim_end_matches(".exe")
    ));
    if !update_path.is_file() {
        return Err("未找到已下载的更新包，请重新检查更新".into());
    }
    // 再校验一次落盘文件，防止下载后被篡改。
    let expected = meta
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or("校验记录缺少 sha256")?;
    let actual = sha256_hex(&std::fs::read(&update_path).map_err(|e| format!("读取更新包失败: {e}"))?);
    if actual != expected.to_lowercase() {
        return Err("更新包校验失败，已拒绝应用".into());
    }

    let target = std::env::current_exe().map_err(|e| format!("无法定位当前 exe: {e}"))?;
    let pid = std::process::id();
    let work = ps_escape(&dir.to_string_lossy());
    let upd = ps_escape(&update_path.to_string_lossy());
    let tgt = ps_escape(&target.to_string_lossy());
    let tgt_name = ps_escape(&target.file_name().unwrap_or_default().to_string_lossy());
    let log = ps_escape(&dir.join("voicetype_update.log").to_string_lossy());
    let verify_meta = ps_escape(&meta_path.to_string_lossy());

    // 安装版：静默重装（Inno 会关掉正在运行的实例、覆盖文件并按 [Run] 重启应用）。
    let install_step = format!(
        "Start-Process -FilePath $upd -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/CLOSEAPPLICATIONS','/RESTARTAPPLICATIONS' -WorkingDirectory $work"
    );
    // 便携版：等本进程退出 → 删旧 exe → 更新包改名 → 重启。
    let portable_step = r#"
$exited = $false
for ($i = 0; $i -lt 120; $i++) {
  if (-not (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue)) { $exited = $true; break }
  Start-Sleep -Milliseconds 500
}
if (-not $exited) { Write-Log "等待退出超时"; exit 1 }
Start-Sleep -Milliseconds 600
for ($i = 0; $i -lt 40; $i++) {
  try {
    if (Test-Path -LiteralPath $tgt) { Remove-Item -LiteralPath $tgt -Force }
    Rename-Item -LiteralPath $upd -NewName $tgtName -Force
    $ok = $true; break
  } catch { Write-Log ("rename try $($i+1): " + $_.Exception.Message); Start-Sleep -Milliseconds 500 }
}
if ($ok) {
  Remove-Item -LiteralPath $verifyMeta -Force -ErrorAction SilentlyContinue
  Start-Process -FilePath $tgt -WorkingDirectory $work
} else {
  Write-Log "旧版替换失败，直接启动下载的新版"
  Start-Process -FilePath $upd -WorkingDirectory $work
}
"#
    .to_string();

    let script = format!(
        r#"
$ErrorActionPreference = 'Continue'
$pidToWait = {pid}
$upd = '{upd}'
$tgt = '{tgt}'
$tgtName = '{tgt_name}'
$work = '{work}'
$log = '{log}'
$verifyMeta = '{verify_meta}'
$appMode = '{mode}'

function Write-Log([string]$msg) {{
  try {{ Add-Content -LiteralPath $log -Value ("[{{0}}] {{1}}" -f (Get-Date -Format o), $msg) -Encoding UTF8 }} catch {{}}
}}

try {{
  Unblock-File -LiteralPath $upd -ErrorAction SilentlyContinue
  if ($appMode -eq 'installer') {{
    Remove-Item -LiteralPath $verifyMeta -Force -ErrorAction SilentlyContinue
    Write-Log "run installer silently"
    {install_step}
  }} else {{{portable_step}
  }}
  Write-Log "done"
}} catch {{
  Write-Log ("failed: " + $_.Exception.Message)
}}
"#,
        pid = pid,
        upd = upd,
        tgt = tgt,
        tgt_name = tgt_name,
        work = work,
        log = log,
        verify_meta = verify_meta,
        mode = if mode == UpdateMode::Installer { "installer" } else { "portable" },
        install_step = install_step,
        portable_step = portable_step,
    );

    let ps1 = std::env::temp_dir().join(format!("voicetype_apply_update_{pid}.ps1"));
    let vbs = std::env::temp_dir().join(format!("voicetype_apply_update_{pid}.vbs"));
    std::fs::write(&ps1, script).map_err(|e| format!("写入更新助手脚本失败: {e}"))?;
    let ps_exe = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()))
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let vbs_body = format!(
        "Set sh = CreateObject(\"WScript.Shell\")\r\nsh.Run \"\"\"{}\"\" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"\"{}\"\"\", 0, False\r\n",
        ps_exe.display(),
        ps1.display()
    );
    std::fs::write(&vbs, vbs_body).map_err(|e| format!("写入启动器失败: {e}"))?;
    std::process::Command::new("wscript.exe")
        .args(["//B", "//Nologo"])
        .arg(&vbs)
        .spawn()
        .map_err(|e| format!("无法启动更新助手: {e}"))?;
    Ok(())
}

/// 更新后台任务的完成事件（主线程经 take_update_event 取走后弹窗处理）。
pub struct UpdateEvent {
    pub ok: bool,
    /// true = 新版已下载校验完毕，主线程询问后可 apply_update_and_restart。
    pub ready: bool,
    pub text: String,
}

/// 可跨线程发送的窗口句柄包装，用于唤醒主线程消费更新事件。
#[derive(Clone, Copy)]
struct NotifyHwnd(isize);
unsafe impl Send for NotifyHwnd {}
impl NotifyHwnd {
    fn post(self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.0 as *mut _)),
                WM_UPDATE_EVENT,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub static UPDATE_STATE: Mutex<Option<UpdateEvent>> = Mutex::new(None);

/// 后台线程入口：检查 →（有更新则）下载校验，完成后 PostMessage 通知主线程弹窗。
pub fn spawn_check_and_download(hwnd: isize) {
    let notify = NotifyHwnd(hwnd);
    std::thread::spawn(move || {
        let event = match check_update() {
            Ok(check) if !check.update_available => UpdateEvent {
                ok: true,
                ready: false,
                text: format!(
                    "已是最新版本 v{}\n\n—— 最新公告 ——\n{}",
                    check.current_version,
                    if check.body.is_empty() { "（暂无）" } else { &check.body }
                ),
            },
            Ok(check) => match download_update(&check) {
                Ok(_) => UpdateEvent {
                    ok: true,
                    ready: true,
                    text: format!(
                        "发现新版本 v{}（当前 v{}）\n\n{}\n\n{}",
                        check.latest_version,
                        check.current_version,
                        check.title,
                        check.body
                    ),
                },
                Err(e) => UpdateEvent {
                    ok: false,
                    ready: false,
                    text: format!("下载更新失败：{e}\n\n可到 {RELEASES_PAGE_URL} 手动下载"),
                },
            },
            Err(e) => UpdateEvent {
                ok: false,
                ready: false,
                text: format!("检查更新失败：{e}"),
            },
        };
        if let Ok(mut st) = UPDATE_STATE.lock() {
            *st = Some(event);
        }
        notify.post();
    });
}

/// 后台线程入口：仅拉取最新公告。
pub fn spawn_fetch_announcement(hwnd: isize) {
    let notify = NotifyHwnd(hwnd);
    std::thread::spawn(move || {
        let event = match fetch_announcement() {
            Ok((title, body)) => UpdateEvent {
                ok: true,
                ready: false,
                text: format!("{title}\n\n{body}"),
            },
            Err(e) => UpdateEvent {
                ok: false,
                ready: false,
                text: format!("获取公告失败：{e}"),
            },
        };
        if let Ok(mut st) = UPDATE_STATE.lock() {
            *st = Some(event);
        }
        notify.post();
    });
}

pub fn take_update_event() -> Option<UpdateEvent> {
    UPDATE_STATE.lock().ok()?.take()
}
