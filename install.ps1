# Install gesta on Windows with one command.
#
#   irm https://raw.githubusercontent.com/zencli/gesta/main/install.ps1 | iex
#
# Written for PowerShell 5.1, which every Windows 10 and 11 machine has, so that the installer
# does not need the newer shell to be installed first.
#   $env:GESTA_CHECK   = '1'       only report what this machine has, change nothing
#   $env:GESTA_FROM    = 'source'  build from source even if a prebuilt binary exists
#   $env:GESTA_VERSION = 'main'    a branch, or a tag
$ErrorActionPreference = 'Stop'

$Repo    = 'zencli/gesta'
$Version = if ($env:GESTA_VERSION) { $env:GESTA_VERSION } else { '' }
$BinDir  = Join-Path $env:LOCALAPPDATA 'gesta\bin'
$Target  = Join-Path $BinDir 'gesta.exe'

function Write-Step($m) { Write-Host $m }

function Get-Platform {
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
    "$arch-pc-windows-msvc"
}

# Rust needs three separate things on Windows, and a missing one produces a linker error that
# names none of them. On the machine this was written for, Rust and the MSVC compiler were both
# present and the Windows SDK was not: link.exe failed with "cannot open input file
# 'kernel32.lib'", which reads like a Rust problem and is not.
function Test-MsvcLinker {
    $roots = @(
        "$env:ProgramFiles\Microsoft Visual Studio",
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio",
        'C:\BuildTools\VC'
    )
    foreach ($r in $roots) {
        if (-not (Test-Path $r)) { continue }
        $hit = Get-ChildItem -Path $r -Filter link.exe -Recurse -ErrorAction SilentlyContinue |
               Where-Object { $_.FullName -like '*Hostx64\x64*' } |
               Select-Object -First 1
        if ($hit) { return $true }
    }
    return $false
}

function Test-WindowsSdk {
    # The linker needs kernel32.lib under <kit>\Lib\<version>\um\<arch>. Checking that a Lib
    # folder exists is not enough: this machine has Windows Kits\10 holding Catalogs and Redist
    # and no Lib at all, so a shallow check would call it installed.
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\Lib",
        "$env:ProgramFiles\Windows Kits\10\Lib"
    )
    foreach ($r in $roots) {
        if (-not (Test-Path $r)) { continue }
        foreach ($v in (Get-ChildItem -Path $r -Directory -ErrorAction SilentlyContinue)) {
            if (Test-Path (Join-Path $v.FullName "um\$arch\kernel32.lib")) { return $true }
        }
    }
    return $false
}

function Show-MissingSdk {
    Write-Host ''
    Write-Host 'Rust cannot link on this machine: the Windows SDK is missing.' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '  Rust:          present'
    Write-Host '  MSVC compiler: present'
    Write-Host '  Windows SDK:   NOT FOUND (no kernel32.lib under Windows Kits\10\Lib)'
    Write-Host ''
    Write-Host 'The linker stops at:'
    Write-Host "  LINK : fatal error LNK1181: cannot open input file 'kernel32.lib'"
    Write-Host ''
    # Both routes need elevation, and `winget install` alone does not work: it sees the Build
    # Tools already present and reports "no available upgrade" without adding the component.
    # That was tried and it changed nothing, so the installer no longer suggests it.
    Write-Host 'Adding the SDK needs administrator rights. Two routes that work:' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '  1. Add the component to the build tools already installed. Open an elevated'
    Write-Host '     PowerShell (right click, "Run as administrator") and run:'
    Write-Host ''
    Write-Host '       & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe" modify --installPath "C:\BuildTools" --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --quiet --norestart'
    Write-Host ''
    Write-Host '     Without elevation the installer exits with code 5007 and changes nothing.'
    Write-Host ''
    Write-Host '  2. Open Visual Studio Installer, choose Modify, tick "Windows 11 SDK" under'
    Write-Host '     Individual components, then run this installer again. The installer asks for'
    Write-Host '     elevation itself when opened from the Start menu.'
    Write-Host ''
    Write-Host '  Either route downloads about 1 GB. After it finishes, run this again.'
    Write-Host ''
    Write-Host 'Nothing was changed on this machine.'
}

function Test-Prerequisites {
    param([switch]$Quiet)
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        if (-not $Quiet) {
            Write-Host ''
            Write-Host 'Rust is not installed or not on PATH.' -ForegroundColor Yellow
            Write-Host 'Install it (no administrator needed), then run this again:'
            Write-Host '  winget install Rustlang.Rustup'
            Write-Host '  rustup default stable-msvc'
            Write-Host ''
        }
        return $false
    }
    $linker = Test-MsvcLinker
    $sdk = Test-WindowsSdk
    if (-not $sdk) {
        if (-not $Quiet) {
            if (-not $linker) {
                Write-Host ''
                Write-Host 'Neither the MSVC compiler nor the Windows SDK is present.' -ForegroundColor Yellow
                Write-Host 'Install the C++ build tools with the SDK. This needs administrator rights:'
                Write-Host ''
                Write-Host '  winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621"'
                Write-Host ''
                Write-Host 'Run that from an elevated PowerShell. On a machine with the build tools already'
                Write-Host 'installed, winget reports "no available upgrade" and adds nothing: use the two'
                Write-Host 'routes shown below instead.'
                Write-Host ''
                Show-MissingSdk
            } else {
                Show-MissingSdk
            }
        }
        return $false
    }
    return $true
}

