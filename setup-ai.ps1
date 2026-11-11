param([switch]$SkipModels)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$env:UV_CACHE_DIR = Join-Path $PSScriptRoot '.moasearch-cache'
$env:PYTHONUTF8 = '1'
if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
    throw 'Install uv from https://docs.astral.sh/uv/getting-started/installation/ first.'
}
uv sync --extra ai --locked
if ($LASTEXITCODE -ne 0) { throw 'Embedded engine installation failed.' }
$pythonExe = Join-Path $PSScriptRoot '.venv\Scripts\python.exe'
if (-not $SkipModels) {
    & $pythonExe -m moasearch models download embedding
    if ($LASTEXITCODE -ne 0) { throw 'Embedding model download failed.' }
    & $pythonExe -m moasearch models download chat
    if ($LASTEXITCODE -ne 0) { throw 'Chat model download failed.' }
    & $pythonExe -m moasearch models download ocr
    if ($LASTEXITCODE -ne 0) { throw 'OCR model download failed.' }
}
& $pythonExe -m moasearch doctor
exit $LASTEXITCODE

