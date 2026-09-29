# 阶段 0：验收场景与当前失败证据清单

基线快照：`478cf74`（codex/quality-recovery，含未提交的 WebView2 主界面等改动）。
日期：2026-09-29。每条证据指向当前代码中可复核的事实；修复后对应场景的回归测试与运行记录写入各阶段变更说明。

## A. 模型有效性（对应 8.1）

| # | 场景 | 当前失败证据 |
| --- | --- | --- |
| A-1 | 同尺寸内容损坏的文件被判就绪 | `main.rs::find_model_files`/`find_vad_model` 只查文件名与 `is_file()`；`models_dl.rs::models_ready` 仅组合两者。测试 `models_dl.rs::ready_status_matches_recording_model_lookup` 把内容为 `test` 的伪 onnx 判为 `models_ready()==true`，固化了错误契约 |
| A-2 | 显式 `VOICETYPE_MODEL_DIR` 指向损坏目录时跳过修复 | `models_dl.rs::ensure_models_extracted` 在显式目录下 `if !root_has_model(...)` 才解压；文件名齐全但内容损坏时 `root_has_model` 为真，直接返回 Ok，不校验内容 |
| A-3 | 替换非事务：先删唯一旧文件再 rename | `models_dl.rs::write_if_missing`：`remove_file(path)` 成功后 `rename` 失败即模型缺失，无备份恢复 |
| A-4 | 并发自检/修复无互斥 | `ensure_models_extracted` 无目录级锁；两个进程同时写同一目录无协调 |
| A-5 | 启动/录音入口同步做约 50MB 散列与解压 | `main` 启动同步调用 `ensure_models_extracted`；`ui_main::toggle_recording` 在主线程调 `models_ready`（每次录音都重新解析目录）；`settings_web::on_ipc("repairModels")` 在 WebView IPC 回调里同步执行修复 |
| A-6 | 无统一状态契约 | 调用方自行组合 `models_ready`/`find_model_files`/`find_vad_model`/`resolve_model_dir` 推断就绪；无 ModelStatus 枚举，无 Corrupt/PermissionDenied 区分 |

## B. 控制层、焦点与设置事务（对应 8.2）

| # | 场景 | 当前失败证据 |
| --- | --- | --- |
| B-1 | 首页「开始录音」按钮在自身窗口获焦时启动注入 | `settings.html` 首页按钮 `record-btn` → `send({action:'record'})` → `settings_web.rs::on_ipc` 调 `ui_main::toggle_recording`；录音结束时 `Ev::Final` 无条件 `inject(&text)`，目标即当前前台窗口（很可能是 VoiceType 设置窗口自身） |
| B-2 | 无目标窗口校验/记录 | `toggle_recording` 与 `drain_worker_events` 全程不调用 `GetForegroundWindow`；hybrid 中间结果与定稿不确认目标窗口是否仍是录音启动时的窗口 |
| B-3 | HTML 把任意 `event.keyCode` 当 Win32 VK | `settings.html` hotkey keydown 直接用 `event.keyCode` 传给后端；`settings_web.rs` 仅检查 `0x08..=0xFE` 范围，未过滤纯修饰键/系统保留组合；原生备用窗口（尚不存在）无同一套校验 |
| B-4 | 热键保存无事务：注册成功但写注册表失败后内存已变更 | `ui_main::apply_new_hotkey` 注册成功即更新 `app.hotkey_*`；随后 `settings_web.rs::on_ipc` 的 `save_hotkey` 失败只发一条失败消息，不回滚注册与内存状态，重启后实际生效的是旧键 |
| B-5 | 自启启停无真实状态回读 | `settings.rs::set_autostart` 写注册表后不重新读取确认；旧快捷方式删除失败返回错误但界面状态不区分 |
| B-6 | IPC 未知 action 静默忽略 | `settings_web.rs::on_ipc` 的 `_ => {}`；无动作编号、无 Running/Failed 终态 |
| B-7 | RefCell 重入风险 | `on_ipc` 在 `with_app` 可变借用外层直接调用 `toggle_recording`（其内部 `with_app`），路径上叠加 `MessageBoxW`（模型未就绪分支）模态泵消息，可重入 wndproc |

