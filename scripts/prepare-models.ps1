# 下载内置模型源文件到 apps/tray/embedded-models/（供 --features embedded-models 编译嵌入）。
# 幂等：源文件已存在且超过最低体积时跳过；最终 exe 在 CI 中初始化真实模型验收。
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

$Dir = (New-Item -ItemType Directory -Force $Dir).FullName

function Get-IfNeeded([string]$Url, [string]$Dest, [int]$MinBytes) {
    if ((Test-Path $Dest) -and (Get-Item $Dest).Length -ge $MinBytes) {
        Write-Host "  已存在: $Dest"
        return
    }
    Write-Host "  下载: $Url"
    $partial = "$Dest.partial"
    try {
        if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
        Invoke-WebRequest -Uri $Url -OutFile $partial -UseBasicParsing
        if ((Get-Item -LiteralPath $partial).Length -lt $MinBytes) { throw "下载不完整: $Dest" }
        Move-Item -LiteralPath $partial -Destination $Dest -Force
    } finally {
        if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
    }
}

# VAD 直接单文件下载
Get-IfNeeded "$BaseUrl/$VadFile" (Join-Path $Dir $VadFile) 100000

# 识别模型：整包下载一次（含 fp32，约 450MB），解出 4 个 int8/tokens 后删除压缩包
$tarball = Join-Path $Dir "$ModelName.tar.bz2"
$missing = $Files | Where-Object { -not ((Test-Path (Join-Path $Dir $_)) -and (Get-Item (Join-Path $Dir $_)).Length -gt 10000) }
if ($missing) {
    Get-IfNeeded "$BaseUrl/$ModelName.tar.bz2" $tarball 100MB
    Write-Host "  解出所需文件…"
    $stage = Join-Path $Dir ("extract-" + [Guid]::NewGuid().ToString("N"))
    $dirPrefix = $Dir.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $stage.StartsWith($dirPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "临时解压目录不在模型源目录内: $stage"
    }
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        $members = @($Files | ForEach-Object { "$ModelName/$_" })
        & tar -xjf $tarball -C $stage @members
        if ($LASTEXITCODE -ne 0) { throw "模型压缩包解压失败" }
        foreach ($f in $Files) {
            $extracted = Join-Path $stage "$ModelName/$f"
            if (-not (Test-Path -LiteralPath $extracted) -or (Get-Item -LiteralPath $extracted).Length -le 10000) {
                throw "模型源文件异常: $f"
            }
        }
        foreach ($f in $Files) {
            Move-Item -LiteralPath (Join-Path $stage "$ModelName/$f") -Destination (Join-Path $Dir $f) -Force
        }
    } finally {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
    Remove-Item $tarball -Force
} else {
    Write-Host "  识别模型文件已就绪"
}

Write-Host "内置模型源文件就绪: $Dir"
Get-ChildItem $Dir -File | ForEach-Object { Write-Host ("  {0}  ({1:N0} bytes)" -f $_.Name, $_.Length) }
