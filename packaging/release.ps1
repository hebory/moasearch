#Requires -Version 7.0

param(
    [string]$Version,
    [string]$IsccPath,
    [string]$CrtDirectory,
    [string]$LicenseDirectory,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
$repositoryRoot = (Resolve-Path -LiteralPath $PSScriptRoot\..).Path
$distributionDirectory = Join-Path $repositoryRoot 'dist'
$artifactDirectory = Join-Path $distributionDirectory 'artifacts'
$iconPath = Join-Path $repositoryRoot 'assets\branding\moasearch.ico'
$launcherIconPath = Join-Path $repositoryRoot 'icon.png'
$launcherCoverPath = Join-Path $repositoryRoot 'cover.png'
$launcherMetadataPath = Join-Path $repositoryRoot 'moalauncher.json'
$defaultLicenseDirectory = Join-Path $repositoryRoot '.moasearch-native\release-licenses'
$installerSource = Join-Path $PSScriptRoot 'moasearch.iss'
if (-not $LicenseDirectory) { $LicenseDirectory = $defaultLicenseDirectory }

if (-not $Version) {
    $manifest = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
    $versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"\s*$')
    if (-not $versionMatch.Success) { throw 'Cargo.toml에서 앱 버전을 읽지 못했습니다.' }
    $Version = $versionMatch.Groups[1].Value
}
if ($Version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$') {
    throw "올바른 SemVer 버전이 아닙니다: $Version"
}
$numericVersion = ([regex]::Match($Version, '^\d+(?:\.\d+){0,3}')).Value
$fileVersion = $numericVersion
while (($fileVersion.Split('.').Count) -lt 4) { $fileVersion += '.0' }

if (-not (Test-Path -LiteralPath $iconPath -PathType Leaf)) {
    throw "설치기 아이콘을 찾을 수 없습니다: $iconPath"
}
$iconHeader = [System.IO.File]::ReadAllBytes($iconPath)
if ($iconHeader.Length -lt 22 -or $iconHeader[0] -ne 0 -or $iconHeader[1] -ne 0 -or $iconHeader[2] -ne 1 -or $iconHeader[3] -ne 0) {
    throw 'assets/branding/moasearch.ico 파일의 ICO 헤더를 확인하세요.'
}
foreach ($pngPath in @($launcherIconPath, $launcherCoverPath)) {
    if (-not (Test-Path -LiteralPath $pngPath -PathType Leaf)) { throw "Moa Launcher PNG 자산이 없습니다: $pngPath" }
    $png = [System.IO.File]::ReadAllBytes($pngPath)
    $pngSignature = @(137,80,78,71,13,10,26,10)
    if ($png.Length -lt 24) { throw "잘못된 PNG 파일입니다: $pngPath" }
    for ($index = 0; $index -lt $pngSignature.Count; $index++) {
        if ($png[$index] -ne $pngSignature[$index]) { throw "PNG 시그니처가 올바르지 않습니다: $pngPath" }
    }
    $width = ([uint32]$png[16] -shl 24) -bor ([uint32]$png[17] -shl 16) -bor ([uint32]$png[18] -shl 8) -bor [uint32]$png[19]
    $height = ([uint32]$png[20] -shl 24) -bor ([uint32]$png[21] -shl 16) -bor ([uint32]$png[22] -shl 8) -bor [uint32]$png[23]
    if (-not $width -or -not $height) { throw "PNG 이미지 크기가 0입니다: $pngPath" }
}
if (-not (Test-Path -LiteralPath $launcherMetadataPath -PathType Leaf)) { throw 'Moa Launcher 등록 파일을 찾을 수 없습니다: moalauncher.json' }
try {
    $launcherMetadata = Get-Content -LiteralPath $launcherMetadataPath -Raw | ConvertFrom-Json
} catch {
    throw "Moa Launcher 등록 JSON을 읽지 못했습니다: $($_.Exception.Message)"
}
$launcherApp = @($launcherMetadata.apps | Where-Object { $_.id -ceq 'moasearch' })
$launcherInstallerPackages = @($launcherApp[0].packages | Where-Object {
    $_.kind -ceq 'inno' -and $_.assetPattern -ceq 'moasearch-*-win-x64-setup.exe' -and
    $_.executablePath -ceq 'moasearch.exe' -and $_.installDirectory -ceq 'Programs/moasearch'
})
if ($launcherMetadata.schemaVersion -ne 1 -or $launcherApp.Count -ne 1 -or
    $launcherApp[0].repository -cne 'hebory/moasearch' -or
    $launcherInstallerPackages.Count -ne 1) {
    throw 'moalauncher.json의 moasearch 카탈로그 설치 패키지 항목이 릴리스 설치기와 일치하지 않습니다.'
}

if (-not $CrtDirectory) {
    $CrtDirectory = Join-Path $env:SystemRoot 'System32'
}

$LicenseDirectory = (Resolve-Path -LiteralPath $LicenseDirectory -ErrorAction Stop).Path
if (-not (Test-Path -LiteralPath (Join-Path $LicenseDirectory 'THIRD-PARTY-NOTICES.txt') -PathType Leaf) -or
    -not (Test-Path -LiteralPath (Join-Path $LicenseDirectory 'licenses') -PathType Container)) {
    throw "제3자 라이선스 수집 결과가 올바르지 않습니다: $LicenseDirectory"
}

$CrtDirectory = (Resolve-Path -LiteralPath $CrtDirectory -ErrorAction Stop).Path
$crtNames = @('MSVCP140.dll', 'VCRUNTIME140.dll', 'VCRUNTIME140_1.dll')
$crtInventory = @()
$crtVersion = $null
foreach ($name in $crtNames) {
    $crtFile = Join-Path $CrtDirectory $name
    if (-not (Test-Path -LiteralPath $crtFile -PathType Leaf)) { throw "App-local CRT 파일을 찾을 수 없습니다: $crtFile" }
    $signature = Get-AuthenticodeSignature -LiteralPath $crtFile
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
        throw "App-local CRT 파일의 Microsoft 서명이 유효하지 않습니다: $crtFile ($($signature.Status))"
    }
    $fileInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($crtFile)
    if (-not $fileInfo.ProductVersion) { throw "App-local CRT 파일에 제품 버전이 없습니다: $crtFile" }
    try { $parsedVersion = [version]$fileInfo.ProductVersion } catch { throw "CRT 버전을 읽지 못했습니다: $crtFile ($($fileInfo.ProductVersion))" }
    if ($parsedVersion -lt [version]'14.39.0.0') { throw "llama.cpp에 필요한 MSVC CRT 14.39 이상이 아닙니다: $crtFile ($parsedVersion)" }
    if ($crtVersion -and $parsedVersion -ne $crtVersion) { throw "배포 CRT 세 파일 버전이 서로 다릅니다: $crtVersion, $parsedVersion" }
    $crtVersion = $parsedVersion
    $crtInventory += [ordered]@{
        file = $name
        productVersion = $fileInfo.ProductVersion
        sha256 = (Get-FileHash -LiteralPath $crtFile -Algorithm SHA256).Hash.ToLowerInvariant()
        signature = $signature.Status.ToString()
        signer = $signature.SignerCertificate.Subject
    }
}

