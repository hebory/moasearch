#Requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$releaseRoot = Join-Path $repositoryRoot '.moasearch-native\release-licenses'
$cargoLock = Join-Path $repositoryRoot 'Cargo.lock'
$cargo = Get-Command cargo -CommandType Application -ErrorAction Stop

if (!(Test-Path -LiteralPath $cargoLock -PathType Leaf)) {
    throw "Cargo.lock not found: $cargoLock"
}

$lockHash = (Get-FileHash -LiteralPath $cargoLock -Algorithm SHA256).Hash.ToLowerInvariant()
$runName = 'run-{0}-{1}-{2}' -f $lockHash.Substring(0, 12), [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ'), $PID
$outputDirectory = Join-Path $releaseRoot $runName
$scratchDirectory = Join-Path (Join-Path $repositoryRoot '.moasearch-native') ('.license-metadata-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $outputDirectory, $scratchDirectory -Force | Out-Null

$metadataPath = Join-Path $scratchDirectory 'cargo-metadata.json'
$errorPath = Join-Path $scratchDirectory 'cargo-metadata.stderr'
$arguments = @('metadata', '--locked', '--offline', '--format-version', '1', '--filter-platform', 'x86_64-pc-windows-msvc')
$metadataProcess = Start-Process -FilePath $cargo.Source -ArgumentList $arguments -WorkingDirectory $repositoryRoot `
    -WindowStyle Hidden -Wait -PassThru -RedirectStandardOutput $metadataPath -RedirectStandardError $errorPath
if ($metadataProcess.ExitCode -ne 0) {
    $detail = if (Test-Path -LiteralPath $errorPath) { Get-Content -LiteralPath $errorPath -Raw } else { '' }
    throw "Offline Cargo metadata failed ($($metadataProcess.ExitCode)): $detail"
}

$metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
if (!$metadata.resolve -or !$metadata.resolve.nodes) {
    throw 'Cargo metadata did not return a resolved dependency graph.'
}

# Restrict output to the workspace and its lock-resolved runtime/build graph.
$workspaceManifest = [System.IO.Path]::GetFullPath((Join-Path $repositoryRoot 'Cargo.toml'))
$rootPackage = @($metadata.packages | Where-Object {
    [System.IO.Path]::GetFullPath($_.manifest_path).Equals($workspaceManifest, [StringComparison]::OrdinalIgnoreCase)
}) | Select-Object -First 1
if (!$rootPackage) { throw "Workspace package was not found in Cargo metadata: $workspaceManifest" }

$nodesById = @{}
$packagesById = @{}
foreach ($node in $metadata.resolve.nodes) { $nodesById[$node.id] = $node }
foreach ($package in $metadata.packages) { $packagesById[$package.id] = $package }
if (!$nodesById.ContainsKey($rootPackage.id)) { throw 'Workspace root is missing from the resolved Cargo dependency graph.' }

$reachable = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$pending = [System.Collections.Generic.Queue[string]]::new()
$null = $reachable.Add($rootPackage.id)
$pending.Enqueue($rootPackage.id)
while ($pending.Count -gt 0) {
    $nodeId = $pending.Dequeue()
    foreach ($dependency in $nodesById[$nodeId].deps) {
        # Keep normal and build dependencies; dev-only crates never ship.
        $shipsWithBuild = @($dependency.dep_kinds | Where-Object { !$_.kind -or $_.kind -ne 'dev' }).Count -gt 0
        if (!$shipsWithBuild -or !$nodesById.ContainsKey($dependency.pkg)) { continue }
        if ($reachable.Add($dependency.pkg)) { $pending.Enqueue($dependency.pkg) }
    }
}

$packages = @($reachable | Where-Object { $_ -ne $rootPackage.id } |
    ForEach-Object { $packagesById[$_] } | Where-Object { $_ } |
    Sort-Object -Property name, version, source)
$safeNames = @{}
foreach ($package in $packages) {
    $baseName = ('{0}-{1}' -f $package.name, $package.version) -replace '[^A-Za-z0-9._+-]', '_'
    if ($safeNames.ContainsKey($baseName)) { $safeNames[$baseName]++ } else { $safeNames[$baseName] = 1 }
}

$notices = [System.Collections.Generic.List[string]]::new()
$notices.Add('THIRD-PARTY NOTICES')
$notices.Add('')
$notices.Add('MoaSearch release dependency licenses. Package versions are resolved from Cargo.lock; the metadata graph is filtered for x86_64-pc-windows-msvc and excludes dev-only dependencies.')
$notices.Add('')
$warnings = [System.Collections.Generic.List[string]]::new()
$collectedFiles = 0
$licenseName = '^(?i:LICENSE|LICENCE|COPYING|NOTICE)(?:[-_.].*)?$'
$maxTextBytes = 4MB
$packageOutputNames = @{}
$licensesDirectory = Join-Path $outputDirectory 'licenses'

foreach ($package in $packages) {
    $manifestPath = [System.IO.Path]::GetFullPath($package.manifest_path)
    $packageDirectory = Split-Path -Parent $manifestPath
    if (!(Test-Path -LiteralPath $packageDirectory -PathType Container)) {
        $warnings.Add("$($package.name) $($package.version): manifest directory not found: $packageDirectory")
        continue
    }
    $packageDirectory = [System.IO.Path]::GetFullPath($packageDirectory)
    $packageDirectoryPrefix = $packageDirectory.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    $declaredLicensePath = $null
    if ($package.license_file) {
        $licenseFileValue = [string]$package.license_file
        $declaredLicensePath = if ([System.IO.Path]::IsPathRooted($licenseFileValue)) {
            [System.IO.Path]::GetFullPath($licenseFileValue)
        } else {
            [System.IO.Path]::GetFullPath((Join-Path $packageDirectory $licenseFileValue))
        }
        if (!$declaredLicensePath.StartsWith($packageDirectoryPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            $warnings.Add("$($package.name) $($package.version): declared license-file escapes the package directory: $($package.license_file)")
            $declaredLicensePath = $null
        }
    }

    $relativeLicensePaths = [System.Collections.Generic.List[string]]::new()
    $packageOutputName = '{0}-{1}' -f $package.name, $package.version
    if ($safeNames[$packageOutputName] -gt 1) {
        $idHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($package.id))).ToLowerInvariant().Substring(0, 8)
        $packageOutputName = "$packageOutputName-$idHash"
    }
    $packageOutputNames[$package.id] = $packageOutputName
    $packageOutputDirectory = Join-Path $licensesDirectory $packageOutputName
    $directories = [System.Collections.Generic.Stack[string]]::new()
    $directories.Push($packageDirectory)

    while ($directories.Count -gt 0) {
        $currentDirectory = $directories.Pop()
        foreach ($entry in Get-ChildItem -LiteralPath $currentDirectory -Force -ErrorAction SilentlyContinue) {
            if ($entry.PSIsContainer) {
                if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
                $relativeDirectory = [System.IO.Path]::GetRelativePath($packageDirectory, $entry.FullName)
                # llama.cpp ships model-server UI sources in this checkout; their
                # npm vendor tree is not linked into the Windows application.
                if ($relativeDirectory -match '(?i)(^|[\\/])node_modules([\\/]|$)') { continue }
                if ($package.name -eq 'llama-cpp-sys-2' -and
                    $relativeDirectory -match '(?i)(^|[\\/])llama\.cpp([\\/](tools[\\/]ui|node_modules)([\\/]|$)|[\\/]tools[\\/]ui[\\/]src[\\/]lib[\\/]vendors([\\/]|$))') {
                    continue
                }
                $directories.Push($entry.FullName)
                continue
            }
            $isDeclaredLicenseFile = $declaredLicensePath -and $entry.FullName.Equals($declaredLicensePath, [StringComparison]::OrdinalIgnoreCase)
            if (($entry.Name -notmatch $licenseName -and !$isDeclaredLicenseFile) -or
                $entry.Length -gt $maxTextBytes -or
                ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }

            $bytes = [System.IO.File]::ReadAllBytes($entry.FullName)
            if ($bytes -contains 0) { continue }
            $relativePath = [System.IO.Path]::GetRelativePath($packageDirectory, $entry.FullName).Replace([char]92, '/')
            if (!$relativePath -or $relativePath.StartsWith('../', [StringComparison]::Ordinal)) { continue }

            $destination = Join-Path $packageOutputDirectory $relativePath
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            [System.IO.File]::WriteAllBytes($destination, $bytes)
            $relativeLicensePaths.Add($relativePath)
            $collectedFiles++
        }
    }

    if ($package.name -in @('llama-cpp-2','llama-cpp-sys-2')) {
        # The Rust wrapper declares the licenses in the parent Git checkout,
        # outside its crate manifest directory.
        $checkoutRoot = Split-Path -Parent $packageDirectory
        foreach ($name in @('LICENSE-APACHE','LICENSE-MIT')) {
            $source = Join-Path $checkoutRoot $name
            if (!(Test-Path -LiteralPath $source -PathType Leaf)) { continue }
            $sourceItem = Get-Item -LiteralPath $source
            if (($sourceItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $sourceItem.Length -gt $maxTextBytes) { continue }
            $bytes = [System.IO.File]::ReadAllBytes($source)
            if ($bytes -contains 0) { continue }
            $relativePath = "llama-cpp-rs-root/$name"
            $destination = Join-Path (Join-Path $packageOutputDirectory 'llama-cpp-rs-root') $name
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            [System.IO.File]::WriteAllBytes($destination, $bytes)
            $relativeLicensePaths.Add($relativePath)
            $collectedFiles++
        }
    }

    # Fill license files that are absent from the published Cargo crate with
    # verified upstream files and provenance maintained in packaging/. Only
    # text license names are copied; SOURCE manifests are included for audit.
    $overrideDirectory = Join-Path $PSScriptRoot (Join-Path 'license-overrides' $packageOutputName)
    if (Test-Path -LiteralPath $overrideDirectory -PathType Container) {
        foreach ($overrideFile in Get-ChildItem -LiteralPath $overrideDirectory -File -Force) {
            if (($overrideFile.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $overrideFile.Length -gt $maxTextBytes) { continue }
            $isSourceManifest = $overrideFile.Name -eq 'SOURCES.json'
            if ($overrideFile.Name -notmatch $licenseName -and !$isSourceManifest) { continue }
            $bytes = [System.IO.File]::ReadAllBytes($overrideFile.FullName)
            if ($bytes -contains 0) { continue }
            $relativePath = "upstream-overrides/$($overrideFile.Name)"
            $destination = Join-Path (Join-Path $packageOutputDirectory 'upstream-overrides') $overrideFile.Name
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            [System.IO.File]::WriteAllBytes($destination, $bytes)
            if ($isSourceManifest) {
                $notices.Add("Upstream source metadata: licenses/$packageOutputName/$relativePath")
            } else {
                $relativeLicensePaths.Add($relativePath)
                $collectedFiles++
            }
        }
    }

    $spdx = if ($package.license) { $package.license } elseif ($package.license_file) { "See $($package.license_file)" } else { 'NOT DECLARED' }
    $repository = if ($package.repository) { $package.repository } elseif ($package.source) { $package.source } else { 'NOT DECLARED' }
    $notices.Add("Package: $($package.name) $($package.version)")
    $notices.Add("SPDX: $spdx")
    $notices.Add("Repository: $repository")
    $notices.Add('Source license paths:')
    if ($relativeLicensePaths.Count -gt 0) {
        foreach ($path in $relativeLicensePaths | Sort-Object -Unique) {
            $notices.Add("  licenses/$($packageOutputName)/$path")
        }
    } else {
        $notices.Add('  NONE FOUND')
        $warning = "$($package.name) $($package.version): no text license file matching LICENSE*, LICENCE*, COPYING*, or NOTICE* was found. SPDX: $spdx"
        $warnings.Add($warning)
        $notices.Add("WARNING: $warning")
    }
    if (!$package.license -and !$package.license_file) {
        $warning = "$($package.name) $($package.version): Cargo metadata has no SPDX license or license-file declaration."
        $warnings.Add($warning)
        $notices.Add("WARNING: $warning")
    }
    $notices.Add('')
}

# The llama-cpp-sys package also compiles its pinned llama.cpp git submodule,
# which is outside the Rust crate manifest directory. Include the upstream and
# ggml notices as source-path entries in that package section.
$llamaPackage = @($packages | Where-Object { $_.name -eq 'llama-cpp-sys-2' }) | Select-Object -First 1
if ($llamaPackage) {
    $llamaPackageDirectory = Split-Path -Parent $llamaPackage.manifest_path
    $llamaSource = Join-Path $llamaPackageDirectory 'llama.cpp'
    $submoduleEntries = @('LICENSE','ggml/LICENSE')
    $submoduleOutput = Join-Path $licensesDirectory $packageOutputNames[$llamaPackage.id]
    foreach ($relative in $submoduleEntries) {
        $source = Join-Path $llamaSource ($relative -replace '/', '\')
        if (!(Test-Path -LiteralPath $source -PathType Leaf)) {
            if ($relative -eq 'ggml/LICENSE') {
                $rootLicense = Join-Path $llamaSource 'LICENSE'
                if (Test-Path -LiteralPath $rootLicense -PathType Leaf) {
                    $rootLicenseText = Get-Content -LiteralPath $rootLicense -Raw
                    if ($rootLicenseText -match '(?i)MIT License' -and $rootLicenseText -match '(?i)ggml authors') {
                        $notices.Add(('Bundled source license attribution: licenses/{0}/llama.cpp/LICENSE also applies to bundled ggml; the root MIT license names the ggml authors.' -f $packageOutputNames[$llamaPackage.id]))
                        continue
                    }
                }
            }
            $warnings.Add("llama-cpp-sys-2: expected bundled llama.cpp license was not found: llama.cpp/$relative")
            $notices.Add("WARNING: expected bundled llama.cpp license was not found: llama.cpp/$relative")
            continue
        }
        $target = Join-Path (Join-Path $submoduleOutput 'llama.cpp') ($relative -replace '/', [string][char]92)
        New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
        Copy-Item -LiteralPath $source -Destination $target
        $notices.Add(("Bundled native source license: licenses/{0}/llama.cpp/{1}" -f $packageOutputNames[$llamaPackage.id], $relative))
        $collectedFiles++
    }
}

if ($warnings.Count -gt 0) {
    $notices.Add('WARNINGS:')
    foreach ($warning in $warnings | Sort-Object -Unique) { $notices.Add("- $warning") }
}

$noticesPath = Join-Path $outputDirectory 'THIRD-PARTY-NOTICES.txt'
[System.IO.File]::WriteAllText($noticesPath, (($notices -join "`r`n") + "`r`n"), [System.Text.UTF8Encoding]::new($false))

# Do not retain Cargo metadata: it contains machine-specific absolute paths.
Remove-Item -LiteralPath $metadataPath, $errorPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $scratchDirectory -Force -ErrorAction SilentlyContinue

[pscustomobject]@{
    lockSha256 = $lockHash
    target = 'x86_64-pc-windows-msvc'
    packageCount = $packages.Count
    licenseFileCount = $collectedFiles
    warningCount = ($warnings | Sort-Object -Unique).Count
    warnings = @($warnings | Sort-Object -Unique)
    outputDirectory = $outputDirectory
    noticesFile = $noticesPath
} | ConvertTo-Json -Depth 5 -Compress
