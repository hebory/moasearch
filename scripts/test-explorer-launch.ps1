#Requires -Version 7.0
[CmdletBinding()]
param([string]$ExecutablePath = (Join-Path $PSScriptRoot '..\dist\moasearch.exe'))

$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $ExecutablePath -ErrorAction Stop).Path
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$fixtureRoot = Join-Path $repo ('.moasearch-tests\explorer-ipc-' + [guid]::NewGuid().ToString('N'))
$data = Join-Path $fixtureRoot 'data-main'
$otherData = Join-Path $fixtureRoot 'data-other'
$documents = Join-Path $fixtureRoot '문서'
$selected = Join-Path $documents '선택 %_ # & + 폴더'
$nested = Join-Path $selected '하위'
$sibling = Join-Path $documents '선택 %_ # & + 폴더2'
$requests = [Collections.Generic.List[System.Diagnostics.Process]]::new()
$dataDirectories = @($data,$otherData)
$cleanupErrors = [Collections.Generic.List[string]]::new()
$failure = $null
$report = $null

function Invoke-MoaCli {
    param([string]$DataDirectory,[string[]]$Arguments,[int]$TimeoutSeconds=30)
    $start = [Diagnostics.ProcessStartInfo]::new($exe)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach($argument in (@('--no-dialog','--data-dir',$DataDirectory)+$Arguments)){[void]$start.ArgumentList.Add($argument)}
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    if(-not $process.Start()){throw "모아검색 CLI를 시작하지 못했습니다: $($Arguments -join ' ')"}
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if(-not $process.WaitForExit($TimeoutSeconds*1000)){
            $process.Kill()
            $process.WaitForExit()
            throw "모아검색 CLI 시간이 초과되었습니다: $($Arguments -join ' ')"
        }
        if(-not $stdout.Wait(5000) -or -not $stderr.Wait(5000)){
            throw "CLI 출력 파이프가 닫히지 않았습니다: $($Arguments -join ' ')"
        }
        $outText=$stdout.GetAwaiter().GetResult();$errText=$stderr.GetAwaiter().GetResult()
        if($process.ExitCode -ne 0){throw "모아검색 CLI 실패($($process.ExitCode)): $errText $outText"}
        return $outText
    } finally {
        if(-not $process.HasExited){try{$process.Kill();$process.WaitForExit()}catch{}}
        $process.Dispose()
    }
}

function Start-ExplorerOpen([string]$DataDirectory,[string]$Folder){
    $start=[Diagnostics.ProcessStartInfo]::new($exe)
    $start.UseShellExecute=$false;$start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true;$start.RedirectStandardError=$true
    foreach($argument in @('--no-dialog','--data-dir',$DataDirectory,'open-folder','--path',$Folder)){[void]$start.ArgumentList.Add($argument)}
    $process=[Diagnostics.Process]::new();$process.StartInfo=$start
    if(-not $process.Start()){throw 'Explorer 요청을 시작하지 못했습니다.'}
    [void]$requests.Add($process)
    $stdout=$process.StandardOutput.ReadToEndAsync();$stderr=$process.StandardError.ReadToEndAsync()
    [pscustomobject]@{Process=$process;Stdout=$stdout;Stderr=$stderr}
}

function Complete-ExplorerOpen($request){
    try {
        if(-not $request.Process.WaitForExit(30000)){throw 'Explorer 연결 요청 시간이 초과되었습니다.'}
        if(-not $request.Stdout.Wait(5000) -or -not $request.Stderr.Wait(5000)){
            throw 'Explorer 요청의 출력 파이프가 종료되지 않았습니다.'
        }
        $outText=$request.Stdout.GetAwaiter().GetResult();$errText=$request.Stderr.GetAwaiter().GetResult()
        if($request.Process.ExitCode -ne 0){throw "Explorer 요청 실패($($request.Process.ExitCode)): $errText $outText"}
        try{return ($outText|ConvertFrom-Json -ErrorAction Stop)}catch{throw "Explorer가 JSON 응답을 반환하지 않았습니다: $outText"}
    } finally {
        if(-not $request.Process.HasExited){try{$request.Process.Kill();$request.Process.WaitForExit()}catch{}}
        [void]$requests.Remove($request.Process)
        $request.Process.Dispose()
    }
}

function Get-ScopedCount([string]$Folder){
    $output=Invoke-MoaCli $data @('search','계약검토','--keyword','--folder',$Folder)
    return (@(($output|ConvertFrom-Json).results)).Count
}

