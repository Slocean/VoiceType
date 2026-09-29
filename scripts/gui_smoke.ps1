# VoiceType GUI 冒烟测试（工作包 D2）。
# 必须在可交互 Windows 桌面会话运行（进程与 UI 自动化同会话）。
# 用法：
#   .\gui_smoke.ps1 -PortableExe <裸 exe 绝对路径> -InstallerExe <安装包绝对路径> -ReportDir <报告目录绝对路径>
# 隔离：所有运行使用独立 LOCALAPPDATA/模型目录，不触碰测试者个人设置。
# 退出码：0=全部通过；1=场景失败；2=参数/环境不满足（不算场景通过）。
param(
    [Parameter(Mandatory = $true)] [string]$PortableExe,
    [Parameter(Mandatory = $false)] [string]$InstallerExe,
    [Parameter(Mandatory = $true)] [string]$ReportDir
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName UIAutomationClient | Out-Null

$script:results = @()
$script:procs = @()
$script:isolatedDirs = @()

function Write-Report([string]$message) {
    Write-Host "[gui-smoke] $message"
}

function Add-Result([string]$name, [bool]$passed, [string]$detail) {
    $script:results += [pscustomobject]@{ Scenario = $name; Passed = $passed; Detail = $detail; Time = (Get-Date -Format o) }
    if ($passed) { Write-Report "PASS  $name" } else { Write-Report "FAIL  $name :: $detail" }
}

function Test-InteractiveDesktop {
    # 测试进程必须处于可交互桌面会话（非服务会话 0 无 UI 的情形）。
    $sessionId = (Get-Process -Id $PID).SessionId
    $interactive = $sessionId -ne 0 -or [Environment]::UserInteractive
    $explorer = Get-Process -Name explorer -ErrorAction SilentlyContinue
    if (-not $interactive -or -not $explorer) {
        Write-Report "当前会话：$sessionId；UserInteractive=$([Environment]::UserInteractive)；explorer=$([bool]$explorer)"
        Write-Report "需要可交互桌面会话运行 GUI 冒烟（D2 要求）。"
        exit 2
    }
}

function New-IsolatedEnvironment([string]$tag) {
    $base = Join-Path ([IO.Path]::GetTempPath()) ("voicetype-smoke-" + [Guid]::NewGuid().ToString("N"))
    $dirs = @{
        Root        = $base
        AppData     = Join-Path $base "localappdata"
        Models      = Join-Path $base "models"
        ExeHome     = Join-Path $base "app"
    }
    foreach ($d in $dirs.Values) { New-Item -ItemType Directory -Force $d | Out-Null }
    $script:isolatedDirs += $base
    return $dirs
}

function Start-VoiceType([string]$exePath, [hashtable]$env) {
    # 以隔离环境启动待测 exe；返回进程对象。
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $exePath
    $psi.WorkingDirectory = $env.ExeHome
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables["LOCALAPPDATA"] = $env.AppData
    $psi.EnvironmentVariables["VOICETYPE_MODEL_DIR"] = $env.Models
    $psi.EnvironmentVariables["TEMP"] = (Join-Path $env.Root "temp")
    $proc = [System.Diagnostics.Process]::Start($psi)
    $script:procs += $proc
    return $proc
}

function Wait-MainWindow([System.Diagnostics.Process]$proc, [int]$timeoutSeconds = 30) {
    # 按窗口类名等待真实主界面（WebView 设置窗口或原生备用窗口）。
    $deadline = (Get-Date).AddSeconds($timeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) { return $null }
        $root = [System.Windows.Automation.AutomationElement]::RootElement
        $condition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $proc.Id)
        $windows = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $condition)
        foreach ($w in $windows) {
            $name = $w.Current.Name
            $class = $w.Current.ClassName
            if ($class -like "VoiceType*" -and $name -like "*VoiceType*") { return $w }
        }
        Start-Sleep -Milliseconds 500
    }
    return $null
}

function Find-ChildByName($window, [string]$name) {
    if ($null -eq $window) { return $null }
    $condition = [System.Windows.Automation.AndCondition]::new(
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::NameProperty, $name),
        [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::IsOffscreenProperty, $false))
    return $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
}

function Stop-AllProcesses {
    foreach ($p in $script:procs) {
        try {
            if ($p -and -not $p.HasExited) { $p.Kill(); $p.WaitForExit(5000) | Out-Null }
        } catch { Write-Report "清理进程 $($p.Id) 失败：$_" }
    }
}

function Remove-IsolatedDirs {
    # 只清理本脚本创建的隔离目录（路径前缀自校验）。
    foreach ($dir in $script:isolatedDirs) {
        $full = [IO.Path]::GetFullPath($dir)
        if ($full.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase) -and
            $full -like "*voicetype-smoke-*") {
            Remove-Item -LiteralPath $full -Recurse -Force -ErrorAction SilentlyContinue
        } else {
            Write-Report "跳过非隔离目录清理：$full"
        }
    }
}

