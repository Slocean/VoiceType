# VoiceType Windows 打包脚本：release 构建 → 便携版 zip + 裸 exe + 安装版 Inno Setup → 全部产物生成 .sha256。
# 用法：.\scripts\package.ps1 [-OutDir dist] [-SkipBuild] [-SkipInstaller]
param(
    [string]$OutDir = "dist",
    [switch]$SkipBuild,
    [switch]$SkipInstaller
)
$ErrorActionPreference = "Stop"

if (-not $SkipBuild) {
    Write-Host "release 构建（首次较慢）…"
    cargo build --release -p voicetype-tray
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

$exe = "target\release\voicetype-tray.exe"
if (-not (Test-Path $exe)) { Write-Error "找不到 $exe，请先构建"; exit 1 }

$version = (Select-String -Path "apps\tray\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
Write-Host "打包版本: v$version"

New-Item -ItemType Directory -Force $OutDir | Out-Null
$assets = @()

# ---------- 便携版（zip，手动下载/首次安装用）----------
$pkg = Join-Path $OutDir "voicetype-win-x64"
if (Test-Path $pkg) { Remove-Item $pkg -Recurse -Force }
New-Item -ItemType Directory -Force $pkg | Out-Null
Copy-Item $exe $pkg\ -Force
Copy-Item README.md $pkg\ -Force
Copy-Item scripts\setup-models.ps1 $pkg\ -Force
# 更新通道本地副本：离线时应用仍有公告可看。
Copy-Item app_update.json $pkg\ -Force
$portableZip = Join-Path $OutDir "voicetype-win-x64-portable.zip"
Compress-Archive -Path (Join-Path $pkg "*") -DestinationPath $portableZip -Force
Remove-Item $pkg -Recurse -Force
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
        Write-Warning "未找到 Inno Setup（ISCC.exe），跳过安装包。可 choco install innosetup -y 后重跑。"
    }
}

# ---------- SHA-256 清单（热更新下载校验用）----------
foreach ($a in $assets) {
    $hash = (Get-FileHash $a -Algorithm SHA256).Hash.ToLower()
    Set-Content -NoNewline -Path "$a.sha256" -Value "$hash  $(Split-Path $a -Leaf)`n" -Encoding ascii
    Write-Host ("  {0}  ({1})" -f $a, $hash)
}
Write-Host "打包完成: $OutDir"
