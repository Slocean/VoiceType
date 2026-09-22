# VoiceType

独立的系统级语音输入工具（Windows 优先）。F9 全局热键触发录音，本地实时语音识别，
转写文本直接注入当前焦点应用的输入框——对任何应用生效，不依赖特定编辑器。

## 仓库结构

```
crates/audio    # 音频采集：整段录音 + 流式采集（增量重采样），WAV 读写
crates/asr      # ASR 引擎抽象：StreamingAsr trait（Send）+ Mock / sherpa-onnx 两种实现
crates/inject   # 文本注入层：paste / type / hybrid 三种策略
apps/tray       # voicetype-tray：托盘常驻，F9 录音→实时识别→注入（日常使用入口）
apps/cli        # voicetype 命令行：devices / record / run / realtime 诊断与验证
apps/demo       # voicetype-demo（手动演示）+ selftest（注入层自动化自测）
```

## 快速开始

```bash
# 默认构建（Mock 引擎，不含模型）
cargo build

# 启用 sherpa-onnx 本地识别引擎（首次构建自动下载预编译静态库）
cargo build --features sherpa

# 注入层自动化自测（自建窗口验证三种注入策略 + 剪贴板恢复）
cargo run -p voicetype-demo --bin selftest
```

## 日常使用（托盘版）

```bash
# 工作目录下放好模型（或设 VOICETYPE_MODEL_DIR），然后常驻运行
cargo run -p voicetype-tray
# 或
.\target\debug\voicetype-tray.exe
```

- 按 **F9** 开始录音（悬浮条显示中间结果，hybrid 模式同步上屏到当前输入框），
  再按 **F9** 结束并注入定稿；托盘双击图标同样可切换录音
- 托盘右键菜单：粘贴模式 / 键入模式 / 混合模式（流式，默认）/ 退出
- 悬浮条 topmost + 不抢焦点，不影响目标应用

## CLI 用法（诊断与验证）

```bash
# 诊断麦克风
voicetype devices

# 录音 3 秒保存 WAV（验证采集）
voicetype record --seconds 3 --out test.wav

# 全链路：录音 5 秒 → 识别 → 注入当前焦点窗口（倒计时 3 秒内点击目标输入框）
voicetype run --seconds 5 --engine sherpa \
  --encoder models/.../encoder.onnx --decoder models/.../decoder.onnx \
  --joiner models/.../joiner.onnx --tokens models/.../tokens.txt

# 无麦环境验证：从 WAV 文件驱动 ASR
voicetype run --engine sherpa --from-file test.wav --dry-run ...

# 注入模式：paste（默认）| type | hybrid
voicetype run --mode hybrid ...

# VAD 断句验证：WAV 驱动 VoiceSession（不传 --vad-model 则用 Mock 验证状态机）
voicetype vad-session --from-file test.wav --vad-model models/silero_vad.onnx \
  --encoder ... --decoder ... --joiner ... --tokens ...
```

## 三种文本注入策略（已实现并通过自动化验证）

| 策略 | 说明 | 适用 |
| ---- | ---- | ---- |
| `paste`（默认） | 剪贴板备份 → 写入 → Ctrl+V → 异步恢复 | 绝大多数场景；原子操作，不触发目标应用按键副作用（@、#、/ 联想面板等） |
| `type` | Win32 `SendInput` + `KEYEVENTF_UNICODE` 逐字注入 | 有实时打字感；粘贴失效时兜底 |
| `hybrid` | 中间识别结果逐字上屏，定稿时撤销中间结果并一次性粘贴 | 流式识别，兼顾实时感与干净插入 |

## 模型

推荐 sherpa-onnx 流式中文模型（已验证）：
`sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23`（int8 约 14MB，来源
https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models ）。
获取方式见下文「模型获取方式」——应用内一键下载 / 脚本 / 手动放置均可。

## 打包与分发

```bash
# 本地打包（release 构建 → 便携/安装 × 标准/带模型 + 裸 exe → 全部生成 .sha256）
.\scripts\package.ps1                 # 产物在 dist\
.\scripts\package.ps1 -WithModels     # 额外产出两种内嵌模型的变体
.\scripts\package.ps1 -SkipBuild      # 跳过构建，仅重新收集
.\scripts\package.ps1 -SkipInstaller  # 跳过安装包（未装 Inno Setup 时）

# GitHub Actions：推 v* tag 自动构建并发布 Release（也支持手动触发）
git tag v0.3.0 && git push origin v0.3.0
```

