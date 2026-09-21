# VoiceType Windows 打包脚本：release 构建 → 收集产物 → zip。
# 用法：.\scripts\package.ps1 [-OutDir dist] [-SkipBuild]
param(
    [string]$OutDir = "dist",
    [switch]$SkipBuild
)
$ErrorActionPreference = "Stop"

if (-not $SkipBuild) {
    Write-Host "release 构建（首次较慢）…"
    cargo build --release -p voicetype-tray
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

$exe = "target\release\voicetype-tray.exe"
if (-not (Test-Path $exe)) { Write-Error "找不到 $exe，请先构建"; exit 1 }

$pkg = Join-Path $OutDir "voicetype-win-x64"
New-Item -ItemType Directory -Force $pkg | Out-Null
Copy-Item $exe $pkg\ -Force
Copy-Item README.md $pkg\ -Force
Copy-Item scripts\setup-models.ps1 $pkg\ -Force

$zip = Join-Path $OutDir "voicetype-win-x64.zip"
Compress-Archive -Path (Join-Path $pkg "*") -DestinationPath $zip -Force
Write-Host "打包完成: $zip"