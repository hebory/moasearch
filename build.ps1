param([switch]$Test, [switch]$PrepareOnly)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Rust MSVC 도구 체인을 설치하세요: https://rustup.rs/'
}
# Configure the installed Microsoft C++ compiler without changing global PATH.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path -LiteralPath $vswhere) {
    $vsRoot = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($vsRoot) {
        $devCmd = Join-Path $vsRoot 'Common7\Tools\VsDevCmd.bat'
        $environmentLines = & $env:ComSpec /s /c "`"$devCmd`" -no_logo -arch=x64 -host_arch=x64 && set"
        if ($LASTEXITCODE -ne 0) { throw 'Visual Studio 빌드 환경 초기화에 실패했습니다.' }
        foreach ($line in $environmentLines) {
            if ($line -match '^([^=]+)=(.*)$') { [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process') }
        }
        $vsCmake = Join-Path $vsRoot 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
        if (Test-Path -LiteralPath (Join-Path $vsCmake 'cmake.exe')) { $env:PATH = "$vsCmake;$env:PATH" }
        $vsNinja = Join-Path $vsRoot 'Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja'
        if (Test-Path -LiteralPath (Join-Path $vsNinja 'ninja.exe')) { $env:PATH = "$vsNinja;$env:PATH" }
    }
}
if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) { throw 'CMake를 설치하고 PATH에 추가하세요.' }
if (-not (Get-Command ninja -ErrorAction SilentlyContinue)) { throw 'Ninja를 설치하거나 Visual Studio CMake 도구를 추가하세요.' }
# Avoid MSBuild structured compiler-output decoding errors on Korean Windows.
$env:CMAKE_GENERATOR = 'Ninja'
$env:VSLANG = '1033'
if (-not $env:CMAKE_BUILD_PARALLEL_LEVEL) { $env:CMAKE_BUILD_PARALLEL_LEVEL = '4' }
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = '4' }
if (-not $env:LIBCLANG_PATH) {
    foreach ($candidate in @((Join-Path $PSScriptRoot '.moasearch-native\libclang\clang\native'), (Join-Path $env:ProgramFiles 'LLVM\bin'))) {
        if (Test-Path -LiteralPath (Join-Path $candidate 'libclang.dll')) { $env:LIBCLANG_PATH=$candidate; break }
    }
}
if (-not $env:LIBCLANG_PATH -or -not (Test-Path -LiteralPath (Join-Path $env:LIBCLANG_PATH 'libclang.dll'))) {
    throw 'LLVM을 설치하거나 libclang.dll 폴더를 LIBCLANG_PATH 환경 변수로 지정하세요.'
}
# Use Microsoft's CPU DLL: static third-party ORT builds require a newer MSVC
# runtime than many VS 2022 installations. The official archive is hash pinned.
$ortArchive = Join-Path $PSScriptRoot '.moasearch-native\onnxruntime-win-x64-1.28.0.zip'
$ortRoot = Join-Path $PSScriptRoot '.moasearch-native\onnxruntime-win-x64-1.28.0'
$ortHash = 'abef733dacbe2f571547a7150b479b5cb9cc0df22f96c24983a42cadb1b4f8bc'
if (-not (Test-Path -LiteralPath $ortArchive)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ortArchive) | Out-Null
    Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/onnxruntime-win-x64-1.28.0.zip' -OutFile $ortArchive
}
if ((Get-FileHash -LiteralPath $ortArchive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ortHash) {
    throw 'ONNX Runtime 다운로드 SHA-256 검증에 실패했습니다.'
}
Expand-Archive -LiteralPath $ortArchive -DestinationPath (Split-Path -Parent $ortArchive) -Force
$env:ORT_LIB_PATH = Join-Path $ortRoot 'lib'
$env:ORT_PREFER_DYNAMIC_LINK = '1'
# Windows searches System32 before PATH. Tests/examples need the pinned DLLs
# beside their executables as well, otherwise the older system ORT can win.
foreach ($profile in @('debug','release')) {
    foreach ($subdir in @('','deps','examples')) {
        $runtimeDir = Join-Path (Join-Path $PSScriptRoot "target\$profile") $subdir
        New-Item -ItemType Directory -Force -Path $runtimeDir | Out-Null
        Get-ChildItem -LiteralPath $env:ORT_LIB_PATH -Filter '*.dll' -File | ForEach-Object {
            Copy-Item -LiteralPath $_.FullName -Destination $runtimeDir -Force
        }
    }
}
if ($PrepareOnly) { return }
# Cargo/libgit2 can exceed MAX_PATH while fetching llama.cpp's unused web UI.
# Fetch through a temporary drive alias; compile through the normal path.
$savedCargoHome = $env:CARGO_HOME
$cargoCache = if ($savedCargoHome) { [System.IO.Path]::GetFullPath($savedCargoHome) } else { Join-Path $env:USERPROFILE '.cargo' }
New-Item -ItemType Directory -Force -Path $cargoCache | Out-Null
$fetchDrive = $null
try {
    foreach ($letter in @('Z','Y','X','W','V','U','T','S','R','Q','P')) {
        if (-not (Get-PSDrive -Name $letter -ErrorAction SilentlyContinue) -and -not (Test-Path -LiteralPath "${letter}:\")) {
            & subst.exe "${letter}:" $cargoCache
            if ($LASTEXITCODE -eq 0) { $fetchDrive=$letter; $env:CARGO_HOME="${letter}:\"; break }
        }
    }
    cargo fetch --locked
    if ($LASTEXITCODE -ne 0) { throw 'Rust 의존성 다운로드에 실패했습니다.' }
} finally {
    $env:CARGO_HOME = $savedCargoHome
    if ($fetchDrive) { & subst.exe "${fetchDrive}:" /D }
}
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Rust 빌드에 실패했습니다.' }
if ($Test) {
    if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
        throw '화면 IPC 검증에는 Node.js 18 이상이 필요합니다.'
    }
    node --test tests/frontend_bridge.test.cjs
    if ($LASTEXITCODE -ne 0) { throw '화면 IPC 검증에 실패했습니다.' }
    $env:PATH = "$(Join-Path $PSScriptRoot 'target\release');$env:PATH"
    cargo test --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Rust 검증에 실패했습니다.' }
}
New-Item -ItemType Directory -Force -Path (Join-Path $PSScriptRoot 'dist') | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'target\release\moasearch.exe') -Destination (Join-Path $PSScriptRoot 'dist\moasearch.exe') -Force
# ONNX Runtime is a native library used by the Rust OCR pipeline.
Get-ChildItem -LiteralPath (Join-Path $ortRoot 'lib') -Filter '*.dll' -File | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $PSScriptRoot 'dist') -Force
}
Copy-Item -LiteralPath (Join-Path $ortRoot 'LICENSE') -Destination (Join-Path $PSScriptRoot 'dist\ONNX-Runtime-LICENSE') -Force
Copy-Item -LiteralPath (Join-Path $ortRoot 'ThirdPartyNotices.txt') -Destination (Join-Path $PSScriptRoot 'dist\ONNX-Runtime-ThirdPartyNotices.txt') -Force
Write-Host 'Rust 실행 파일 준비 완료: dist\moasearch.exe'