Release 资产（全部附 `.sha256` 清单）：

| 资产 | 说明 |
| --- | --- |
| `voicetype-tray.exe` | **便携版（推荐首装入口）**：单文件即装即用，模型在应用内一键下载 |
| `voicetype-setup-x64-with-models.exe` | **安装版（含模型）**：模型已内嵌，安装完成即可用 |
| `voicetype-setup-x64.exe` | **安装版**：Inno Setup 向导，按用户安装到 `%LocalAppData%\VoiceType`（无需管理员），可选开机自启，装完可一键下载模型 |
| `voicetype-win-x64-portable-with-models.zip` | 便携版（含模型）zip 变体：离线场景备选 |
| `voicetype-win-x64-portable.zip` | 便携版 zip 变体：与裸 exe 等价，多带一份文档和备用脚本 |

便携版使用：下载 `voicetype-tray.exe` → 运行 → 主界面自动弹出（双击托盘图标可再次打开）→ 点击「下载语音模型」→ 完成。
（该文件同时是热更新的下载目标，首装与热更同源。）

## 主界面

双击托盘图标（或右键菜单「主界面…」）打开，包含：

- **注入模式**：粘贴 / 键入 / 混合（流式）三选一
- **语音模型**：状态显示 + 一键下载（约 15MB，进度在悬浮条）
- **更新与公告**：检查更新、更新公告浏览
- **开机自动启动**：写入 HKCU Run（按用户级别）

## 模型获取方式

三种方式任选，布局一致（`models\<识别模型子目录>\*.onnx` + `models\silero_vad.onnx`），可互相混用：

1. **应用内一键下载**（推荐）：托盘右键 → 「下载语音模型…」，自动下载约 15MB 并解压就位，进度显示在悬浮条。
2. **脚本下载**：exe 旁执行 `.\setup-models.ps1`（幂等，已存在的模型自动跳过）。
3. **手动放置**：从 https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models 下载
   `sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23`（int8）解压到 `models\`，
   并下载 `silero_vad.onnx` 放到 `models\`。

模型目录解析顺序：`VOICETYPE_MODEL_DIR` 环境变量 > exe 旁 `models\` > 工作目录 `models\`。

## 热更新与更新公告

- **更新通道**：仓库根 [`app_update.json`](app_update.json) 是版本与更新公告的唯一事实来源，应用直读 main 分支（离线时回落到 exe 旁的本地副本）。
- **检查更新**：托盘右键 → 「检查更新…」，发现新版自动下载并做 SHA-256 校验；「更新公告」随时查看通道最新公告。
- **便携版热更**：替换 `voicetype-tray.exe` 后自动重启（参照 Nexuz 的 PowerShell 助手换文件方案）。
- **安装版热更**：静默重装新版安装包（`/VERYSILENT /RESTARTAPPLICATIONS`）并自动拉起应用；安装版目录由 `install.mode` 标记区分。
- **发版公告**：改 `app_update.json`（版本 + title + body）并 bump `apps/tray/Cargo.toml` 版本号，打 tag 推送；CI 用 `scripts/release_notes.py` 把该版本公告生成为 Release 正文。

## 路线图

- [x] 文本注入层（paste / type / hybrid 三策略并行，自动化自测通过）
- [x] 音频采集（cpal，16kHz 单声道重采样，WAV 读写）
- [x] 本地 ASR 引擎（sherpa-onnx 流式中文识别，三条官方测试音频识别正确）
- [x] 全局快捷键（F9）+ 托盘常驻 + 悬浮录音条（hybrid 流式上屏接入实时识别）
  - 已做：构建通过、启动后完整进入消息循环、CLI `realtime` 同款管线运行 4 秒无崩溃无死锁
  - 待你验证：有麦克风机器上按 F9 说中文，确认悬浮条出字、再按 F9 注入定稿
- [x] VAD 自动断句（silero-vad + VoiceSession 状态机，3 个单测 + 3 条真实音频验证；
  说完停顿 0.8s 自动结束注入，按 F9 后 5 秒无语音自动取消）
- [x] 打包：便携版 zip + 安装版 Inno Setup + GitHub Actions（tag 触发自动 Release，全部资产带 SHA-256 清单）
- [x] 应用内热更新（便携版换 exe / 安装版静默重装，双通道校验）+ 更新公告（app_update.json）
- [ ] 注入模式设置项与按应用配置
- [ ] macOS（CGEvent Unicode 注入）/ Linux X11（XTest）支持；Wayland 不支持
