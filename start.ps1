param([int]$Port = 8765, [switch]$NoBrowser, [string]$DataDir = '')
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$env:UV_CACHE_DIR = Join-Path $PSScriptRoot '.moasearch-cache'
$env:PYTHONUTF8 = '1'
$pythonExe = Join-Path $PSScriptRoot '.venv\Scripts\python.exe'
if (-not (Test-Path -LiteralPath $pythonExe)) {
    if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
        throw 'Install uv from https://docs.astral.sh/uv/getting-started/installation/ first.'
    }
    uv sync --extra ai --locked
    if ($LASTEXITCODE -ne 0) { throw 'Dependency installation failed.' }
}
$launchArgs = @('-m', 'moasearch')
if ($DataDir) { $launchArgs += @('--data-dir', $DataDir) }
$launchArgs += @('serve', '--port', "$Port")
if ($NoBrowser) { $launchArgs += '--no-browser' }
& $pythonExe @launchArgs
exit $LASTEXITCODE