## C. 窗口、更新结果与运行时（对应 8.3 可见性 / 8.2 窗口）

| # | 场景 | 当前失败证据 |
| --- | --- | --- |
| C-1 | 更新结果写入不可见页面或仅弹窗 | `settings_web::fill_announcement` 在窗口隐藏时返回 false → 回退 `MessageBoxW`；从托盘触发的检查更新先写悬浮条文本，结果落地时页面可能已关闭；CheckUpdate 与 FetchAnnouncement 共用 `UpdateEvent{ok,ready,text}` 无类型区分，`handle_update_event` 靠 `ev.ok && !ev.ready && fill_announcement(...)` 猜用途 |
| C-2 | 高 DPI 固定尺寸裁切 | `settings.html` body `min-width:690px; overflow:hidden`；`settings_web.rs` 固定 `CLIENT_W/H 760x610` 逻辑尺寸创建 WebView bounds；无 WM_SIZE/WM_DPICHANGED 处理 |
| C-3 | 无 WebView2 时设置完全不可用 | `show_settings` 创建失败仅弹 MessageBox 报错并销毁窗口，无原生备用界面 |
| C-4 | 关闭重开丢最近一次公告/更新结果 | 公告只经 `fill_announcement` 一次性写入 DOM，页面重载后无状态恢复；`refresh_all` 不含公告/更新结果字段 |

## D. 门禁与测试基础设施（对应 8.4）

| # | 场景 | 当前失败证据 |
| --- | --- | --- |
| D-1 | 无提交阶段 CI | `.github/` 仅有 tag 触发的 `release.yml`；无 `ci.yml`；fmt/clippy/locked 测试/DOM 测试均未在提交阶段执行 |
| D-2 | 无 DOM 交互测试 | 无 `ui-tests/`；HTML 行为（按钮动作、状态渲染、隐藏页结果）零自动化 |
| D-3 | 无可交互桌面验收 | 无 `scripts/gui_smoke.ps1`；无 UI Automation 场景 |
| D-4 | 测试经全局环境变量切换路径 | `models_dl.rs` 测试 `std::env::set_var("VOICETYPE_MODEL_DIR", ...)`，并行测试共享进程环境 |
| D-5 | LNK4098 未追查 | release 构建出现 LNK4098 链接器警告，来源未记录结论 |
| D-6 | 缺陷回归测试缺失 | 上述 A/B/C 场景均无失败复现测试 |

## E. 发布契约（对应 8.3 发版前）

| # | 场景 | 当前失败证据 |
| --- | --- | --- |
| E-1 | package.ps1 OutDir 与 installer.iss 固定 `..\dist` 不一致 | `installer.iss` `OutputDir=..\dist` 硬编码；`package.ps1 -OutDir X` 时安装包仍落 `dist`，`$assets += Join-Path $OutDir "voicetype-setup-x64.exe"` 引用不存在的文件 → SHA 清单步骤报错或漏文件 |
| E-2 | 版本未推进 | `Cargo.toml` 0.7.0；`app_update.json` 头部 0.7.0；v0.7.0 tag 已存在（`git tag` 可见），0.8.0 尚未建立 |
| E-3 | CI 打包用固定 `dist` | `release.yml` `./scripts/package.ps1 -SkipBuild` 无 `-OutDir`，复用 checkout 内目录 |
| E-4 | 六资产/双形态热更新未在本次工作区验证 | 本工作区改动后未产出候选资产，无 verify_release --dist 记录；列于 8.3 发版前/发版后清单 |

## 处置说明

- A/B/C/D 项由本轮代码修复与新增测试关闭；E-1/E-3 由工作包 E 修复。
- 涉及真机/可交互桌面的验收（8.1 `--self-check` 真机、8.2 双窗口与 DPI、8.3 双形态热更新、8.4 GUI 冒烟）在 CI 与脚本就绪后于可交互 Windows 环境执行；在此之前按完成定义计为未通过，不宣布发布。
