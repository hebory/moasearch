#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot '..\dist\moasearch.exe'),
    [ValidateRange(0,65535)]
    [int]$Port = 0,
    [switch]$Desktop
)

$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $ExecutablePath -ErrorAction Stop).Path
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$fixtureRoot = Join-Path $repo ('.moasearch-tests\explorer-launch-' + [guid]::NewGuid().ToString('N'))
$data = Join-Path $fixtureRoot 'app-data'
$documents = Join-Path $fixtureRoot '문서'
$selected = Join-Path $documents '선택 %_ # & + 폴더'
$nested = Join-Path $selected '하위'
$sibling = Join-Path $documents '선택 %_ # & + 폴더2'
$unindexed = Join-Path $fixtureRoot '새 폴더'
$requests = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()
$ownedPid = $null
$cleanupError = $null

function Get-AvailablePort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    try {
        $listener.Start()
        return ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    } finally {
        $listener.Stop()
    }
}

function Test-SamePath([string]$Left, [string]$Right) {
    if (-not $Left -or -not $Right) { return $false }
    $leftPath = [System.IO.Path]::GetFullPath($Left).TrimEnd('\','/')
    $rightPath = [System.IO.Path]::GetFullPath($Right).TrimEnd('\','/')
    return [string]::Equals($leftPath, $rightPath, [System.StringComparison]::OrdinalIgnoreCase)
}

function Start-ExplorerRequest([string]$DataDirectory, [string]$Folder) {
    $start = [System.Diagnostics.ProcessStartInfo]::new($exe)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $arguments = @('--no-dialog', '--data-dir', $DataDirectory, 'open-folder', '--path', $Folder, '--port', [string]$Port)
    if (-not $Desktop) { $arguments += '--no-browser' }
    foreach ($argument in $arguments) {
        [void]$start.ArgumentList.Add($argument)
    }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    if (-not $process.Start()) { throw 'Explorer 실행 요청 프로세스를 시작하지 못했습니다.' }
    [void]$requests.Add($process)
    return [pscustomobject]@{
        Process = $process
        Stdout = $process.StandardOutput.ReadToEndAsync()
        Stderr = $process.StandardError.ReadToEndAsync()
    }
}

function Complete-ExplorerRequest($Request) {
    if (-not $Request.Process.WaitForExit(30000)) {
        try { $Request.Process.Kill() } catch { }
        throw 'Explorer 실행 요청 시간이 초과되었습니다.'
    }
    $pipeDeadline = [DateTime]::UtcNow.AddSeconds(5)
    $remainingMilliseconds = [Math]::Max(0, [int](($pipeDeadline - [DateTime]::UtcNow).TotalMilliseconds))
    if (-not $Request.Stdout.Wait(5000) -or
        -not $Request.Stderr.Wait($remainingMilliseconds)) {
        throw '백그라운드 앱 서버가 호출자의 출력 파이프를 닫지 않았습니다.'
    }
    return [pscustomobject]@{
        ExitCode = $Request.Process.ExitCode
        Output = $Request.Stdout.GetAwaiter().GetResult()
        Error = $Request.Stderr.GetAwaiter().GetResult()
    }
}

function Get-OwnedInstance {
    $baseUrl = "http://127.0.0.1:$Port"
    try {
        $page = Invoke-WebRequest -UseBasicParsing -Uri $baseUrl -TimeoutSec 2
        $token = [regex]::Match($page.Content, 'name="moasearch-token" content="([^"]+)"').Groups[1].Value
        if (-not $token) { return $null }
        $headers = @{ 'X-MoaSearch-Token' = $token }
        $instance = Invoke-RestMethod -UseBasicParsing -Uri "$baseUrl/api/instance" -Headers $headers -TimeoutSec 2
        if ($instance.app -ne 'moasearch' -or $instance.folder_protocol -ne 1 -or -not (Test-SamePath $instance.data_dir $data)) {
            return $null
        }
        $instancePid = 0
        if (-not [int]::TryParse([string]$instance.pid, [ref]$instancePid) -or $instancePid -le 0) { return $null }
        $serverProcess = Get-Process -Id $instancePid -ErrorAction Stop
        if (-not (Test-SamePath $serverProcess.Path $exe)) { return $null }
        return [pscustomobject]@{ Instance = $instance; Token = $token; BaseUrl = $baseUrl }
    } catch {
        return $null
    }
}

if ($Port -eq 0) { $Port = Get-AvailablePort }
if ($Port -lt 1024) { throw 'Port는 자동 선택(0) 또는 1024~65535 범위를 사용하세요.' }
New-Item -ItemType Directory -Path $nested, $sibling, $unindexed -Force | Out-Null
Set-Content -LiteralPath (Join-Path $selected '현재.txt') -Value '계약검토 선택한 폴더의 내용' -Encoding utf8
Set-Content -LiteralPath (Join-Path $nested '하위.txt') -Value '계약검토 선택한 폴더의 하위 내용' -Encoding utf8
Set-Content -LiteralPath (Join-Path $sibling '바깥.txt') -Value '계약검토 다른 형제 폴더 내용' -Encoding utf8
Set-Content -LiteralPath (Join-Path $unindexed '새문서.txt') -Value '새색인 검증 문서' -Encoding utf8
Get-ChildItem -LiteralPath $documents, $unindexed -File -Recurse | ForEach-Object { $_.IsReadOnly = $true }

try {
    $indexLog = Join-Path $fixtureRoot 'index.log'
    & $exe --data-dir $data index $documents 2>&1 | Out-File -LiteralPath $indexLog -Encoding utf8
    if ($LASTEXITCODE -ne 0) { throw "시작 검증용 문서 색인에 실패했습니다. 로그: $indexLog" }

    $first = Start-ExplorerRequest $data $selected
    $second = Start-ExplorerRequest $data $sibling
    $responses = @((Complete-ExplorerRequest $first), (Complete-ExplorerRequest $second))
    foreach ($response in $responses) {
        if ($response.ExitCode -ne 0) { throw "동시 Explorer 요청 실패: $($response.Error)" }
    }
    $firstResult = $responses[0].Output | ConvertFrom-Json
    $secondResult = $responses[1].Output | ConvertFrom-Json
    if (-not $firstResult.pid -or $firstResult.pid -ne $secondResult.pid) {
        throw '동시 기동 요청이 하나의 서버 프로세스로 합쳐지지 않았습니다.'
    }
    $ownedPid = [int]$firstResult.pid
    if ($Desktop -and (-not $firstResult.desktop -or -not $secondResult.desktop)) {
        throw '탐색기 요청이 Windows 앱으로 연결되지 않았습니다.'
    }

    $reuseResponse = Complete-ExplorerRequest (Start-ExplorerRequest $data $selected)
    if ($reuseResponse.ExitCode -ne 0) { throw "실행 중인 앱 재사용 실패: $($reuseResponse.Error)" }
    $reuseResult = $reuseResponse.Output | ConvertFrom-Json
    if (-not $reuseResult.reused -or $reuseResult.pid -ne $ownedPid) { throw '이미 실행 중인 앱을 재사용하지 않았습니다.' }

    $differentData = Join-Path $fixtureRoot '다른 데이터'
    $otherDataResponse = Complete-ExplorerRequest (Start-ExplorerRequest $differentData $selected)
    if ($otherDataResponse.ExitCode -eq 0 -or $otherDataResponse.Error -notmatch '다른 데이터 위치') {
        throw '데이터 폴더가 다른 앱을 잘못 재사용했거나 오류를 알리지 않았습니다.'
    }

    $page = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port" -TimeoutSec 5
    $token = [regex]::Match($page.Content, 'name="moasearch-token" content="([^"]+)"').Groups[1].Value
    if (-not $token) { throw '기동한 서버에서 인증 토큰을 확인하지 못했습니다.' }
    $headers = @{ 'X-MoaSearch-Token' = $token }
    foreach ($folder in @($selected, $sibling)) {
        $body = @{ query = '계약검토'; mode = 'keyword'; folder = $folder } | ConvertTo-Json
        $search = Invoke-RestMethod -UseBasicParsing -Uri "http://127.0.0.1:$Port/api/search" -Headers $headers `
            -Method Post -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($body))
        $expected = if ($folder -eq $selected) { 2 } else { 1 }
        if ($search.results.Count -ne $expected) { throw "폴더 범위 검색 결과가 예상과 다릅니다: $folder" }
    }

    $unindexedUrl = $firstResult.url -replace '#folder=.*$', ('#folder=' + [Uri]::EscapeDataString($unindexed))
    [pscustomobject]@{
        pid = $ownedPid
        desktop = [bool]$Desktop
        concurrentSingleServer = $true
        redirectedPipesClosed = $true
        reusedRunningServer = $true
        differentDataRejected = $true
        scopedResults = @{ selected = 2; sibling = 1 }
        selectedUrl = $firstResult.url
        siblingUrl = $secondResult.url
        unindexedFolderUrl = $unindexedUrl
        data = $data
        fixturesAndLogs = $fixtureRoot
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $fixtureRoot 'report.json') -Encoding utf8
} finally {
    foreach ($process in $requests) {
        if (-not $process.HasExited) {
            try { $process.Kill(); [void]$process.WaitForExit(5000) } catch { }
        }
        $process.Dispose()
    }

    $owned = $null
    $discoveryDeadline = [DateTime]::UtcNow.AddSeconds(20)
    while (-not $owned -and [DateTime]::UtcNow -lt $discoveryDeadline) {
        $owned = Get-OwnedInstance
        if (-not $owned) { Start-Sleep -Milliseconds 200 }
    }
    if ($owned) {
        $runningPid = [int]$owned.Instance.pid
        if ($ownedPid -and $runningPid -ne $ownedPid) {
            $cleanupError = '정리 중 서버 PID가 변경되어 앱 종료 요청을 보내지 않았습니다.'
        } else {
            $ownedPid = $runningPid
            try {
                Invoke-RestMethod -UseBasicParsing -Uri "$($owned.BaseUrl)/api/quit" -Headers @{ 'X-MoaSearch-Token' = $owned.Token } `
                    -Method Post -ContentType 'application/json' -Body '{}' -TimeoutSec 3 | Out-Null
                $deadline = [DateTime]::UtcNow.AddSeconds(15)
                while ([DateTime]::UtcNow -lt $deadline -and (Get-Process -Id $ownedPid -ErrorAction SilentlyContinue)) {
                    Start-Sleep -Milliseconds 200
                }
                if (Get-Process -Id $ownedPid -ErrorAction SilentlyContinue) {
                    $cleanupError = "검증 서버 PID $ownedPid 가 /api/quit 이후 종료되지 않았습니다."
                }
            } catch {
                if (Get-Process -Id $ownedPid -ErrorAction SilentlyContinue) {
                    $cleanupError = "검증 서버 종료 확인 실패: $($_.Exception.Message)"
                }
            }
            if (Get-Process -Id $ownedPid -ErrorAction SilentlyContinue) {
                # Recheck the unique data directory and executable before a forced fallback.
                $stillOwned = Get-OwnedInstance
                if ($stillOwned -and [int]$stillOwned.Instance.pid -eq $ownedPid) {
                    Stop-Process -Id $ownedPid -ErrorAction Stop
                    try { Wait-Process -Id $ownedPid -Timeout 10 -ErrorAction Stop } catch { }
                    if (Get-Process -Id $ownedPid -ErrorAction SilentlyContinue) {
                        $cleanupError = "검증 서버 PID $ownedPid 를 종료하지 못했습니다."
                    }
                } else {
                    $cleanupError = '종료 대기 중 PID 소유권이 달라져 프로세스를 강제 종료하지 않았습니다.'
                }
            }
        }
    }
}

if ($cleanupError) { throw $cleanupError }
Write-Output "Explorer 기동 회귀 검증 통과. fixture 및 로그 보존 위치: $fixtureRoot"
