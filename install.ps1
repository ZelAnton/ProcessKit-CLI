# Install a verified processkit-cli GitHub Release binary.
[CmdletBinding()]
param(
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string] $Version,
    [string] $InstallDir = (Join-Path ([Environment]::GetFolderPath('UserProfile')) '.local\bin'),
    [string] $Target
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repository = 'ZelAnton/ProcessKit-CLI'
$staged = $null

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    throw 'Install directory cannot be empty'
}

if (-not $Version) {
    $release = Invoke-RestMethod `
        -Uri "https://api.github.com/repos/$repository/releases/latest" `
        -Headers @{ 'User-Agent' = 'processkit-cli-installer' }
    if ($release.tag_name -notmatch '^v(\d+\.\d+\.\d+)$') {
        throw "Latest release returned an invalid tag: $($release.tag_name)"
    }
    $Version = $Matches[1]
}

if (-not $Target) {
    $Target = switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'x86_64-pc-windows-msvc' }
        'ARM64' { 'aarch64-pc-windows-msvc' }
        default { throw "No prebuilt Windows archive for architecture '$env:PROCESSOR_ARCHITECTURE'" }
    }
}
if ($Target -notmatch '^[A-Za-z0-9_-]+$') {
    throw "Invalid target triple: $Target"
}

$archive = "processkit-cli-v$Version-$Target.zip"
$releaseBase = if ($env:PROCESSKIT_CLI_RELEASE_BASE) {
    $env:PROCESSKIT_CLI_RELEASE_BASE.TrimEnd('/')
} else {
    "https://github.com/$repository/releases/download"
}
$base = "$releaseBase/v$Version"
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("processkit-cli-install-" + [guid]::NewGuid())

try {
    New-Item -ItemType Directory -Path $scratch | Out-Null
    $archivePath = Join-Path $scratch $archive
    $checksumPath = "$archivePath.sha256"
    Write-Host "processkit-cli installer: downloading $archive"
    Invoke-WebRequest -Uri "$base/$archive" -OutFile $archivePath
    Invoke-WebRequest -Uri "$base/$archive.sha256" -OutFile $checksumPath

    $checksumLine = (Get-Content -LiteralPath $checksumPath -Raw).Trim()
    $expected = ($checksumLine -split '\s+')[0].ToLowerInvariant()
    if ($expected -notmatch '^[0-9a-f]{64}$') {
        throw 'Published checksum sidecar is malformed'
    }
    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA-256 mismatch for $archive"
    }

    $expanded = Join-Path $scratch 'expanded'
    Expand-Archive -LiteralPath $archivePath -DestinationPath $expanded
    $source = Join-Path $expanded 'processkit-cli.exe'
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw 'Verified archive does not contain processkit-cli.exe at its root'
    }
    $downloadedVersion = try { (& $source --version 2>$null) } catch { '' }
    if ($downloadedVersion -ne "processkit-cli $Version") {
        throw "Verified archive contains '$downloadedVersion', expected processkit-cli $Version"
    }

    $destination = Join-Path $InstallDir 'processkit-cli.exe'
    if (Test-Path -LiteralPath $destination) {
        $installedVersion = try { (& $destination --version 2>$null) } catch { '' }
        if ($installedVersion -notmatch '^processkit-cli \d+\.\d+\.\d+$') {
            throw "$destination exists but is not processkit-cli; refusing to overwrite"
        }
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $staged = "$destination.tmp.$PID"
    Copy-Item -LiteralPath $source -Destination $staged
    Move-Item -LiteralPath $staged -Destination $destination -Force
    $staged = $null
    & $destination --version
    Write-Host "processkit-cli installer: installed to $destination"

    $pathEntries = $env:PATH -split [IO.Path]::PathSeparator
    if ($InstallDir -notin $pathEntries) {
        Write-Host "processkit-cli installer: add $InstallDir to PATH"
    }
} finally {
    if ($staged -and (Test-Path -LiteralPath $staged)) {
        Remove-Item -LiteralPath $staged -Force
    }
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
