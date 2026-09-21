# VoiceType 开发约定

## 热更新规则（便携版 / 安装版通用）

热更新参照 Nexuz 机制：更新通道宣告版本 → 应用检查比对 → 从 GitHub Release 下载资产并 SHA-256 校验 → PowerShell 助手换文件/静默重装后自动重启。

### 版本发布流程（每次发版必须完整走一遍，缺一不可）

1. **bump 代码版本**：`apps/tray/Cargo.toml` 的 `version`（应用内置版本号，热更新比对用它，更新后跑 `cargo build -p voicetype-tray` 顺带刷新 `Cargo.lock`）。
2. **写更新公告**：`app_update.json` 的 `history` **数组头部**插入新条目（`version` / `title` / `body`），`version` 与 tag 完全一致（不带 `v` 前缀）。公告 body 用 `【新增：…】【修复：…】【变更：…】` 分节，面向用户描述，不写实现细节流水账。**没有公告条目的版本不许发。**
3. **提交推送 → 打 tag → 推 tag**：`git tag vX.Y.Z && git push origin vX.Y.Z`，tag 触发 `release` 工作流（构建 → 双产物打包 → `release_notes.py` 从通道生成 Release 正文 → 发布）。
4. **盯完工作流**：CI 失败常见原因是网络类瞬时错误（sherpa-onnx 资产下载 504 等），用 `rerun-failed-jobs` 重跑即可，不要急着改代码。

### 不变量（改坏了热更新就断）

- **资产名是协议**：热更按固定文件名在 Release 里找资产——便携版 `voicetype-tray.exe`、安装版 `voicetype-setup-x64.exe`，各自必须带同名 `.sha256` 清单（`<sha256>  <文件名>` 格式，ascii 编码）。改名必须同步改 `apps/tray/src/updater.rs` 里的 `PORTABLE_ASSET` / `INSTALLER_ASSET` 和 `scripts/package.ps1`。
- **所有 Release 资产必须带 `.sha256`**：`package.ps1` 尾部统一生成，别手工绕过；`release.yml` 的 `files:` 列表与资产生成保持一一对应。
- **资产全集**（`package.ps1 -WithModels`）：标准/带模型 × 便携/安装 + 热更裸 exe 共 5 个产物。带模型变体（`*-with-models.*`）不参与热更新，只是首装选项，内嵌模型布局必须与 `setup-models.ps1` 一致（`models\<识别模型子目录>\*.onnx` + `models\silero_vad.onnx`）。
- **模型目录解析口径**：`VOICETYPE_MODEL_DIR` > exe 旁 `models\` > 工作目录 `models\`（`main.rs::resolve_model_dir` 与 `models_dl.rs::models_root` 必须同口径，改一处必改另一处）。
- **版本一致性**：tag = `app_update.json` 最新条目 = `apps/tray/Cargo.toml` 版本。tag 指向的提交里三者必须一致，否则 `release_notes.py` 找不到公告条目直接失败。
- **通道先行原则**：`app_update.json` 宣告了新版本，但对应 Release 资产还没上传完时，应用会提示「安装包尚未就绪，请稍后再试」并拒绝下载——不会回落到旧 Release，属预期行为，勿"修"。
- **安全边界不许松**：下载地址只允许 `https://github.com/Slocean/VoiceType/releases/download/` 前缀（`updater.rs::validate_github_url`）；SHA-256 双重校验（清单 + 落盘后）只许加强不许删。
- **形态标记**：安装版目录由安装包写入 `install.mode`，是 `updater.rs` 区分「便携换 exe / 安装版静默重装」的唯一依据；便携包里绝不能带这个文件。

### 发版后验证

Release 页确认 6 个资产齐全（两种安装形态 + 裸 exe + 三个 `.sha256`）且公告正文正确；真机托盘「检查更新」能发现新版并完成热更重启（首次验证便携版换 exe 路径，安装版静默重装路径每次形态改动后验一次）。

### 相关文件

| 文件 | 职责 |
| --- | --- |
| `app_update.json` | 更新通道：版本 + 更新公告（唯一事实来源） |
| `apps/tray/src/updater.rs` | 检查 / 下载校验 / 应用更新（PowerShell 助手） |
| `apps/tray/src/models_dl.rs` | 应用内一键下载模型（tar.bz2 流式解压） |
| `scripts/package.ps1` | 双形态 + 带模型变体打包 + `.sha256` 清单生成 |
| `scripts/installer.iss` | 安装版打包脚本（写 `install.mode` 标记；`/DModelsDir` 内嵌模型） |
| `scripts/release_notes.py` | 从通道生成 Release 公告正文 |
| `.github/workflows/release.yml` | tag 触发的构建发布流水线 |
