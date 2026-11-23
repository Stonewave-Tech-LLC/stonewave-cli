# Tests of install/cli/install.ps1.in on Windows PowerShell 5.1 (or newer). Nothing goes to the internet; nothing outside a temporary folder and
# one throw-away registry key under HKCU\Software is changed. The real user PATH is never touched.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-user-install.ps1
#
# The signature check is tested with a program that Windows itself signed (a stand-in for our signed build): its signer name and thumbprint
# are pinned for the test, and a changed copy, an unsigned file and a wrong pin must all be refused.
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$T = Join-Path $env:TEMP ('swinst-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force -Path $T | Out-Null
$script:pass = 0
$script:failed = 0

function Ok { $script:pass++ }
function Bad([string]$m) { $script:failed++; Write-Host "FAIL: $m" -ForegroundColor Red }
function Expect([bool]$cond, [string]$m) { if ($cond) { Ok } else { Bad $m } }
function Throws([scriptblock]$code, [string]$needle, [string]$m) {
    try { & $code; Bad "$m (did not fail)" }
    catch { if ($_.Exception.Message -like "*$needle*") { Ok } else { Bad "$m (message: $($_.Exception.Message))" } }
}

try {
    # ---------------------------------------------------------------- a signed donor and the rendered installer
    $donor = $null
    foreach ($f in (Get-ChildItem "$env:windir\System32\*.exe" | Sort-Object Length | Select-Object -First 400)) {
        $s = Get-AuthenticodeSignature -FilePath $f.FullName
        if ($s.Status -eq 'Valid' -and $s.SignerCertificate) { $donor = $f.FullName; $donorSig = $s; break }
    }
    if (-not $donor) { throw 'no signed program found to stand in for our build' }
    $cert = $donorSig.SignerCertificate
    $cn = ([regex]::Match($cert.Subject, '(^|,\s*)CN=([^,]+)')).Groups[2].Value
    $thumb = $cert.Thumbprint.ToUpperInvariant()
    Write-Host "stand-in signer: CN=$cn $thumb ($(Split-Path -Leaf $donor))"

    $rendered = Join-Path $T 'install.ps1'
    (Get-Content -Raw -LiteralPath (Join-Path $here 'install\cli\install.ps1.in')) -replace '@WIN_THUMBPRINTS@', $thumb | Set-Content -Encoding UTF8 -LiteralPath $rendered
    $e = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($rendered, [ref]$null, [ref]$e)
    Expect (-not $e) "install.ps1 does not parse: $e"
    . $rendered                                 # only defines the functions
    $script:SignerName = $cn
    $script:SignerThumbprints = $thumb
    $text = Get-Content -Raw -LiteralPath $rendered
    Expect ($text -match "Base = 'https://") 'the base is https'
    Expect ($text -notmatch 'http://') 'no plain http anywhere'
    Expect ($text -notmatch '(?i)RunAs|Start-Process.*-Verb') 'no elevation'

    # ---------------------------------------------------------------- the manifest
    function Sha([string]$p) { (Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash.ToLowerInvariant() }
    function New-Release([string]$Dir, [string]$Ver, [string]$Commit, [string]$Source, [switch]$Latest) {
        New-Item -ItemType Directory -Force -Path (Join-Path $Dir $Ver) | Out-Null
        $name = "stonewave-$Ver-x86_64-windows.exe"
        Copy-Item -LiteralPath $Source -Destination (Join-Path $Dir "$Ver\$name")
        $p = Join-Path $Dir "$Ver\$name"
        $m = "stonewave-manifest 1`nversion $Ver`ncommit $Commit`nfile x86_64-windows $(Sha $p) $((Get-Item $p).Length) $name`n"
        [IO.File]::WriteAllText((Join-Path $Dir "$Ver\manifest.txt"), $m)
        if ($Latest) {
            New-Item -ItemType Directory -Force -Path (Join-Path $Dir 'latest') | Out-Null
            [IO.File]::WriteAllText((Join-Path $Dir 'latest\manifest.txt'), $m)
        }
    }
    $good = Join-Path $T 'good.txt'
    [IO.File]::WriteAllText($good, "stonewave-manifest 1`nversion 0.2.0`ncommit cafe0123`nfile x86_64-windows $('a' * 64) 123 stonewave-0.2.0-x86_64-windows.exe`n")
    $m = Read-Manifest $good 'x86_64-windows'
    Expect ($m.Version -eq '0.2.0' -and $m.Commit -eq 'cafe0123' -and $m.Size -eq '123') 'a good manifest is read'
    function Bend([string]$from, [string]$to) {
        $p = Join-Path $T 'bent.txt'
        [IO.File]::WriteAllText($p, ([IO.File]::ReadAllText($good)).Replace($from, $to))
        return $p
    }
    Throws { Read-Manifest (Bend 'version 0.2.0' 'version 0.2.0; calc') 'x86_64-windows' } 'no usable version' 'a version with a command in it'
    Throws { Read-Manifest (Bend 'stonewave-0.2.0-x86_64-windows.exe' '..\..\evil.exe') 'x86_64-windows' } 'does not fit' 'a path in the file name'
    Throws { Read-Manifest (Bend ('a' * 64) 'ABC') 'x86_64-windows' } 'no usable checksum' 'a short checksum'
    Throws { Read-Manifest (Bend 'commit cafe0123' 'commit cafe0123-dirty') 'x86_64-windows' } 'no usable commit' 'a dirty commit'
    Throws { Read-Manifest (Bend 'x86_64-windows' 'aarch64-windows') 'x86_64-windows' } 'has no build for' 'a release without a build for this system'
    Throws { Read-Manifest (Bend 'stonewave-manifest 1' 'stonewave-manifest 2') 'x86_64-windows' } 'not in a format' 'another format'

    # ---------------------------------------------------------------- the signature check
    Test-StonewaveSignature $donor $cn $thumb 6>$null
    Ok
    Throws { Test-StonewaveSignature $donor 'Stonewave Tech LLC' $thumb } 'not by Stonewave Tech LLC' 'another signer name'
    Throws { Test-StonewaveSignature $donor ($cn + ' Ltd') $thumb } 'not by' 'a name that only starts like the pinned one'
    Throws { Test-StonewaveSignature $donor $cn ('0' * 40) } 'not one this installer pins' 'another thumbprint'
    Throws { Test-StonewaveSignature $donor $cn '@WIN_THUMBPRINTS@' } 'not prepared for Windows' 'an installer that was not prepared'
    Throws { Test-StonewaveSignature $donor $cn '' } 'no certificate is pinned' 'nothing pinned'
    $plain = Join-Path $T 'plain.exe'
    [IO.File]::WriteAllText($plain, 'MZ not a program')
    Throws { Test-StonewaveSignature $plain $cn $thumb } 'not valid' 'an unsigned file'
    $changed = Join-Path $T 'changed.exe'
    Copy-Item $donor $changed
    $fs = [IO.File]::Open($changed, 'Append'); $fs.WriteByte(0); $fs.Close()
    Throws { Test-StonewaveSignature $changed $cn $thumb } 'not valid' 'a signed file with a changed byte'

    # ---------------------------------------------------------------- the whole install, with the stand-in as the program
    $script:says = 'stonewave 0.2.0 (cafe0123)'
    function Get-BinaryVersion([string]$Path) { if ((Get-Item -LiteralPath $Path).Length -lt 100) { return 'stonewave 0.1.0 (00000000)' } else { return $script:says } }
    function Reset([string]$Name) {
        $d = Join-Path $T $Name
        New-Item -ItemType Directory -Force -Path $d | Out-Null
        $script:FromDir = $d
        $script:BinDir = Join-Path $T "bin-$Name"
        Remove-Item -Recurse -Force $script:BinDir -ErrorAction SilentlyContinue
        $script:Version = ''
        $script:NoPath = [switch]$true
        $script:DryRun = [switch]$false
        return $d
    }
    function Run([scriptblock]$Extra = {}) { $o = Install-Stonewave 6>&1 | Out-String; return $o }

    $d = Reset 'site1'
    New-Release $d '0.2.0' 'cafe0123' $donor -Latest
    $o = Run
    Expect (Test-Path "$script:BinDir\stonewave.exe") 'a fresh install puts the program in place'
    Expect ($o -match 'checksum ok' -and $o -match 'signature ok' -and $o -match 'installed') 'the install says what it checked'
    Expect (@(Get-ChildItem -Force $script:BinDir).Count -eq 1) 'no temporary folder is left'
    $stamp = (Get-Item "$script:BinDir\stonewave.exe").LastWriteTimeUtc.Ticks
    $o = Run
    Expect ($o -match 'already up to date') 'a second run changes nothing'
    Expect ((Get-Item "$script:BinDir\stonewave.exe").LastWriteTimeUtc.Ticks -eq $stamp) 'and does not touch the file'

    # dry run
    $d = Reset 'site2'
    New-Release $d '0.2.0' 'cafe0123' $donor -Latest
    $script:DryRun = [switch]$true
    $o = Run
    Expect ($o -match 'dry run: would install') 'a dry run says so'
    Expect (-not (Test-Path "$script:BinDir\stonewave.exe")) 'a dry run installs nothing'

    # refusals: nothing is installed, no leftovers
    function Refused([string]$name, [scriptblock]$prepare, [string]$needle) {
        $d = Reset $name
        New-Release $d '0.2.0' 'cafe0123' $donor -Latest
        & $prepare $d
        Throws { Install-Stonewave 6>$null } $needle $name
        Expect (-not (Test-Path "$script:BinDir\stonewave.exe")) "${name}: nothing was installed"
        Expect (@(Get-ChildItem -Force $script:BinDir -ErrorAction SilentlyContinue).Count -eq 0) "${name}: no leftovers"
    }
    Refused 'changed' { param($d) $fs = [IO.File]::Open("$d\0.2.0\stonewave-0.2.0-x86_64-windows.exe", 'Append'); $fs.WriteByte(1); $fs.Close() } 'the size is'
    Refused 'samesize' { param($d) $p = "$d\0.2.0\stonewave-0.2.0-x86_64-windows.exe"; $n = (Get-Item $p).Length; [IO.File]::WriteAllBytes($p, (New-Object byte[] $n)) } 'checksum does not match'
    # a file with the right hash and size that is not signed (as if the host had been swapped and the manifest with it)
    Refused 'unsigned' {
        param($d)
        $p = "$d\0.2.0\stonewave-0.2.0-x86_64-windows.exe"
        [IO.File]::WriteAllText($p, 'MZ not a signed program')
        $mt = "stonewave-manifest 1`nversion 0.2.0`ncommit cafe0123`nfile x86_64-windows $(Sha $p) $((Get-Item $p).Length) stonewave-0.2.0-x86_64-windows.exe`n"
        [IO.File]::WriteAllText("$d\latest\manifest.txt", $mt)
    } 'signature of the download is not valid'
    Refused 'says-other' { param($d) $script:says = 'stonewave 0.2.0 (deadbeef)' } 'not ''stonewave 0.2.0 (cafe0123)'''
    $script:says = 'stonewave 0.2.0 (cafe0123)'
    Refused 'missing' { param($d) Remove-Item "$d\0.2.0\stonewave-0.2.0-x86_64-windows.exe" } 'not found in'
    $script:SignerThumbprints = '0' * 40
    Refused 'wrong-pin' { param($d) } 'not one this installer pins'
    $script:SignerThumbprints = $thumb

    # downgrade and update
    $d = Reset 'site3'
    New-Release $d '0.2.0' 'cafe0123' $donor
    New-Release $d '0.1.9' 'beef4567' $donor -Latest
    $script:says = 'stonewave 0.2.0 (cafe0123)'
    $script:Version = '0.2.0'
    [void](Run)
    $script:Version = ''
    Throws { Install-Stonewave 6>$null } 'newer than the latest release 0.1.9' 'an older latest is refused'
    $script:says = 'stonewave 0.1.9 (beef4567)'
    $script:Version = '0.1.9'
    $o = Run
    Expect ($o -match 'installed') 'an older one can be asked for by number'
    $script:Version = ''
    New-Release $d '0.3.0' 'abcd0123' $donor -Latest
    $script:says = 'stonewave 0.3.0 (abcd0123)'
    $o = Run
    Expect ($o -match 'installed') 'an update works'
    Throws { $script:Version = '0.2'; Install-Stonewave 6>$null } 'must look like' 'a bad -Version'
    $script:Version = ''

    # a running copy: the old file is renamed aside, the new one takes its place
    $d = Reset 'site4'
    New-Release $d '0.2.0' 'cafe0123' $donor -Latest
    New-Item -ItemType Directory -Force -Path $script:BinDir | Out-Null
    $hold = Join-Path $script:BinDir 'stonewave.exe'
    [IO.File]::WriteAllText($hold, 'old')
    $lock = [IO.File]::Open($hold, 'Open', 'Read', [IO.FileShare]'Read,Delete')   # held open, as a running program is (it can be renamed, not overwritten)
    $script:says = 'stonewave 0.2.0 (cafe0123)'
    try { $o = Run } finally { $lock.Close() }
    Expect ((Get-Item $hold).Length -gt 100) 'an open old copy is replaced'
    Expect (@(Get-ChildItem -Force $script:BinDir | Where-Object Name -like '*.old-*').Count -le 1) 'at most the held old copy is left aside'

    # ---------------------------------------------------------------- the user PATH, in a throw-away key (never the real one)
    $sub = 'Software\StonewaveInstallerTest-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($sub)
    try {
        $k.SetValue('Path', '%USERPROFILE%\tools;C:\Existing', [Microsoft.Win32.RegistryValueKind]::ExpandString)
        $k.Close()
        Expect (Add-ToUserPath 'C:\New Folder\bin' $sub) 'a new folder is added'
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($sub)
        $v = $k.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        Expect ($v -eq '%USERPROFILE%\tools;C:\Existing;C:\New Folder\bin') "the old value is kept as it was (%VARIABLES% not expanded): $v"
        Expect ($k.GetValueKind('Path') -eq 'ExpandString') 'and stays an expandable string'
        $k.Close()
        Expect (-not (Add-ToUserPath 'c:\new folder\BIN\' $sub)) 'the same folder (other case, a slash) is not added twice'
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($sub, $true)
        $k.SetValue('Path', '', [Microsoft.Win32.RegistryValueKind]::ExpandString)
        $k.Close()
        Expect (Add-ToUserPath 'C:\Only' $sub) 'an empty PATH gets the folder'
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($sub)
        Expect ($k.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) -eq 'C:\Only') 'with no leading semicolon'
        $k.Close()
    } finally {
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($sub, $false)
    }

    # ---------------------------------------------------------------- the way it is meant to be used: a script block with parameters
    $d = Reset 'site5'
    New-Release $d '0.2.0' 'cafe0123' $donor -Latest
    $sb = [scriptblock]::Create((Get-Content -Raw -LiteralPath $rendered))
    $hostOut = & $sb -FromDir $d -BinDir (Join-Path $T 'bin-sb') -NoPath -DryRun 6>&1 | Out-String
    Expect ($hostOut -match 'Stonewave CLI installer') 'a script block with parameters runs the installer'
    # the stand-in's own signer is not ours, so the real, unmodified pins make it refuse (and say so in red) instead of installing
    $fresh = Join-Path $T 'fresh.ps1'
    (Get-Content -Raw -LiteralPath (Join-Path $here 'install\cli\install.ps1.in')) -replace '@WIN_THUMBPRINTS@', ('F' * 40) | Set-Content -Encoding UTF8 -LiteralPath $fresh
    $sb2 = [scriptblock]::Create((Get-Content -Raw -LiteralPath $fresh))
    $hostOut = & $sb2 -FromDir $d -BinDir (Join-Path $T 'bin-sb2') -NoPath 6>&1 *>&1 | Out-String
    Expect ($hostOut -match 'not by Stonewave Tech LLC|not one this installer pins|not valid') 'with other pins the program is refused'
    Expect (-not (Test-Path (Join-Path $T 'bin-sb2\stonewave.exe'))) 'and not installed'
}
finally {
    Remove-Item -Recurse -Force $T -ErrorAction SilentlyContinue
}
Write-Host "$script:pass passed, $script:failed failed"
if ($script:failed -ne 0) { exit 1 }
