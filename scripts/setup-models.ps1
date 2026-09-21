# VoiceType 模型一键下载脚本。
# 用法：.\setup-models.ps1 [-ModelsDir <目录>]
# 默认模型目录：脚本所在目录的 models\（打包布局）；
# 若上级目录已有 models\（仓库布局）则优先使用之。
param([string]$ModelsDir = "")
$ErrorActionPreference = "Stop"

if ($ModelsDir -eq "") {
    $repoModels = Join-Path (Split-Path $PSScriptRoot -Parent) "models"
    if (Test-Path $repoModels) {
        $ModelsDir = $repoModels
    } else {
        $ModelsDir = Join-Path $PSScriptRoot "models"
    }
}

$VadUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
$ZhName = "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23"
$ZhUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/$ZhName.tar.bz2"

New-Item -ItemType Directory -Force $ModelsDir | Out-Null

# silero-vad（约 0.6MB）
$vad = Join-Path $ModelsDir "silero_vad.onnx"
if (-not (Test-Path $vad)) {
    Write-Host "下载 VAD 模型…"
    Invoke-WebRequest -Uri $VadUrl -OutFile $vad -UseBasicParsing
} else { Write-Host "VAD 模型已存在" }

# 流式中文识别模型（int8，约 14MB 压缩包）
$marker = Join-Path $ModelsDir "$ZhName\tokens.txt"
if (-not (Test-Path $marker)) {
    Write-Host "下载识别模型…"
    $archive = Join-Path $ModelsDir "$ZhName.tar.bz2"
    Invoke-WebRequest -Uri $ZhUrl -OutFile $archive -UseBasicParsing
    tar -xjf $archive -C $ModelsDir
    Remove-Item $archive
    Write-Host "识别模型就绪"
} else { Write-Host "识别模型已存在" }

Write-Host "模型目录: $ModelsDir"
Write-Host "现在可以启动 voicetype-tray.exe（保持本目录为工作目录，或设置 VOICETYPE_MODEL_DIR）"