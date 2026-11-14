param([switch]$SkipModels, [string]$DataDir = '')
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$appExe = Join-Path $PSScriptRoot 'dist\moasearch.exe'
if (-not (Test-Path -LiteralPath $appExe)) { & (Join-Path $PSScriptRoot 'build.ps1') }
$baseArgs = @()
if ($DataDir) { $baseArgs += @('--data-dir', $DataDir) }
if (-not $SkipModels) {
    & $appExe @baseArgs models download embedding | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'EmbeddingGemma 2 Q8_0 준비에 실패했습니다.' }
    & $appExe @baseArgs models download chat | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'Gemma 4 E2B IT QAT Q4_0 준비에 실패했습니다.' }
    & $appExe @baseArgs models download ocr | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'PP-OCRv5 OCR 모델 준비에 실패했습니다.' }
}
& $appExe @baseArgs doctor | Out-Host
exit $LASTEXITCODE

