# 下载内置模型源文件到 apps/tray/embedded-models/（供 --features embedded-models 编译嵌入）。
# 幂等：文件已存在且大小一致时跳过。CI 与本地打包前都要跑一次。
# 用法：.\scripts\prepare-models.ps1 [-Dir apps\tray\embedded-models]
param([string]$Dir = "apps\tray\embedded-models")
$ErrorActionPreference = "Stop"

$ModelName = "sherpa-onnx-streaming-zipformer-small-bilingual-zh-en-2023-02-16"
$BaseUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models"
$Files = @(
    "encoder-epoch-99-avg-1.int8.onnx",
    "decoder-epoch-99-avg-1.int8.onnx",
    "joiner-epoch-99-avg-1.int8.onnx",
    "tokens.txt"
)
$VadFile = "silero_vad.onnx"

New-Item -ItemType Directory -Force $Dir | Out-Null

function Get-IfNeeded([string]$Url, [string]$Dest, [int]$MinBytes) {
    if ((Test-Path $Dest) -and (Get-Item $Dest).Length -ge $MinBytes) {
        Write-Host "  已存在: $Dest"
        return
    }
    Write-Host "  下载: $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
    if ((Get-Item $Dest).Length -lt $MinBytes) { Write-Error "下载不完整: $Dest"; exit 1 }
}

# VAD 直接单文件下载
Get-IfNeeded "$BaseUrl/$VadFile" (Join-Path $Dir $VadFile) 100000

# 识别模型：整包下载一次（含 fp32，约 450MB），解出 4 个 int8/tokens 后删除压缩包
$tarball = Join-Path $Dir "$ModelName.tar.bz2"
$missing = $Files | Where-Object { -not ((Test-Path (Join-Path $Dir $_)) -and (Get-Item (Join-Path $Dir $_)).Length -gt 10000) }
if ($missing) {
    Get-IfNeeded "$BaseUrl/$ModelName.tar.bz2" $tarball 100MB
    Write-Host "  解出所需文件…"
    foreach ($f in $Files) {
        tar -xjf $tarball -C $Dir --wildcards "$ModelName/$f"
        if ($LASTEXITCODE -ne 0) { Write-Error "解压失败: $f"; exit 1 }
        Move-Item (Join-Path $Dir "$ModelName/$f") (Join-Path $Dir $f) -Force
    }
    Remove-Item (Join-Path $Dir $ModelName) -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $tarball -Force
} else {
    Write-Host "  识别模型文件已就绪"
}

Write-Host "内置模型源文件就绪: $Dir"
Get-ChildItem $Dir -File | ForEach-Object { Write-Host ("  {0}  ({1:N0} bytes)" -f $_.Name, $_.Length) }
