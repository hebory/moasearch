param([int]$Port = 8765, [switch]$NoBrowser, [string]$DataDir = '')
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$appExe = Join-Path $PSScriptRoot 'dist\moasearch.exe'
if (-not (Test-Path -LiteralPath $appExe)) { & (Join-Path $PSScriptRoot 'build.ps1') }
$launchArgs = @()
if ($DataDir) { $launchArgs += @('--data-dir', $DataDir) }
$launchArgs += @('serve', '--port', "$Port")
if ($NoBrowser) { $launchArgs += '--no-browser' }
& $appExe @launchArgs
exit $LASTEXITCODE

