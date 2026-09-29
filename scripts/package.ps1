# VoiceType Windows 打包脚本：release 构建（含内置模型）→ 便携版 zip + 裸 exe + 安装版 → 全部产物生成 .sha256。
# 用法：.\scripts\package.ps1 [-OutDir dist] [-SkipBuild] [-SkipInstaller]
# 模型经 include_bytes! 编译进 exe（embedded-models feature），三种产物都自带模型、开箱即用。
param(
    [string]$OutDir = "dist",
    [switch]$SkipBuild,
    [switch]$SkipInstaller
)
$ErrorActionPreference = "Stop"

if (-not $SkipBuild) {
    Write-Host "准备内置模型源文件（首次较慢，约 450MB 下载）…"
    & (Join-Path $PSScriptRoot "prepare-models.ps1")

    Write-Host "release 构建（首次较慢）…"
    cargo build --release -p voicetype-tray --features embedded-models
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

$exe = "target\release\voicetype-tray.exe"
if (-not (Test-Path $exe)) { Write-Error "找不到 $exe，请先构建"; exit 1 }

$version = (Select-String -Path "apps\tray\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
Write-Host "打包版本: v$version"

New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$assets = @()

# 使用隔离的空模型目录验证实际 exe，阻止误把无内置模型的开发构建打包发布。
$smokeDir = Join-Path $OutDir ("voicetype-smoke-" + [Guid]::NewGuid().ToString("N"))
$outPrefix = $OutDir.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $smokeDir.StartsWith($outPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "自检临时目录不在输出目录内: $smokeDir"
}
$previousModelDir = $env:VOICETYPE_MODEL_DIR
try {
    $env:VOICETYPE_MODEL_DIR = $smokeDir
    $check = Start-Process -FilePath (Resolve-Path -LiteralPath $exe).Path -ArgumentList '--self-check' -Wait -PassThru -WindowStyle Hidden
    if ($check.ExitCode -ne 0) { throw "实际 exe 的内置模型解压或识别引擎初始化失败（退出码 $($check.ExitCode)）" }
} finally {
    $env:VOICETYPE_MODEL_DIR = $previousModelDir
    if (Test-Path -LiteralPath $smokeDir) { Remove-Item -LiteralPath $smokeDir -Recurse -Force }
}

# 清理上次打包的协议资产，避免安装包缺失时误把旧文件上传为新版。
foreach ($name in @(
    "voicetype-win-x64-portable.zip",
    "voicetype-setup-x64.exe",
    "voicetype-tray.exe"
)) {
    foreach ($path in @((Join-Path $OutDir $name), (Join-Path $OutDir "$name.sha256"))) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
    }
}

# ---------- 便携版（zip，手动下载/首次安装用）----------
$pkg = Join-Path $OutDir ("voicetype-stage-" + [Guid]::NewGuid().ToString("N"))
if (-not $pkg.StartsWith($outPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "临时打包目录不在输出目录内: $pkg"
}
$portableZip = Join-Path $OutDir "voicetype-win-x64-portable.zip"
New-Item -ItemType Directory -Path $pkg | Out-Null
try {
    Copy-Item $exe -Destination $pkg -Force
    Copy-Item README.md -Destination $pkg -Force
    # 更新通道本地副本：离线时应用仍有公告可看。
    Copy-Item app_update.json -Destination $pkg -Force
    Compress-Archive -Path (Join-Path $pkg "*") -DestinationPath $portableZip -Force
} finally {
    Remove-Item -LiteralPath $pkg -Recurse -Force
}
$assets += $portableZip

# ---------- 便携版热更资产（裸 exe，热更新只替换这一个文件）----------
$bareExe = Join-Path $OutDir "voicetype-tray.exe"
Copy-Item $exe $bareExe -Force
$assets += $bareExe

# ---------- 安装版（Inno Setup）----------
if (-not $SkipInstaller) {
    $iscc = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($iscc) { $iscc = $iscc.Source }
    else {
        foreach ($p in @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe")) {
            if (Test-Path $p) { $iscc = $p; break }
        }
    }
    if ($iscc) {
        Write-Host "生成安装包…"
        & $iscc "/DAppVersion=$version" scripts\installer.iss
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        $assets += (Join-Path $OutDir "voicetype-setup-x64.exe")
    } else {
        throw "未找到 Inno Setup（ISCC.exe）；安装包是发布必需资产。请安装后重跑，或明确使用 -SkipInstaller。"
    }
}

# ---------- SHA-256 清单（热更新下载校验用）----------
foreach ($a in $assets) {
    $hash = (Get-FileHash $a -Algorithm SHA256).Hash.ToLower()
    Set-Content -NoNewline -Path "$a.sha256" -Value "$hash  $(Split-Path $a -Leaf)`n" -Encoding ascii
    Write-Host ("  {0}  ({1})" -f $a, $hash)
}
Write-Host "打包完成: $OutDir"