function Install-Binary($src) {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $tmp = Join-Path $BinDir ".gesta.$PID.exe"
    Copy-Item $src $tmp -Force
    Move-Item $tmp $Target -Force
}

function Get-LatestTag {
    try {
        $ProgressPreference = 'SilentlyContinue'
        $r = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
             -UseBasicParsing -TimeoutSec 30 -Headers @{ 'User-Agent' = 'gesta-installer' }
        return $r.tag_name
    } catch { return $null }
}

function Try-Prebuilt {
    if ($env:GESTA_FROM -eq 'source') { return $false }
    if (-not $Version) { $script:Version = Get-LatestTag }
    if (-not $Version) {
        Write-Step 'No release found; building from source'
        return $false
    }
    $plat = Get-Platform
    # The release writes `gesta-<version>-<platform>.zip`, and the tag carries a leading v that the
    # asset name does not: `v0.11.1` is the tag, `gesta-0.11.1-...` is the file. Both details are what
    # make the URL resolve; without them every machine got a 404 that read as "no build for you".
    $bare = $Version -replace '^v', ''
    $name = "gesta-$bare-$plat.zip"
    $url  = "https://github.com/$Repo/releases/download/$Version/$name"
    $tmp  = Join-Path $env:TEMP "gesta-$PID.zip"
    $dir  = Join-Path $env:TEMP "gesta-$PID-unpack"
    Write-Step "Trying $name"
    try {
        $old = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing -TimeoutSec 300
        $ProgressPreference = $old
        Expand-Archive -Path $tmp -DestinationPath $dir -Force
        $exe = Get-ChildItem -Path $dir -Filter 'gesta.exe' -Recurse -ErrorAction SilentlyContinue |
               Select-Object -First 1
        if (-not $exe) { throw 'the archive did not contain gesta.exe' }
        Install-Binary $exe.FullName
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
        return $true
    } catch {
        Write-Step "No prebuilt archive at $Version for $plat; building from source"
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
        return $false
    }
}

function Build-From-Source {
    New-Item -ItemType Directory -Force -Path (Split-Path $BinDir) | Out-Null
    # `--branch main` fails against a remote with no such tracking ref, so the default branch is
    # requested by leaving the flag out and a version by --tag.
    if (-not $Version) { $script:Version = Get-LatestTag }
    $ref = if (-not $Version -or $Version -eq 'main') { @() } else { @('--tag', $Version) }
    # --locked first, so a build gets the dependency versions the project was tested with. The
    # output goes to a log as well as the screen, because the reason a build failed scrolls past
    # faster than anyone can read it.
    $log = Join-Path $env:TEMP "gesta-build-$PID.log"
    & cargo install --git "https://github.com/$Repo.git" @ref --root (Split-Path $BinDir) --locked 2>&1 |
        Tee-Object -FilePath $log
    if ($LASTEXITCODE -ne 0) {
        & cargo install --git "https://github.com/$Repo.git" @ref --root (Split-Path $BinDir) 2>&1 |
            Tee-Object -FilePath $log
    }
    if ($LASTEXITCODE -ne 0) {
        # A raw linker error names a .lib file and nothing a person can act on, so translate the
        # one that actually happens here rather than printing it again.
        $text = if (Test-Path $log) { Get-Content $log -Raw } else { '' }
        if ($text -match 'LNK1181' -and $text -match 'kernel32\.lib') {
            Show-MissingSdk
            throw 'build failed: the Windows SDK is missing (see above)'
        }
        throw "cargo install failed. The build log is at $log"
    }
    Remove-Item $log -Force -ErrorAction SilentlyContinue
}

if ($env:GESTA_CHECK -eq '1') {
    Write-Step 'Checking what this machine has. Nothing will be changed.'
    Write-Step ''
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    $rust = if ($cargo) { "present at $($cargo.Source)" } else { 'NOT FOUND' }
    Write-Step "  Rust (cargo):  $rust"
    Write-Step "  MSVC linker:   $(if (Test-MsvcLinker) { 'present' } else { 'NOT FOUND' })"
    Write-Step "  Windows SDK:   $(if (Test-WindowsSdk) { 'present' } else { 'NOT FOUND' })"
    Write-Step ''
    if (Test-Prerequisites -Quiet) {
        Write-Step 'This machine can build gesta. Run the installer without GESTA_CHECK.'
        exit 0
    }
    exit 1
}

$ok = Try-Prebuilt
if (-not $ok) {
    if (-not (Test-Prerequisites)) { exit 1 }
    Build-From-Source
}

if (-not (Test-Path $Target)) { throw "the binary did not land at $Target" }

# The user PATH needs no administrator rights, and an install that asks for elevation is an
# install people abandon.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
if ($userPath -notlike "*$BinDir*") {
    $joined = if ($userPath) { "$userPath;$BinDir" } else { $BinDir }
    [Environment]::SetEnvironmentVariable('Path', $joined, 'User')
    Write-Step ''
    Write-Step "$BinDir was added to your user PATH. Open a new terminal for it to take effect."
}

Write-Step ''
Write-Step "gesta installed: $Target"
Write-Step ''
Write-Step 'Then in a project:'
Write-Step '  gesta init'
Write-Step '  gesta changeset create "first change" --author me'
Write-Step '  gesta status'
