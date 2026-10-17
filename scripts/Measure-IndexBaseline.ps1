#Requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateRange(1000, 1000000)][int]$FixtureFiles = 50000,
    [ValidateRange(1, 1000)][int]$Directories = 200,
    [ValidateRange(1, 10000)][int]$Iterations = 100,
    [string]$FixtureParent,
    [string]$RealRoot,
    [string]$OutputPath,
    [switch]$KeepFixture
)

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $FixtureParent) {
    $FixtureParent = if (Test-Path -LiteralPath 'D:\Cache\Codex' -PathType Container) {
        'D:\Cache\Codex'
    } else {
        [IO.Path]::GetTempPath()
    }
}
$parentPath = (Resolve-Path -LiteralPath $FixtureParent).ProviderPath
if (-not (Test-Path -LiteralPath $parentPath -PathType Container)) {
    throw 'FixtureParent must be an existing directory.'
}
if ($RealRoot) {
    $RealRoot = (Resolve-Path -LiteralPath $RealRoot).ProviderPath
    if (-not (Test-Path -LiteralPath $RealRoot -PathType Container)) {
        throw 'RealRoot must be an existing, explicitly selected directory.'
    }
}

function Get-StorageDescription([string]$SelectedPath) {
    $driveRoot = [IO.Path]::GetPathRoot($SelectedPath)
    if ($driveRoot -notmatch '^([A-Za-z]):\\$') {
        return @{ fileSystem = 'unknown'; medium = 'unknown (network or provider path)' }
    }
    $driveLetter = $Matches[1]
    try {
        $volume = Get-Volume -DriveLetter $driveLetter
        $disk = Get-Partition -DriveLetter $driveLetter | Get-Disk
        $physical = Get-PhysicalDisk | Where-Object { $_.FriendlyName -eq $disk.FriendlyName }
        return @{
            fileSystem = [string]$volume.FileSystem
            medium = [string]$physical.MediaType
            bus = [string]$disk.BusType
            model = [string]$disk.FriendlyName
        }
    } catch {
        return @{ fileSystem = 'unknown'; medium = 'unknown'; reason = $_.Exception.Message }
    }
}

function Invoke-Baseline([string]$SelectedRoot, [string]$DatasetLabel) {
    $records = & $binary $SelectedRoot $Iterations $parentPath
    if ($LASTEXITCODE -ne 0) {
        throw "Index baseline failed for $DatasetLabel (exit $LASTEXITCODE)."
    }
    return @{
        label = $DatasetLabel
        storage = Get-StorageDescription $SelectedRoot
        records = @($records | ForEach-Object { $_ | ConvertFrom-Json })
    }
}

Push-Location -LiteralPath $repoRoot
try {
    & cargo build -p fsearch-core --example index_baseline --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Benchmark example build failed.' }
    $binary = Join-Path $repoRoot 'target\release\examples\index_baseline.exe'
    $os = Get-CimInstance Win32_OperatingSystem
    $cpu = Get-CimInstance Win32_Processor
    $report = [ordered]@{
        schema = 1
        measuredAtUtc = [DateTime]::UtcNow.ToString('o')
        operatingSystem = [string]$os.Caption
        osVersion = [string]$os.Version
        architecture = [string]$os.OSArchitecture
        cpu = [string]$cpu.Name
        logicalProcessors = [int]$cpu.NumberOfLogicalProcessors
        rust = [string](& rustc --version)
        profile = 'release; workspace lto=true, debug=false, panic=abort'
        binaryBytes = (Get-Item -LiteralPath $binary).Length
        cachePolicy = 'No OS cache flush. Generated fixture is warm from creation; first means first index build in the measured process. Rebuild and queries are warm.'
        queryPolicy = '10 warmups then nearest-rank p50/p95; four modes; 20,000 displayed indices with full match count; all entries filter. Exact queries sample up to nine native entries plus a miss. Mixed substrings: report, 資料, café, .txt, .rs, 000, dir-, empty, [literal], and a miss. Separate empty_all and miss workloads are also measured for each substring mode.'
        fixture = @{ files = $FixtureFiles; directories = $Directories; contents = 'zero bytes' }
        datasets = @()
    }
    $fixtureRoot = [IO.Path]::GetFullPath((Join-Path $parentPath ('fsearch-index-baseline-' + [Guid]::NewGuid().ToString('N'))))
    $safePrefix = $parentPath.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $fixtureRoot.StartsWith($safePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Generated fixture escaped its selected parent.'
    }
    if (Test-Path -LiteralPath $fixtureRoot) { throw 'Fixture already exists.' }
    [IO.Directory]::CreateDirectory($fixtureRoot) | Out-Null
    try {
        $fixtureStarted = [Diagnostics.Stopwatch]::StartNew()
        for ($directoryIndex = 0; $directoryIndex -lt $Directories; $directoryIndex++) {
            [IO.Directory]::CreateDirectory((Join-Path $fixtureRoot ('dir-{0:D4}' -f $directoryIndex))) | Out-Null
        }
        $names = @('report', '資料', 'café', 'notes', '[literal]')
        for ($fileIndex = 0; $fileIndex -lt $FixtureFiles; $fileIndex++) {
            $directory = Join-Path $fixtureRoot ('dir-{0:D4}' -f ($fileIndex % $Directories))
            $name = '{0}-{1:D8}.txt' -f $names[$fileIndex % $names.Count], $fileIndex
            [IO.File]::WriteAllBytes((Join-Path $directory $name), [byte[]]@())
        }
        $fixtureStarted.Stop()
        $report.fixture.generationMs = $fixtureStarted.Elapsed.TotalMilliseconds
        $report.datasets += Invoke-Baseline $fixtureRoot 'generated fixture'
        if ($RealRoot) { $report.datasets += Invoke-Baseline $RealRoot 'explicit existing tree' }
    } finally {
        if (-not $KeepFixture) {
            # Verify the final absolute path before recursive deletion. This is
            # always the unique directory created above, never RealRoot.
            $resolvedFixture = (Resolve-Path -LiteralPath $fixtureRoot).ProviderPath
            if (-not $resolvedFixture.StartsWith($safePrefix, [StringComparison]::OrdinalIgnoreCase) -or
                [IO.Path]::GetFileName($resolvedFixture) -notlike 'fsearch-index-baseline-*') {
                throw 'Refusing fixture cleanup outside the selected parent.'
            }
            Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
        }
    }
    $json = $report | ConvertTo-Json -Depth 12
    if ($OutputPath) {
        $resolvedOutput = [IO.Path]::GetFullPath($OutputPath)
        [IO.File]::WriteAllText($resolvedOutput, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
        Write-Host "Baseline report: $resolvedOutput"
    } else {
        Write-Output $json
    }
} finally {
    Pop-Location
}
