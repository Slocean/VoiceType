; VoiceType 安装版打包脚本（Inno Setup 6）。
; 按用户安装（无需管理员）：%LocalAppData%\VoiceType，写 install.mode 标记，
; 该标记是应用热更新时区分安装版（静默重装）/便携版（换 exe）的依据。
; 构建：ISCC.exe /DAppVersion=0.2.0 installer.iss → dist\voicetype-setup-x64.exe
; 带模型变体：加 /DModelsDir=<模型暂存目录> → dist\voicetype-setup-x64-with-models.exe，
; 模型解压到 {app}\models（布局与 setup-models.ps1 一致），安装后无需再下载。

#define AppName "VoiceType"
#define AppExeName "voicetype-tray.exe"

#ifndef AppVersion
#define AppVersion "0.0.0"
#endif

#ifdef ModelsDir
#define OutBase "voicetype-setup-x64-with-models"
#else
#define OutBase "voicetype-setup-x64"
#endif

[Setup]
AppId={{8E5F2A10-6C3D-4B8A-9E21-3F7D0A55C101}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Slocean
; 安装包本身与卸载入口都用应用图标。
SetupIconFile=..\assets\voicetype.ico
DefaultDirName={localappdata}\{#AppName}
DisableProgramGroupPage=yes
; 按用户安装，热更静默重装也不需要提权。
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename={#OutBase}
Compression=lzma2/max
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#AppExeName}
WizardStyle=modern

[Languages]
; 中文语言包随仓库分发（choco 装的 Inno Setup 不自带），相对本脚本目录解析。
Name: "chinesesimplified"; MessagesFile: "Languages\ChineseSimplified.isl"

[Tasks]
Name: "autostart"; Description: "开机自动启动 VoiceType"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\scripts\setup-models.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
; 安装版标记：热更新据此选择「静默重装」路径。
Source: "install.mode"; DestDir: "{app}"; Flags: ignoreversion
; 更新通道本地副本：离线时应用仍有公告可看。
Source: "..\app_update.json"; DestDir: "{app}"; Flags: ignoreversion
#ifdef ModelsDir
; 内嵌模型：识别模型子目录 + silero_vad.onnx，与应用内下载同布局。
Source: "{#ModelsDir}\*"; DestDir: "{app}\models"; Flags: recursesubdirs ignoreversion
#endif

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{userstartup}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: autostart

[Run]
; 不带 skipifsilent：静默重装（热更新）结束后同样拉起新版。
Filename: "{app}\{#AppExeName}"; Flags: nowait runasoriginaluser
#ifndef ModelsDir
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\setup-models.ps1"""; Flags: postinstall skipifsilent runasoriginaluser; Description: "下载语音识别模型（约 40MB）"
#endif

[UninstallDelete]
; 清理热更新残留的校验记录与日志。
Type: files; Name: "{app}\voicetype_update.verify.json"
Type: files; Name: "{app}\voicetype_update.log"
Type: files; Name: "{app}\voicetype-tray_update.exe"