try{
    New-Item -ItemType Directory -Path $nested,$sibling -Force|Out-Null
    Set-Content -LiteralPath (Join-Path $selected '현재.txt') -Value '계약검토 선택한 폴더의 내용' -Encoding utf8
    Set-Content -LiteralPath (Join-Path $nested '하위.txt') -Value '계약검토 선택한 폴더의 하위 내용' -Encoding utf8
    Set-Content -LiteralPath (Join-Path $sibling '바깥.txt') -Value '계약검토 다른 형제 폴더 내용' -Encoding utf8
    Get-ChildItem -LiteralPath $documents -File -Recurse|ForEach-Object{$_.IsReadOnly=$true}
    $null=Invoke-MoaCli $data @('index',$documents)

    $first=Start-ExplorerOpen $data $selected
    $second=Start-ExplorerOpen $data $sibling
    $one=Complete-ExplorerOpen $first
    $two=Complete-ExplorerOpen $second
    if(-not $one.desktop -or -not $two.desktop -or $one.pid -ne $two.pid){throw '동시 Explorer 요청이 단일 데스크톱 앱으로 합쳐지지 않았습니다.'}
    $status=Invoke-MoaCli $data @('instance')|ConvertFrom-Json
    if(-not $status.ready -or $status.pid -ne $one.pid -or $status.app -ne 'moasearch'){throw '앱 상태 named pipe 응답이 올바르지 않습니다.'}
    if((Get-ScopedCount $selected)-ne 2 -or (Get-ScopedCount $sibling)-ne 1){throw '폴더별 검색 결과가 다른 폴더로 누출되었습니다.'}
    $listeners=@(Get-NetTCPConnection -State Listen -ErrorAction Stop|Where-Object{$_.OwningProcess -eq [int]$one.pid})
    if($listeners.Count -ne 0){throw '앱 프로세스가 TCP listener를 열었습니다.'}

    $other=Complete-ExplorerOpen (Start-ExplorerOpen $otherData $selected)
    if(-not $other.desktop -or $other.pid -eq $one.pid){throw '데이터 위치가 다른 앱이 독립 실행되지 않았습니다.'}
    $report=[pscustomobject]@{
        desktopWindow=$true
        namedPipeStatus=$true
        concurrentSingleInstance=$true
        differentDataIsolated=($other.pid -ne $one.pid)
        tcpListeners=0
        scopedResults=@{selected=2;sibling=1}
        pids=@([int]$one.pid,[int]$other.pid)
        fixtures=$fixtureRoot
    }
} catch {
    $failure=$_.Exception.Message
} finally {
    foreach($request in @($requests)){
        if(-not $request.HasExited){try{$request.Kill();$request.WaitForExit()}catch{[void]$cleanupErrors.Add("Explorer 요청을 종료하지 못했습니다: $_")}}
        $request.Dispose()
    }
    foreach($candidateData in $dataDirectories){
        try {
            $statusText=Invoke-MoaCli $candidateData @('instance')
            $status=$statusText|ConvertFrom-Json
            if(-not $status.pid){continue}
            $candidatePath=[IO.Path]::GetFullPath([string]$candidateData).TrimEnd('\','/')
            $reportedPath=[IO.Path]::GetFullPath([string]$status.data_dir).TrimEnd('\','/')
            if(-not [string]::Equals($candidatePath,$reportedPath,[StringComparison]::OrdinalIgnoreCase)){
                throw '검증 앱의 데이터 경로가 일치하지 않아 종료하지 않았습니다.'
            }
            $process=Get-Process -Id ([int]$status.pid) -ErrorAction Stop
            if(-not [string]::Equals([IO.Path]::GetFullPath($process.Path),$exe,[StringComparison]::OrdinalIgnoreCase)){
                throw '검증 앱 실행 파일이 일치하지 않아 종료하지 않았습니다.'
            }
            $null=Invoke-MoaCli $candidateData @('quit')
            $deadline=[DateTime]::UtcNow.AddSeconds(10)
            while([DateTime]::UtcNow-lt$deadline-and(Get-Process -Id ([int]$status.pid) -ErrorAction SilentlyContinue)){
                Start-Sleep -Milliseconds 150
            }
            if(Get-Process -Id ([int]$status.pid) -ErrorAction SilentlyContinue){
                throw "검증 앱 PID $($status.pid)가 종료되지 않았습니다."
            }
        } catch {
            [void]$cleanupErrors.Add("검증 앱 정리에 실패했습니다($candidateData): $($_.Exception.Message)")
        }
    }
}

if($cleanupErrors.Count){throw ($cleanupErrors -join [Environment]::NewLine)}
if($failure){throw $failure}
$report|ConvertTo-Json -Depth 4|Set-Content -LiteralPath (Join-Path $fixtureRoot 'report.json') -Encoding utf8
$report|ConvertTo-Json -Depth 4
Write-Output "Explorer named pipe 회귀 검증 통과. 테스트 자료: $fixtureRoot"
