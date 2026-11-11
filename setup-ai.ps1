param([switch]$SkipModels, [string]$DataDir = '')
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$appExe = Join-Path $PSScriptRoot 'dist\moasearch.exe'
if (-not (Test-Path -LiteralPath $appExe)) { & (Join-Path $PSScriptRoot 'build.ps1') }
$baseArgs = @()
if ($DataDir) { $baseArgs += @('--data-dir', $DataDir) }
if (-not $SkipModels) {
    & $appExe @baseArgs models download embedding
    if ($LASTEXITCODE -ne 0) { throw 'EmbeddingGemma 모델 준비에 실패했습니다.' }
    & $appExe @baseArgs models download chat
    if ($LASTEXITCODE -ne 0) { throw 'Gemma 답변 모델 준비에 실패했습니다.' }
    & $appExe @baseArgs models download ocr
    if ($LASTEXITCODE -ne 0) { throw 'OCR 모델 준비에 실패했습니다.' }
}
& $appExe @baseArgs doctor
exit $LASTEXITCODE