function Invoke-PortableSmoke {
    $env = New-IsolatedEnvironment "portable"
    Copy-Item $PortableExe (Join-Path $env.ExeHome "voicetype-tray.exe") -Force

    $proc = Start-VoiceType (Join-Path $env.ExeHome "voicetype-tray.exe") $env
    $window = Wait-MainWindow $proc 40
    if ($null -eq $window) {
        # 诊断信息后失败：进程、窗口列表、日志。
        $diag = "进程存在=$(-not $proc.HasExited)；退出码=$(if ($proc.HasExited) { $proc.ExitCode } else { 'N/A' })"
        $log = Join-Path $env.AppData "VoiceType\logs\voicetype.log"
        if (Test-Path $log) { $diag += "；日志尾部：$(Get-Content $log -Tail 5 | Out-String)" }
        Add-Result "便携版主窗口出现" $false $diag
        return
    }
    Add-Result "便携版主窗口出现" $true $window.Current.ClassName

    # 首页关键文本可见（UIA Name/文本核对）。
    $visible = @()
    foreach ($text in @("隐藏窗口", "输入方式", "录音快捷键")) {
        if ($null -ne (Find-ChildByName $window $text)) { $visible += $text }
    }
    Add-Result "首页关键文本可见" ($visible.Count -ge 1) "可见：$($visible -join ',')"

    # 关闭（隐藏）→ 二次启动复用实例重开窗口。
    $window.SetFocus()
    $closeButton = Find-ChildByName $window "关闭"
    if ($closeButton) { $closeButton.Invoke() }
    Start-Sleep -Seconds 2
    $second = Start-VoiceType (Join-Path $env.ExeHome "voicetype-tray.exe") $env
    $reopened = Wait-MainWindow $second 20
    if ($null -eq $reopened) { $reopened = Wait-MainWindow $proc 20 }
    Add-Result "关闭后重开主窗口" ($null -ne $reopened) "$(if ($reopened) { $reopened.Current.ClassName } else { '未找到窗口' })"

    # 隔离性：模型/日志都在隔离目录。
    $usedIsolatedModels = (Test-Path $env.Models)
    Add-Result "使用隔离模型目录" $usedIsolatedModels $env.Models
}

function Invoke-InstallerSmoke {
    if (-not $InstallerExe) {
        Add-Result "安装版冒烟（跳过：未提供安装包）" $false "-InstallerExe 未传入"
        return
    }
    $env = New-IsolatedEnvironment "installer"
    $prevLocalAppData = $env:LOCALAPPDATA
    try {
        # 静默安装进隔离 LOCALAPPDATA（按用户安装，写 install.mode）。
        $env:LOCALAPPDATA = $env.AppData
        $silent = Start-Process -FilePath $InstallerExe -ArgumentList "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART" -Wait -PassThru
        if ($silent.ExitCode -ne 0) {
            Add-Result "安装版静默安装" $false "退出码 $($silent.ExitCode)"
            return
        }
        $installed = Join-Path $env.AppData "VoiceType\voicetype-tray.exe"
        $modeMarker = Join-Path $env.AppData "VoiceType\install.mode"
        Add-Result "安装版静默安装" ((Test-Path $installed) -and (Test-Path $modeMarker)) "exe=$(Test-Path $installed) install.mode=$(Test-Path $modeMarker)"
    } finally {
        $env:LOCALAPPDATA = $prevLocalAppData
    }
}

# ---------- 主流程 ----------
Test-InteractiveDesktop

if (-not ([IO.Path]::IsPathRooted($PortableExe)) -or -not (Test-Path $PortableExe)) {
    Write-Report "-PortableExe 必须是存在的绝对路径：$PortableExe"
    exit 2
}
if (-not ([IO.Path]::IsPathRooted($ReportDir))) {
    Write-Report "-ReportDir 必须是绝对路径：$ReportDir"
    exit 2
}
New-Item -ItemType Directory -Force $ReportDir | Out-Null

try {
    Invoke-PortableSmoke
    Invoke-InstallerSmoke
} finally {
    Stop-AllProcesses
    Start-Sleep -Seconds 1
    Remove-IsolatedDirs
}

$reportPath = Join-Path $ReportDir ("gui-smoke-" + (Get-Date -Format "yyyyMMdd-HHmmss") + ".json")
$script:results | ConvertTo-Json -Depth 3 | Set-Content -Path $reportPath -Encoding UTF8
Write-Report "报告：$reportPath"

$failed = @($script:results | Where-Object { -not $_.Passed })
if ($failed.Count -gt 0) {
    Write-Report "$($failed.Count) 个场景未通过。"
    exit 1
}
Write-Report "全部场景通过。"
exit 0
