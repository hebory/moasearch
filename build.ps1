param([switch]$Test, [switch]$PrepareOnly, [switch]$CpuOnly, [string]$TestFilter)
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
# The packaged build includes CUDA plus llama.cpp's CPU fallback. A CPU-only
# build remains available for development without the CUDA Toolkit installed.
$gpuFeatureArgs = @()
if (-not $CpuOnly) {
    $cudaRoot = $env:CUDA_PATH
    if (-not $cudaRoot -or -not (Test-Path -LiteralPath (Join-Path $cudaRoot 'bin\nvcc.exe'))) {
        $cudaCandidates = @(Get-ChildItem -LiteralPath (Join-Path $env:ProgramFiles 'NVIDIA GPU Computing Toolkit\CUDA') -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending)
        $cudaRoot = $cudaCandidates | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin\nvcc.exe') } | Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $cudaRoot) { throw '설치용 CUDA GPU 빌드에는 NVIDIA CUDA Toolkit과 nvcc가 필요합니다.' }
    $env:CUDA_PATH = $cudaRoot
    $cudaRuntimeDirectory = Join-Path $cudaRoot 'bin\x64'
    $cudaRuntimeFiles = @('cudart64_13.dll','cublas64_13.dll','cublasLt64_13.dll')
    foreach ($runtimeName in $cudaRuntimeFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $cudaRuntimeDirectory $runtimeName) -PathType Leaf)) {
            throw "CUDA 재배포 runtime DLL이 없습니다: $runtimeName"
        }
    }
    $cudaLicense = Join-Path $cudaRoot 'LICENSE'
    if (-not (Test-Path -LiteralPath $cudaLicense -PathType Leaf)) { throw 'CUDA Toolkit LICENSE를 찾을 수 없습니다.' }
    $gpuFeatureArgs = @('--features','gpu')
    # Build native code for current Turing/Ampere GPUs plus forward-compatible
    # PTX so newer NVIDIA devices can JIT the kernels at runtime.
    $env:CMAKE_CUDA_ARCHITECTURES = '75-real;86-real;86-virtual'
}
# Avoid MSBuild structured compiler-output decoding errors on Korean Windows.
$env:CMAKE_GENERATOR = 'Ninja'
$env:VSLANG = '1033'
if (-not $env:CMAKE_BUILD_PARALLEL_LEVEL) { $env:CMAKE_BUILD_PARALLEL_LEVEL = '8' }
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
        if (-not $CpuOnly) {
            foreach ($runtimeName in $cudaRuntimeFiles) {
                Copy-Item -LiteralPath (Join-Path $cudaRuntimeDirectory $runtimeName) -Destination $runtimeDir -Force
            }
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
$buildCommand = @('build','--release') + $gpuFeatureArgs + @('--locked')
cargo @buildCommand
if ($LASTEXITCODE -ne 0) { throw 'Rust 빌드에 실패했습니다.' }
if ($Test -or $TestFilter) {
    if (-not $TestFilter -and -not (Get-Command node -ErrorAction SilentlyContinue)) {
        throw '화면 IPC 검증에는 Node.js 18 이상이 필요합니다.'
    }
    if (-not $TestFilter) {
        node --test tests/frontend_bridge.test.cjs tests/frontend_app.test.cjs
        if ($LASTEXITCODE -ne 0) { throw '화면 IPC 검증에 실패했습니다.' }
    }
    $env:PATH = "$(Join-Path $PSScriptRoot 'target\release');$env:PATH"
    $testCommand = @('test','--release') + $gpuFeatureArgs + @('--locked')
    if ($TestFilter) { $testCommand += @($TestFilter) }
    cargo @testCommand
    if ($LASTEXITCODE -ne 0) { throw 'Rust 검증에 실패했습니다.' }
}
New-Item -ItemType Directory -Force -Path (Join-Path $PSScriptRoot 'dist') | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'target\release\moasearch.exe') -Destination (Join-Path $PSScriptRoot 'dist\moasearch.exe') -Force
# ONNX Runtime is a native library used by the Rust OCR pipeline.
Get-ChildItem -LiteralPath (Join-Path $ortRoot 'lib') -Filter '*.dll' -File | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $PSScriptRoot 'dist') -Force
}
# CUDA 13 runtime DLLs are NVIDIA redistributables required by the linked
# CUDA/cuBLAS backend; no CUDA Toolkit installation is needed by end users.
if (-not $CpuOnly) {
    foreach ($runtimeName in $cudaRuntimeFiles) {
        Copy-Item -LiteralPath (Join-Path $cudaRuntimeDirectory $runtimeName) -Destination (Join-Path $PSScriptRoot 'dist') -Force
    }
    Copy-Item -LiteralPath $cudaLicense -Destination (Join-Path $PSScriptRoot 'dist\CUDA-Runtime-LICENSE') -Force
} else {
    foreach ($runtimeName in @('cudart64_13.dll','cublas64_13.dll','cublasLt64_13.dll','CUDA-Runtime-LICENSE')) {
        $staleRuntime = Join-Path $PSScriptRoot "dist\$runtimeName"
        if (Test-Path -LiteralPath $staleRuntime -PathType Leaf) { Remove-Item -LiteralPath $staleRuntime -Force }
    }
    Write-Host 'CPU-only 빌드: CUDA runtime DLL을 배포 폴더에 추가하지 않았습니다.'
}
Copy-Item -LiteralPath (Join-Path $ortRoot 'LICENSE') -Destination (Join-Path $PSScriptRoot 'dist\ONNX-Runtime-LICENSE') -Force
Copy-Item -LiteralPath (Join-Path $ortRoot 'ThirdPartyNotices.txt') -Destination (Join-Path $PSScriptRoot 'dist\ONNX-Runtime-ThirdPartyNotices.txt') -Force
if ($CpuOnly) { Write-Host 'CPU-only 실행 파일 준비 완료: dist\moasearch.exe' }
else { Write-Host 'CUDA 가속 및 CPU fallback 실행 파일 준비 완료: dist\moasearch.exe' }