if (-not $IsccPath) {
    $iscc = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
    if ($iscc) { $IsccPath = $iscc.Source }
}
if (-not $IsccPath -or -not (Test-Path -LiteralPath $IsccPath -PathType Leaf)) {
    throw 'Inno Setup 6/7 컴파일러 ISCC.exe를 설치하거나 -IsccPath로 지정하세요.'
}
if (-not $SkipBuild) {
    & (Join-Path $repositoryRoot 'build.ps1')
    if ($LASTEXITCODE -ne 0) { throw 'Rust 배포 빌드에 실패했습니다.' }
}

$requiredDistFiles = @(
    'moasearch.exe',
    'onnxruntime.dll',
    'onnxruntime_providers_shared.dll',
    'ONNX-Runtime-LICENSE',
    'ONNX-Runtime-ThirdPartyNotices.txt'
)
foreach ($name in $requiredDistFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $distributionDirectory $name) -PathType Leaf)) {
        throw "dist 필수 배포 파일이 없습니다: $name"
    }
}
$appExe = Join-Path $distributionDirectory 'moasearch.exe'
$executableVersion = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($appExe)
if (-not $executableVersion.ProductVersion -or $executableVersion.ProductVersion.Split('+')[0] -ne $Version) {
    throw "앱 실행 파일 버전($($executableVersion.ProductVersion))과 패키지 버전($Version)이 다릅니다."
}

