#Requires -Modules @{ ModuleName = 'Pester'; ModuleVersion = '5.0' }
<#
    Windows twin of scripts/tests/test-install.sh: exercises install.ps1 end to end
    against a local fixture HTTP server, mirroring the same scenarios (checksum
    verification, destination-identity refusal, version pinning, custom install
    dir) without any real network access. See install.ps1 for the installer's
    security contract this suite is guarding.

    Fixture archives contain a real (tiny) Windows executable — install.ps1 shells
    out to `--version` on the extracted binary, which a plain text/script file
    cannot satisfy on Windows regardless of file extension. The fixture compiles
    one via csc.exe (the .NET Framework compiler that ships with every
    Windows runner, independent of which PowerShell edition is running this
    suite) rather than `Add-Type -OutputType ConsoleApplication`, which only
    works under Windows PowerShell 5.1's CodeDom-based provider and fails outright
    under PowerShell 7 (pwsh)'s Roslyn-based Add-Type.
#>

Set-StrictMode -Version Latest

BeforeAll {
    # Two-argument Join-Path chained rather than the multi-segment form: the
    # multi-segment (-AdditionalChildPath) overload only exists from
    # PowerShell 6 on, and this suite must also run under Windows PowerShell
    # 5.1 (see the New-FakeExe comment below for why).
    $repoRoot = Resolve-Path (Join-Path (Join-Path $PSScriptRoot '..') '..')
    $installScript = Join-Path $repoRoot 'install.ps1'

    function Get-CscPath {
        # Locate the .NET Framework C# compiler. Its on-disk version segment
        # (v4.0.30319) has been stable across every .NET Framework 4.x release
        # since 4.0, so this does not need updating for 4.5/4.8/etc, but we still
        # search rather than hardcode the full path in case a future runner image
        # relocates it.
        $candidates = Get-ChildItem -Path (Join-Path $env:windir 'Microsoft.NET\Framework64') `
            -Filter csc.exe -Recurse -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending
        if (-not $candidates) {
            $candidates = Get-ChildItem -Path (Join-Path $env:windir 'Microsoft.NET\Framework') `
                -Filter csc.exe -Recurse -ErrorAction SilentlyContinue |
                Sort-Object FullName -Descending
        }
        if (-not $candidates) {
            throw 'csc.exe was not found under %windir%\Microsoft.NET; cannot build fixture executables'
        }
        return $candidates[0].FullName
    }
    $csc = Get-CscPath

    function New-FakeExe {
        # Compiles a tiny real console executable at $Path that prints
        # $VersionOutput for `--version` and exits non-zero for anything else —
        # enough to stand in for processkit-cli.exe (real one) or an unrelated
        # program (foreign one) in the scenarios below.
        param(
            [Parameter(Mandatory)] [string] $Path,
            [Parameter(Mandatory)] [string] $VersionOutput
        )
        $sourcePath = [IO.Path]::ChangeExtension($Path, '.cs')
        $escaped = $VersionOutput.Replace('"', '\"')
        @"
using System;
class Program {
    static int Main(string[] args) {
        if (args.Length == 1 && args[0] == "--version") {
            Console.WriteLine("$escaped");
            return 0;
        }
        return 2;
    }
}
"@ | Set-Content -LiteralPath $sourcePath -Encoding UTF8
        $output = & $csc /nologo "/out:$Path" $sourcePath 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "csc.exe failed to compile fixture executable '$Path': $output"
        }
        Remove-Item -LiteralPath $sourcePath -Force
    }

    function Publish-FixtureRelease {
        # Lays out $Root/v$Version/processkit-cli-v$Version-$Target.zip (plus a
        # matching or deliberately corrupted .sha256 sidecar), the same layout
        # install.ps1 expects to find under its release base URL.
        param(
            [Parameter(Mandatory)] [string] $Root,
            [Parameter(Mandatory)] [string] $Version,
            [Parameter(Mandatory)] [string] $Target,
            [Parameter(Mandatory)] [string] $VersionOutput,
            [switch] $CorruptChecksum
        )
        $releaseDir = Join-Path $Root "v$Version"
        New-Item -ItemType Directory -Path $releaseDir -Force | Out-Null
        $archiveName = "processkit-cli-v$Version-$Target.zip"
        $archivePath = Join-Path $releaseDir $archiveName

        $staging = Join-Path ([IO.Path]::GetTempPath()) ("processkit-cli-fixture-" + [guid]::NewGuid())
        New-Item -ItemType Directory -Path $staging | Out-Null
        try {
            $exePath = Join-Path $staging 'processkit-cli.exe'
            New-FakeExe -Path $exePath -VersionOutput $VersionOutput
            if (Test-Path -LiteralPath $archivePath) {
                Remove-Item -LiteralPath $archivePath -Force
            }
            Compress-Archive -Path $exePath -DestinationPath $archivePath
        } finally {
            Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
        }

        if ($CorruptChecksum) {
            # A well-formed but wrong 64-hex-digit checksum: exercises the
            # "verification fails" path distinctly from the "sidecar is
            # malformed" path already covered indirectly by the sidecar format
            # check in install.ps1.
            Set-Content -LiteralPath "$archivePath.sha256" -Value ('0' * 64 + "  $archiveName") -NoNewline
        } else {
            $hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
            Set-Content -LiteralPath "$archivePath.sha256" -Value "$hash  $archiveName" -NoNewline
        }
    }

    function Get-FreeTcpPort {
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
        $listener.Start()
        try {
            return $listener.LocalEndpoint.Port
        } finally {
            $listener.Stop()
        }
    }

    function Start-FixtureServer {
        # A minimal static file server rooted at $Root, the PowerShell/HttpListener
        # analogue of `python -m http.server --directory $root` that
        # test-install.sh uses for install.sh. Runs in a background runspace so
        # install.ps1's synchronous Invoke-WebRequest calls can be served while
        # the test thread is blocked waiting on the installer.
        param([Parameter(Mandatory)] [string] $Root)
        $port = Get-FreeTcpPort
        $listener = [Net.HttpListener]::new()
        $listener.Prefixes.Add("http://127.0.0.1:$port/")
        $listener.Start()

        $runspace = [runspacefactory]::CreateRunspace()
        $runspace.Open()
        $ps = [powershell]::Create()
        $ps.Runspace = $runspace
        [void]$ps.AddScript({
            param($listener, $root)
            while ($listener.IsListening) {
                try {
                    $context = $listener.GetContext()
                } catch {
                    break
                }
                $response = $context.Response
                try {
                    $relative = [Uri]::UnescapeDataString($context.Request.Url.LocalPath.TrimStart('/'))
                    $filePath = Join-Path $root $relative
                    if (Test-Path -LiteralPath $filePath -PathType Leaf) {
                        $bytes = [IO.File]::ReadAllBytes($filePath)
                        $response.ContentLength64 = $bytes.Length
                        $response.OutputStream.Write($bytes, 0, $bytes.Length)
                    } else {
                        $response.StatusCode = 404
                    }
                } catch {
                    try { $response.StatusCode = 500 } catch {}
                } finally {
                    $response.OutputStream.Close()
                }
            }
        }).AddArgument($listener).AddArgument($Root)
        $handle = $ps.BeginInvoke()

        [pscustomobject]@{
            Listener   = $listener
            Runspace   = $runspace
            PowerShell = $ps
            Handle     = $handle
            BaseUrl    = "http://127.0.0.1:$port"
        }
    }

    function Stop-FixtureServer {
        param($Server)
        if (-not $Server) {
            return
        }
        try { $Server.Listener.Stop() } catch {}
        try { $Server.Listener.Close() } catch {}
        try { $Server.PowerShell.Stop() } catch {}
        try { $Server.PowerShell.EndInvoke($Server.Handle) } catch {}
        $Server.PowerShell.Dispose()
        $Server.Runspace.Close()
        $Server.Runspace.Dispose()
    }

    function Invoke-Installer {
        # Runs install.ps1 with PROCESSKIT_CLI_RELEASE_BASE pointed at the
        # fixture server — the same override point test-install.sh uses for
        # install.sh — so no scenario here ever reaches the real GitHub API or
        # release CDN. -Version is always supplied so the (unfixturable)
        # "no -Version" path to api.github.com/releases/latest is never hit.
        param(
            [Parameter(Mandatory)] [string] $ReleaseBase,
            [Parameter(Mandatory)] [string] $Version,
            [Parameter(Mandatory)] [string] $InstallDir,
            [string] $Target = 'x86_64-pc-windows-msvc'
        )
        $previous = $env:PROCESSKIT_CLI_RELEASE_BASE
        $env:PROCESSKIT_CLI_RELEASE_BASE = $ReleaseBase
        try {
            & $installScript -Version $Version -InstallDir $InstallDir -Target $Target
        } finally {
            if ($null -eq $previous) {
                Remove-Item Env:\PROCESSKIT_CLI_RELEASE_BASE -ErrorAction SilentlyContinue
            } else {
                $env:PROCESSKIT_CLI_RELEASE_BASE = $previous
            }
        }
    }
}

