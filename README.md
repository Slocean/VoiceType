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
`sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23`（int8 约 14MB，
从 https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models 下载，
解压到 `models/` 即可）。

## 打包与分发

```bash
# 本地打包（release 构建 → 收集产物 → zip）
.\scripts\package.ps1            # 产物：dist\voicetype-win-x64.zip
.\scripts\package.ps1 -SkipBuild # 跳过构建，仅重新收集

# GitHub Actions：推 v* tag 自动构建并发布 Release（也支持手动触发）
git tag v0.1.0 && git push origin v0.1.0
```

发布包内容：`voicetype-tray.exe`（约 18MB，sherpa-onnx 静态链接）+
`setup-models.ps1`（首次运行下载 ASR/VAD 模型到 models\）+ README。
用户解压后先执行 `.\setup-models.ps1`，再启动 `voicetype-tray.exe`。

## 路线图

- [x] 文本注入层（paste / type / hybrid 三策略并行，自动化自测通过）
- [x] 音频采集（cpal，16kHz 单声道重采样，WAV 读写）
- [x] 本地 ASR 引擎（sherpa-onnx 流式中文识别，三条官方测试音频识别正确）
- [x] 全局快捷键（F9）+ 托盘常驻 + 悬浮录音条（hybrid 流式上屏接入实时识别）
  - 已做：构建通过、启动后完整进入消息循环、CLI `realtime` 同款管线运行 4 秒无崩溃无死锁
  - 待你验证：有麦克风机器上按 F9 说中文，确认悬浮条出字、再按 F9 注入定稿
- [x] VAD 自动断句（silero-vad + VoiceSession 状态机，3 个单测 + 3 条真实音频验证；
  说完停顿 0.8s 自动结束注入，按 F9 后 5 秒无语音自动取消）
- [x] 打包：本地 package.ps1 + GitHub Actions（tag 触发自动 Release）
- [ ] 注入模式设置项与按应用配置
- [ ] macOS（CGEvent Unicode 注入）/ Linux X11（XTest）支持；Wayland 不支持