New-Item -ItemType Directory -Force -Path $artifactDirectory | Out-Null
$stem = "moasearch-$Version-win-x64"
$setupName = "$stem-setup.exe"
$zipName = "$stem.zip"
if ($setupName -notlike $launcherInstallerPackages[0].assetPattern) {
    throw "Moa Launcher 설치 자산 패턴이 현재 설치기 이름과 일치하지 않습니다: $setupName"
}
$launcherAssetNames = @('icon.png', 'cover.png', 'moalauncher.json')
$releaseAssetNames = @($setupName, $zipName) + $launcherAssetNames
$checksumsPath = Join-Path $artifactDirectory 'SHA256SUMS.txt'
$manifestName = "$stem-manifest.json"
$manifestPath = Join-Path $artifactDirectory $manifestName
$setupPath = Join-Path $artifactDirectory $setupName
$zipPath = Join-Path $artifactDirectory $zipName
foreach ($path in (@($checksumsPath, $manifestPath) + @($releaseAssetNames | ForEach-Object { Join-Path $artifactDirectory $_ }))) {
    if (Test-Path -LiteralPath $path) { throw "기존 산출물을 덮어쓰지 않습니다: $path" }
}

$stagingRoot = (Resolve-Path -LiteralPath $distributionDirectory).Path
$stageDirectory = Join-Path $stagingRoot ".moasearch-release-$PID-$(Get-Random)"
New-Item -ItemType Directory -Path $stageDirectory | Out-Null
try {
    foreach ($name in $requiredDistFiles) {
        Copy-Item -LiteralPath (Join-Path $distributionDirectory $name) -Destination $stageDirectory
    }
    Copy-Item -LiteralPath (Join-Path $LicenseDirectory 'THIRD-PARTY-NOTICES.txt') -Destination $stageDirectory
    $stageLicenses = Join-Path $stageDirectory 'licenses'
    New-Item -ItemType Directory -Path $stageLicenses | Out-Null
    $licenseRoot = (Resolve-Path -LiteralPath (Join-Path $LicenseDirectory 'licenses')).Path.TrimEnd('\') + '\'
    $licenseFiles = @(Get-ChildItem -LiteralPath $licenseRoot -File -Recurse)
    if (-not $licenseFiles.Count) { throw '수집된 제3자 라이선스 파일이 없습니다.' }
    foreach ($file in $licenseFiles) {
        if ($file.Attributes -band [System.IO.FileAttributes]::ReparsePoint) { throw "제3자 라이선스에 재분석 지점은 허용되지 않습니다: $($file.FullName)" }
        if (-not $file.FullName.StartsWith($licenseRoot, [System.StringComparison]::OrdinalIgnoreCase)) { throw "제3자 라이선스가 입력 폴더 밖을 가리킵니다: $($file.FullName)" }
        $relativePath = $file.FullName.Substring($licenseRoot.Length)
        $target = Join-Path $stageLicenses $relativePath
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        Copy-Item -LiteralPath $file.FullName -Destination $target
    }
    $licenseInventory = @(
        [ordered]@{
            file = 'THIRD-PARTY-NOTICES.txt'
            bytes = (Get-Item (Join-Path $stageDirectory 'THIRD-PARTY-NOTICES.txt')).Length
            sha256 = (Get-FileHash (Join-Path $stageDirectory 'THIRD-PARTY-NOTICES.txt') -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    )
    foreach ($file in (Get-ChildItem -LiteralPath $stageLicenses -File -Recurse)) {
        $licenseInventory += [ordered]@{
            file = 'licenses/' + [System.IO.Path]::GetRelativePath($stageLicenses, $file.FullName).Replace('\','/')
            bytes = $file.Length
            sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    foreach ($name in $crtNames) {
        Copy-Item -LiteralPath (Join-Path $CrtDirectory $name) -Destination $stageDirectory
    }
    $payloadInventory = @(
        foreach ($file in (Get-ChildItem -LiteralPath $stageDirectory -File -Recurse)) {
            [ordered]@{
                file = [System.IO.Path]::GetRelativePath($stageDirectory, $file.FullName).Replace([string][char]92, '/')
                bytes = $file.Length
                sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
    )
    $stageFullPath = (Resolve-Path -LiteralPath $stageDirectory).Path
    if (-not $stageFullPath.StartsWith($stagingRoot.TrimEnd('\') + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw '릴리스 임시 경로가 dist 밖에 있습니다.'
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        $stageDirectory,
        $zipPath,
        [System.IO.Compression.CompressionLevel]::Optimal,
        $false
    )

    $isccArgs = @(
        "/DProductVersion=$Version",
        "/DProductFileVersion=$fileVersion",
        "/DPayloadDir=$stageDirectory",
        "/DBrandingIcon=$iconPath",
        "/DOutputDir=$artifactDirectory",
        "/O$artifactDirectory",
        "/F$([System.IO.Path]::GetFileNameWithoutExtension($setupName))",
        $installerSource
    )
    & $IsccPath @isccArgs
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $setupPath -PathType Leaf)) {
        throw 'Inno Setup이 설치 프로그램을 만들지 못했습니다.'
    }

    foreach ($name in $launcherAssetNames) {
        Copy-Item -LiteralPath (Join-Path $repositoryRoot $name) -Destination (Join-Path $artifactDirectory $name)
    }

    $assetRecords = foreach ($name in $releaseAssetNames) {
        $file = Join-Path $artifactDirectory $name
        [ordered]@{
            file = $name
            bytes = (Get-Item -LiteralPath $file).Length
            sha256 = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    $manifest = [ordered]@{
        schemaVersion = 1
        product = [ordered]@{ id = 'moasearch'; version = $Version; architecture = 'win-x64' }
        release = [ordered]@{ tag = "v$Version"; assets = @($assetRecords) }
        installer = [ordered]@{ format = 'Inno Setup'; version = $Version; privileges = 'current-user'; installDirectory = '%LOCALAPPDATA%\Programs\moasearch'; closesRunningAppOnUpdate = $true; launchesAfterSilentInstall = $false }
        portable = [ordered]@{ format = 'zip'; executable = 'moasearch.exe' }
        minimumSystem = [ordered]@{ operatingSystem = 'Windows 10/11 x64'; localAiCpu = 'AVX2 required' }
        packagedFiles = $payloadInventory
        licenses = @($licenseInventory)
        runtime = [ordered]@{ source = $(if ($CrtDirectory -eq (Join-Path $env:SystemRoot 'System32')) { 'Windows System32' } else { 'Supplied Microsoft-signed CRT directory' }); msvcProductVersion = $crtVersion.ToString(); minimumMsvcProductVersion = '14.39.0.0'; files = @($crtInventory) }
        models = [ordered]@{ bundled = $false; googleEmbedding = 'EmbeddingGemma 2 Q8_0'; googleChat = 'Gemma 4 E2B QAT Q4_0'; ocr = 'PP-OCRv5 Korean ONNX'; downloadAfterInstall = $true }
        branding = [ordered]@{ installerIcon = 'assets/branding/moasearch.ico'; installerIconSha256 = (Get-FileHash -LiteralPath $iconPath -Algorithm SHA256).Hash.ToLowerInvariant() }
        signature = [ordered]@{ codeSigned = $false; setupSigned = $false }
    }
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding utf8
    $sumLines = @($assetRecords | ForEach-Object { "$($_.sha256) *$($_.file)" })
    $sumLines += "$((Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()) *$manifestName"
    Set-Content -LiteralPath $checksumsPath -Value $sumLines -Encoding ascii
    Write-Output "설치 프로그램: $setupPath"
    Write-Output "포터블 ZIP: $zipPath"
    Write-Output "체크섬: $checksumsPath"
    Write-Output "릴리스 명세: $manifestPath"
    Write-Output "모아런처 이미지 및 카탈로그: $($launcherAssetNames -join ', ')"
    Write-Output "제3자 라이선스 파일: $($licenseInventory.Count)개"
}
finally {
    if (Test-Path -LiteralPath $stageDirectory) {
        $stageFullPath = (Resolve-Path -LiteralPath $stageDirectory).Path
        if ($stageFullPath.StartsWith($stagingRoot.TrimEnd('\') + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
            Remove-Item -LiteralPath $stageFullPath -Recurse -Force
        }
    }
}
