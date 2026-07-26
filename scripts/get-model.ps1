# Blurt 模型下载脚本
# 下载 Qwen3-ASR-0.6B (int8, sherpa-onnx 格式) 到 %APPDATA%\Blurt\models
# 用法:  pwsh -File scripts/get-model.ps1  [-Dest <目录>]

param(
    [string]$Dest = "$env:APPDATA\Blurt\models"
)

$ErrorActionPreference = 'Stop'
$name = 'sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25'
$modelDir = Join-Path $Dest $name

$files = @('conv_frontend.onnx', 'encoder.int8.onnx', 'decoder.int8.onnx',
           'tokenizer/vocab.json', 'tokenizer/merges.txt', 'tokenizer/tokenizer_config.json')

function Test-ModelComplete {
    foreach ($f in $files) {
        $p = Join-Path $modelDir $f
        if (-not (Test-Path $p)) { return $false }
    }
    return $true
}

if (Test-ModelComplete) {
    Write-Host "✓ 模型已存在：$modelDir" -ForegroundColor Green
    exit 0
}

New-Item -ItemType Directory -Force $Dest | Out-Null
$tarball = Join-Path $Dest "$name.tar.bz2"

# 首选：GitHub Release 整包
$primary = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/$name.tar.bz2"
Write-Host "正在下载模型（约 937 MB），请耐心等待..." -ForegroundColor Cyan
$ok = $false
try {
    curl.exe -L --fail --retry 3 --retry-delay 5 -o $tarball $primary
    if ($LASTEXITCODE -eq 0) { $ok = $true }
} catch {}

if ($ok) {
    Write-Host "正在解压..." -ForegroundColor Cyan
    tar -xjf $tarball -C $Dest
    Remove-Item $tarball -Force -Confirm:$false
} else {
    # 备选：hf-mirror 按文件下载（国内网络友好）
    Write-Host "GitHub 下载失败，切换到 hf-mirror.com 镜像..." -ForegroundColor Yellow
    $repo = "https://hf-mirror.com/cattle12/$name/resolve/main"
    New-Item -ItemType Directory -Force (Join-Path $modelDir 'tokenizer') | Out-Null
    foreach ($f in $files) {
        $out = Join-Path $modelDir $f
        Write-Host "  ↓ $f"
        curl.exe -L --fail --retry 3 --retry-delay 5 -o $out "$repo/$f"
        if ($LASTEXITCODE -ne 0) { throw "下载失败：$f" }
    }
}

if (Test-ModelComplete) {
    Write-Host "✓ 模型就绪：$modelDir" -ForegroundColor Green
} else {
    Write-Host "✗ 模型文件不完整，请重试或手动下载。" -ForegroundColor Red
    exit 1
}