Describe 'install.ps1' {
    BeforeEach {
        $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ("processkit-cli-fixture-root-" + [guid]::NewGuid())
        New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
        $installRoot = Join-Path ([IO.Path]::GetTempPath()) ("processkit-cli-install-root-" + [guid]::NewGuid())
        $server = Start-FixtureServer -Root $fixtureRoot
        $target = 'x86_64-pc-windows-msvc'
    }

    AfterEach {
        # Runs even when the It above failed/threw, so no fixture server or
        # temp directory survives a failed run.
        Stop-FixtureServer -Server $server
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $installRoot -Recurse -Force -ErrorAction SilentlyContinue
    }

    Context 'a valid archive with a matching checksum' {
        It 'installs and the installed binary reports the expected version' {
            $version = '1.2.3'
            Publish-FixtureRelease -Root $fixtureRoot -Version $version -Target $target `
                -VersionOutput "processkit-cli $version"

            Invoke-Installer -ReleaseBase $server.BaseUrl -Version $version -InstallDir $installRoot -Target $target

            $installedExe = Join-Path $installRoot 'processkit-cli.exe'
            $installedExe | Should -Exist
            (& $installedExe --version) | Should -BeExactly "processkit-cli $version"
        }
    }

    Context 'checksum verification' {
        It 'refuses an archive whose checksum does not match its .sha256 sidecar, before extraction' {
            $version = '3.4.5'
            Publish-FixtureRelease -Root $fixtureRoot -Version $version -Target $target `
                -VersionOutput "processkit-cli $version" -CorruptChecksum

            {
                Invoke-Installer -ReleaseBase $server.BaseUrl -Version $version -InstallDir $installRoot -Target $target
            } | Should -Throw -ExpectedMessage '*SHA-256 mismatch*'

            (Join-Path $installRoot 'processkit-cli.exe') | Should -Not -Exist
        }

        It 'leaves an existing valid install untouched when a later install has a checksum mismatch' {
            $goodVersion = '1.0.0'
            Publish-FixtureRelease -Root $fixtureRoot -Version $goodVersion -Target $target `
                -VersionOutput "processkit-cli $goodVersion"
            Invoke-Installer -ReleaseBase $server.BaseUrl -Version $goodVersion -InstallDir $installRoot -Target $target
            $destination = Join-Path $installRoot 'processkit-cli.exe'
            $before = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash

            $badVersion = '2.0.0'
            Publish-FixtureRelease -Root $fixtureRoot -Version $badVersion -Target $target `
                -VersionOutput "processkit-cli $badVersion" -CorruptChecksum
            {
                Invoke-Installer -ReleaseBase $server.BaseUrl -Version $badVersion -InstallDir $installRoot -Target $target
            } | Should -Throw

            (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash | Should -BeExactly $before
            (& $destination --version) | Should -BeExactly "processkit-cli $goodVersion"
        }
    }

    Context 'destination identity' {
        It 'refuses to overwrite a destination that does not identify as processkit-cli' {
            New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
            $destination = Join-Path $installRoot 'processkit-cli.exe'
            New-FakeExe -Path $destination -VersionOutput 'foreign-program'
            $before = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash

            $version = '4.5.6'
            Publish-FixtureRelease -Root $fixtureRoot -Version $version -Target $target `
                -VersionOutput "processkit-cli $version"

            {
                Invoke-Installer -ReleaseBase $server.BaseUrl -Version $version -InstallDir $installRoot -Target $target
            } | Should -Throw -ExpectedMessage '*refusing to overwrite*'

            (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash | Should -BeExactly $before
        }
    }

    Context 'version pinning and install location' {
        It 'downloads exactly the pinned version and honors a custom -InstallDir' {
            $pinned = '2.5.0'
            $other = '9.9.9'
            Publish-FixtureRelease -Root $fixtureRoot -Version $pinned -Target $target `
                -VersionOutput "processkit-cli $pinned"
            Publish-FixtureRelease -Root $fixtureRoot -Version $other -Target $target `
                -VersionOutput "processkit-cli $other"

            $customDir = Join-Path $installRoot 'nested\custom'
            Invoke-Installer -ReleaseBase $server.BaseUrl -Version $pinned -InstallDir $customDir -Target $target

            $installedExe = Join-Path $customDir 'processkit-cli.exe'
            $installedExe | Should -Exist
            (& $installedExe --version) | Should -BeExactly "processkit-cli $pinned"
            # The default (non-custom) location under $installRoot must stay empty.
            (Join-Path $installRoot 'processkit-cli.exe') | Should -Not -Exist
        }
    }
}
