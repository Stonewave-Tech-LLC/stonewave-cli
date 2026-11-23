# Tests of install/cli/install.ps1.in on Windows PowerShell 5.1 (or newer). Nothing goes to the internet; nothing outside a temporary folder and
# one throw-away registry key under HKCU\Software is changed. The real user PATH is never touched.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-user-install.ps1
#
# The signature check is tested with a program that Windows itself signed (a stand-in for our signed build): its subject and issuing authority
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
    $subject = $cert.Subject
    $donorChain = New-Object System.Security.Cryptography.X509Certificates.X509Chain
    [void]$donorChain.Build($cert)
    $caPin = $donorChain.ChainElements[1].Certificate.Thumbprint.ToUpperInvariant()
    $rootPin = $donorChain.ChainElements[$donorChain.ChainElements.Count - 1].Certificate.Thumbprint.ToUpperInvariant()
    Write-Host "stand-in signer: $subject, leaf $thumb, authority $caPin ($(Split-Path -Leaf $donor))"

    $rendered = Join-Path $T 'install.ps1'
    (Get-Content -Raw -LiteralPath (Join-Path $here 'install\cli\install.ps1.in')) -replace '@WIN_CA_THUMBPRINTS@', $caPin | Set-Content -Encoding UTF8 -LiteralPath $rendered
    $e = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($rendered, [ref]$null, [ref]$e)
    Expect (-not $e) "install.ps1 does not parse: $e"
    # the installer is one script block (& { ... } @args): for the tests the wrapper lines are taken off, so that dot-sourcing defines the functions here
    $all = @(Get-Content -LiteralPath $rendered)
    $first = [array]::FindIndex($all, [Predicate[string]]{ param($l) $l -like '*stonewave-installer-begin' })
    $last = [array]::FindIndex($all, [Predicate[string]]{ param($l) $l -like '*stonewave-installer-end' })
    Expect (($first -ge 0) -and ($last -gt $first)) 'the wrapper markers are there'
    $innerFile = Join-Path $T 'inner.ps1'
    ($all[($first + 1)..($last - 1)]) | Set-Content -Encoding UTF8 -LiteralPath $innerFile
    . $innerFile                                # only defines the functions
    $SignerSubject = $subject
    $SignerCaThumbprints = $caPin
    $text = Get-Content -Raw -LiteralPath $rendered
    Expect ($text -match "Base = 'https://") 'the base is https'
    Expect ($text -notmatch 'http://') 'no plain http anywhere'
    Expect ($text -notmatch '(?i)RunAs|Start-Process.*-Verb') 'no elevation'
    Expect ($text -match '\$oldProtocol -bor \[Net\.SecurityProtocolType\]::Tls12' -and $text -notmatch '12288') 'TLS 1.2 is added, nothing is replaced, TLS 1.3 is not forced'
    $ast = [System.Management.Automation.Language.Parser]::ParseInput($text, [ref]$null, [ref]$null)
    Expect ($ast.EndBlock.Statements.Count -eq 1) 'the whole installer is ONE statement (& { ... } @args): nothing else runs in the scope of the caller'
    Expect ($ast.ParamBlock -eq $null) 'no param block at the top: under irm | iex it would overwrite the variables of the caller'
    Expect ($text -notmatch '\$script:') 'no script-scope variables: under irm | iex that would be the global scope'

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

    # ---------------------------------------------------------------- the signature check (a program Windows signed: real Authenticode, real chain)
    $plain = Join-Path $T 'plain.exe'
    [IO.File]::WriteAllText($plain, 'MZ not a program')
    $changed = Join-Path $T 'changed.exe'
    Copy-Item $donor $changed
    $fs = [IO.File]::Open($changed, 'Append'); $fs.WriteByte(0); $fs.Close()
    Test-StonewaveSignature $donor $subject $caPin 6>$null
    Ok
    Test-StonewaveSignature $donor $subject "$('0' * 40),$caPin" 6>$null     # several authorities: one of them is enough
    Ok
    Expect ($donorChain.ChainElements.Count -ge 3) 'the stand-in has a chain of at least three: certificate, authority, root'
    Throws { Test-StonewaveSignature $donor $subject $rootPin } 'not issued under the authority' 'the root of the chain is not an authority that counts (a root pin would be every certificate under that root)'
    Throws { Test-StonewaveSignature $donor 'CN=Stonewave Tech LLC, O=Stonewave Tech LLC, L=Sheridan, S=Wyoming, C=US' $caPin } 'not by' 'another subject'
    Throws { Test-StonewaveSignature $donor ($subject + ', OU=x') $caPin } 'not by' 'a subject that only starts like the pinned one'
    Throws { Test-StonewaveSignature $donor $subject.ToLowerInvariant() $caPin } 'not by' 'the subject is compared exactly, also the case'
    Throws { Test-StonewaveSignature $donor $subject ('0' * 40) } 'not issued under the authority' 'another authority'
    Throws { Test-StonewaveSignature $donor $subject $thumb } 'not issued under the authority' 'the certificate itself is not an authority above itself'
    # the certificates inside the signature
    # (the stand-in is signed through a catalog, with nothing inside the file: look for a program that carries its signature)
    $embDonor = $null
    foreach ($f in (Get-ChildItem "$env:ProgramFiles\*\*.exe", "$env:ProgramFiles\*\*\*.exe", "${env:ProgramFiles(x86)}\*\*.exe", "$env:windir\*.exe" -ErrorAction SilentlyContinue | Select-Object -First 600)) {
        if ($f.Length -gt 20MB) { continue }
        $c = @(Get-EmbeddedCertificates $f.FullName)
        if ($c.Count -ge 2) {
            $sg = Get-AuthenticodeSignature -FilePath $f.FullName
            if ($sg.Status -eq 'Valid' -and $sg.SignerCertificate -and (@($c | Where-Object { $_.Thumbprint -eq $sg.SignerCertificate.Thumbprint }).Count -eq 1)) { $embDonor = $f.FullName; break }
        }
    }
    if ($embDonor) { Ok } else { Write-Host 'note: no program with an embedded signature found on this machine; that check is skipped' }
    Expect (@(Get-EmbeddedCertificates $donor).Count -ge 0) 'a catalog-signed program gives nothing or something, never an error'
    Expect (@(Get-EmbeddedCertificates $plain).Count -eq 0) 'a file that is not a program gives none'
    $cut = Join-Path $T 'cut.exe'
    [IO.File]::WriteAllBytes($cut, ([IO.File]::ReadAllBytes($donor))[0..4000])
    Expect (@(Get-EmbeddedCertificates $cut).Count -eq 0) 'a program cut short gives none'
    Expect (@(Get-EmbeddedCertificates (Join-Path $T 'does-not-exist.exe')).Count -eq 0) 'a missing file gives none'
    Throws { Test-StonewaveSignature $donor $subject '@WIN_CA_THUMBPRINTS@' } 'not prepared for Windows' 'an installer that was not prepared'
    Throws { Test-StonewaveSignature $donor $subject '' } 'not prepared for Windows' 'nothing pinned'
    Throws { Test-StonewaveSignature $plain $subject $caPin } 'not valid' 'an unsigned file'
    if ($embDonor) {
        $es = (Get-AuthenticodeSignature -FilePath $embDonor).SignerCertificate
        $ec = New-Object System.Security.Cryptography.X509Certificates.X509Chain
        [void]$ec.Build($es)
        $ePin = $ec.ChainElements[1].Certificate.Thumbprint.ToUpperInvariant()
        Test-StonewaveSignature $embDonor $es.Subject $ePin 6>$null
        Ok
        Throws { Test-StonewaveSignature $embDonor $es.Subject ('0' * 40) } 'not issued under the authority' 'a program that carries its signature, another pin'
    }
    Throws { Test-StonewaveSignature $changed $subject $caPin } 'not valid' 'a signed file with a changed byte'
    # the legacy pin of one release: the certificate's own thumbprint, with the right subject
    Test-StonewaveSignature $donor $subject ('0' * 40) @($thumb) 6>$null
    Ok
    Throws { Test-StonewaveSignature $donor 'CN=Somebody Else' ('0' * 40) @($thumb) } 'not by' 'a legacy thumbprint does not replace the subject'
    Throws { Test-StonewaveSignature $donor $subject ('0' * 40) @('1' * 40) } 'not issued under the authority' 'another legacy thumbprint'

    # ---------------------------------------------------------------- the pins on certificates made here (a chain of our own: no trusted root, so -TestOnlyAcceptUntrustedRoot)
    $made = @()
    try {
        $valid = (Get-Date).AddDays(2)
        function New-TestCert([string]$Subject, $Signer = $null, [switch]$Authority) {
            $a = @{ Subject = $Subject; CertStoreLocation = 'Cert:\CurrentUser\My'; NotAfter = $valid }
            if ($Authority) { $a.KeyUsage = @('CertSign', 'CRLSign'); $a.TextExtension = @('2.5.29.19={critical}{text}ca=1') } else { $a.Type = 'CodeSigningCert' }
            if ($Signer) { $a.Signer = $Signer }
            $c = New-SelfSignedCertificate @a
            $script:made += $c
            return $c
        }
        $root1 = New-TestCert 'CN=Test Root One' -Authority
        $ca1 = New-TestCert 'CN=Test Authority One' $root1 -Authority      # like the real thing: certificate -> authority -> root
        $ca2 = New-TestCert 'CN=Test Authority Two' $root1 -Authority      # another authority under the same root
        $leafA = New-TestCert 'CN=Stonewave Tech LLC, O=Stonewave Tech LLC, L=Sheridan, S=Wyoming, C=US' $ca1
        $want = $leafA.Subject
        $leafA2 = New-TestCert $want $ca1                       # another certificate of the same subject under the same authority (the next one Azure makes)
        $leafB = New-TestCert $want $ca2                        # the same subject under another authority
        $self = New-TestCert $want                              # a certificate that signed itself, the same subject
        $other = New-TestCert 'CN=Somebody Else' $ca1           # our authority, another subject
        Expect ($leafA2.Thumbprint -ne $leafA.Thumbprint -and $leafA2.Subject -ceq $want -and $leafB.Subject -ceq $want -and $self.Subject -ceq $want) 'the test certificates are what they should be'
        $pin = @($ca1.Thumbprint.ToUpperInvariant())
        $rootOnly = @($root1.Thumbprint.ToUpperInvariant())
        function Chain($c, [switch]$Trusted) { if ($Trusted) { return Get-SignerChain $c } else { return Get-SignerChain $c -TestOnlyAcceptUntrustedRoot } }

        Expect ((Test-SignerPins $leafA (Chain $leafA) $want $pin @()) -match 'issued under') 'a certificate under the pinned authority passes'
        Expect ((Test-SignerPins $leafA2 (Chain $leafA2) $want $pin @()) -match 'issued under') 'a DIFFERENT certificate under the same authority passes too (the next one Azure makes)'
        Throws { Test-SignerPins $leafB (Chain $leafB) $want $pin @() } 'not issued under the authority' 'the same subject under another authority fails'
        Throws { Test-SignerPins $self (Chain $self) $want $pin @() } 'not issued under the authority' 'a self-signed certificate with the same subject fails'
        Throws { Test-SignerPins $self (Chain $self) $want @($self.Thumbprint.ToUpperInvariant()) @() } 'not issued under the authority' 'its own thumbprint as an authority does not help'
        Throws { Test-SignerPins $other (Chain $other) $want $pin @() } 'not by' 'our authority and another subject fails'
        Expect ((Chain $leafA).Thumbprints.Count -eq 3) 'the made chain is certificate, authority, root'
        Throws { Test-SignerPins $leafA (Chain $leafA) $want $rootOnly @() } 'not issued under the authority' 'the root as the pin is refused (it would be every certificate under that root)'
        Throws { Test-SignerPins $leafB (Chain $leafB) $want $rootOnly @() } 'not issued under the authority' 'also for the other authority under the same root'
        Throws { Test-SignerPins $leafA (Chain $leafA) $want @($leafA.Thumbprint.ToUpperInvariant()) @() } 'not issued under the authority' 'the certificate as its own pin is refused'
        # a made-up chain of two (the certificate and a root that is the "authority"): the root is not an authority that counts
        $evilRoot = New-TestCert 'CN=Evil Root' -Authority
        $evilLeaf = New-TestCert $want $evilRoot
        Throws { Test-SignerPins $evilLeaf (Chain $evilLeaf) $want @($evilRoot.Thumbprint.ToUpperInvariant()) @() } 'not issued under the authority' 'a made-up root as the "authority" is refused'
        # Windows must also trust the chain: an authority of our own made up has no trusted root
        $untrusted = Chain $leafA -Trusted
        Expect (-not $untrusted.Ok) 'a chain that ends in a root Windows does not trust is not ok'
        Throws { Test-SignerPins $leafA $untrusted $want $pin @() } 'not valid' 'and is refused even under the pinned authority'
        # the legacy pin: that very certificate, nothing else
        Expect ((Test-SignerPins $leafA $untrusted $want @('0' * 40) @($leafA.Thumbprint.ToUpperInvariant())) -match 'certificate') 'the legacy leaf pin passes by itself'
        Throws { Test-SignerPins $leafA2 (Chain $leafA2) $want @('0' * 40) @($leafA.Thumbprint.ToUpperInvariant()) } 'not issued under the authority' 'but not for another certificate'

        # a machine that has never seen the authority (offline, nothing cached): a made-up chain that was never built here, kept out of the stores
        $root3 = New-TestCert 'CN=Test Root Three' -Authority
        $ca3 = New-TestCert 'CN=Test Authority Three' $root3 -Authority
        $leaf3 = New-TestCert $want $ca3
        $forged = New-TestCert 'CN=Test Authority Three' -Authority          # same name as ca3, another key, signed by nobody we know
        $raw = @{}
        foreach ($c in @($root3, $ca3, $leaf3, $forged)) { $raw[$c.Thumbprint] = $c.RawData }
        $t3 = $ca3.Thumbprint.ToUpperInvariant()
        # (building a chain copies the authorities it meets into the user's "CA" store: out of there too, or the chain is found without the extra material)
        foreach ($c in @($root3, $ca3, $leaf3, $forged)) { foreach ($st in 'My', 'CA') { Remove-Item -LiteralPath "Cert:\CurrentUser\$st\$($c.Thumbprint)" -Force -ErrorAction SilentlyContinue } }
        # (a fresh PowerShell: the one running here remembers the certificates it has seen)
        [IO.File]::WriteAllBytes((Join-Path $T 'c-leaf.cer'), $raw[$leaf3.Thumbprint])
        [IO.File]::WriteAllBytes((Join-Path $T 'c-ca.cer'), $raw[$ca3.Thumbprint])
        [IO.File]::WriteAllBytes((Join-Path $T 'c-root.cer'), $raw[$root3.Thumbprint])
        [IO.File]::WriteAllBytes((Join-Path $T 'c-forged.cer'), $raw[$forged.Thumbprint])
        $child = Join-Path $T 'chain-child.ps1'
        @'
param($Inner, $Dir, $Want, $Pin)
$ErrorActionPreference = 'Stop'
. $Inner
function Cer($n) { New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 (,[IO.File]::ReadAllBytes((Join-Path $Dir "c-$n.cer"))) }
$leaf = Cer 'leaf'
function Pins($chain) { try { Test-SignerPins $leaf $chain $Want @($Pin) @() } catch { 'refused: ' + $_.Exception.Message } }
$a = Get-SignerChain $leaf -TestOnlyAcceptUntrustedRoot
"without ok=$($a.Ok) status=$($a.Status) n=$(@($a.Thumbprints).Count)"
"without-pins $(Pins $a)"
$b = Get-SignerChain $leaf -TestOnlyAcceptUntrustedRoot -Extra @((Cer 'ca'), (Cer 'root'))
"with ok=$($b.Ok) status=$($b.Status) n=$(@($b.Thumbprints).Count)"
"with-pins $(Pins $b)"
$c = Get-SignerChain $leaf -TestOnlyAcceptUntrustedRoot -Extra @((Cer 'forged'))
"forged ok=$($c.Ok) status=$($c.Status)"
"forged-pins $(Pins $c)"
'@ | Set-Content -Encoding UTF8 -LiteralPath $child
        $co = (& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $child $innerFile $T $want $t3 2>&1 | Out-String)
        Expect ($co -match 'without ok=False') "without the authority at hand the chain cannot be built  [$($co -replace '\s+', ' ')]"
        Expect ($co -match 'without-pins refused: .*not valid') 'and the check refuses'
        Expect ($co -match 'with ok=True status=\S+ n=3') 'with the certificates the signature carries the chain is built'
        Expect ($co -match 'with-pins issued under') 'and the pinned authority is found in it'
        Expect ($co -match 'forged ok=False') 'an authority that carries the right name but not the right key does not complete the chain'
        Expect ($co -match 'forged-pins refused: .*not valid') 'and the check refuses'
    } finally {
        foreach ($c in $made) { foreach ($st in 'My', 'CA') { Remove-Item -LiteralPath "Cert:\CurrentUser\$st\$($c.Thumbprint)" -Force -ErrorAction SilentlyContinue } }
        # and what chain building left in the "CA" store from our made-up authorities (only these subjects, made in this test)
        foreach ($c in @(Get-ChildItem Cert:\CurrentUser\CA | Where-Object { $_.Subject -match '^CN=(Test (Root|Authority) (One|Two|Three)|Evil Root)$' })) { Remove-Item -LiteralPath "Cert:\CurrentUser\CA\$($c.Thumbprint)" -Force -ErrorAction SilentlyContinue }
    }

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
    $SignerCaThumbprints = '0' * 40
    Refused 'wrong-pin' { param($d) } 'not issued under the authority'
    $SignerCaThumbprints = $caPin
    # the legacy certificate pin belongs to one release: 0.2.0 passes by it, another release does not
    $LegacyLeaf = @{ '0.2.0' = $thumb }
    $SignerCaThumbprints = '0' * 40
    $d = Reset 'site-legacy'
    New-Release $d '0.2.0' 'cafe0123' $donor -Latest
    $script:says = 'stonewave 0.2.0 (cafe0123)'
    Expect ((Run) -match 'installed') 'release 0.2.0 passes by its legacy certificate pin'
    $d = Reset 'site-legacy2'
    New-Release $d '0.2.1' 'cafe0124' $donor -Latest
    $script:says = 'stonewave 0.2.1 (cafe0124)'
    Throws { Install-Stonewave 6>$null } 'not issued under the authority' 'release 0.2.1 does not (the legacy pin is for 0.2.0 only)'
    $LegacyLeaf = @{ '0.2.0' = '8A69FFDF224024FD656E942B043D92D5AA381505' }
    $SignerCaThumbprints = $caPin
    $script:says = 'stonewave 0.2.0 (cafe0123)'

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
        # the folder is in PATH already, written with %LOCALAPPDATA%
        $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($sub, $true)
        $k.SetValue('Path', 'C:\x;%LOCALAPPDATA%\Stonewave\bin', [Microsoft.Win32.RegistryValueKind]::ExpandString)
        $k.Close()
        Expect (-not (Add-ToUserPath (Join-Path $env:LOCALAPPDATA 'Stonewave\bin') $sub)) 'an entry written with %LOCALAPPDATA% counts as the folder'
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
    (Get-Content -Raw -LiteralPath (Join-Path $here 'install\cli\install.ps1.in')) -replace '@WIN_CA_THUMBPRINTS@', ('F' * 40) | Set-Content -Encoding UTF8 -LiteralPath $fresh
    $sb2 = [scriptblock]::Create((Get-Content -Raw -LiteralPath $fresh))
    # `irm | iex` runs the text with Invoke-Expression in the caller's scope. A child PowerShell plays the person's session: nothing of the installer
    # (functions, variables, StrictMode, the error preference, the TLS setting) may stay, and the options reach it through $args.
    # (the text is pointed at a port where nothing listens, and the profile folder is a temporary one: no internet, nothing of the real profile)
    $iexText = Join-Path $T 'iex-text.ps1'
    (Get-Content -Raw -LiteralPath $rendered).Replace('https://stonewavetech.com/cli', 'https://127.0.0.1:9/cli') | Set-Content -Encoding UTF8 -LiteralPath $iexText
    $iexProfile = Join-Path $T 'iex-profile'
    New-Item -ItemType Directory -Force -Path $iexProfile | Out-Null
    $check = Join-Path $T 'iexcheck.ps1'
    @'
# the options of the installer come in as this script's own arguments ($args), as they would be in a session
$out = $null; $vars1 = $null; $fn1 = $null; $new = $null; $newfn = $null; $eap1 = $null; $tls1 = $null; $strict = $null; $text = $null
$vars0 = $null; $fn0 = $null; $eap0 = $null; $tls0 = $null
$vars0 = @(Get-Variable | ForEach-Object Name); $fn0 = @(Get-ChildItem Function: | ForEach-Object Name)
$eap0 = $ErrorActionPreference; $tls0 = [string][Net.ServicePointManager]::SecurityProtocol
$text = Get-Content -Raw -LiteralPath $env:SW_TEXT
$out = Invoke-Expression $text *>&1 | Out-String
$vars1 = @(Get-Variable | ForEach-Object Name); $fn1 = @(Get-ChildItem Function: | ForEach-Object Name)
$new = @($vars1 | Where-Object { ($vars0 -notcontains $_) -and ($_ -ne 'LASTEXITCODE') })   # LASTEXITCODE is the one thing it sets, on purpose, when it fails
$newfn = @($fn1 | Where-Object { $fn0 -notcontains $_ })
$strict = 'off'; try { $undefinedVariableXyz | Out-Null } catch { $strict = 'ON' }
$eap1 = $ErrorActionPreference; $tls1 = [string][Net.ServicePointManager]::SecurityProtocol
Write-Output "NEWVARS=$($new -join ',')"
Write-Output "NEWFUNCS=$($newfn -join ',')"
Write-Output "EAP=$eap0>$eap1"
Write-Output "TLS=$($tls0 -eq $tls1)"
Write-Output "STRICT=$strict"
Write-Output ('OUT=' + ($out -replace '\r?\n', ' | '))
'@ | Set-Content -Encoding UTF8 -LiteralPath $check
    $env:SW_TEXT = $iexText
    $realProfile = $env:LOCALAPPDATA
    $env:LOCALAPPDATA = $iexProfile
    try { $iexOut = (& powershell -NoProfile -ExecutionPolicy Bypass -File $check) | Out-String }
    finally { $env:LOCALAPPDATA = $realProfile; Remove-Item Env:SW_TEXT }
    Expect ($iexOut -match 'NEWVARS=\s*\r?\n') "irm | iex leaves no variables behind: $iexOut"
    Expect ($iexOut -match 'NEWFUNCS=\s*\r?\n') "and no functions: $iexOut"
    Expect ($iexOut -match 'EAP=Continue>Continue') 'the error preference is untouched'
    Expect ($iexOut -match 'TLS=True') 'the TLS setting is untouched'
    Expect ($iexOut -match 'STRICT=off') 'strict mode did not leak'
    Expect ($iexOut -match 'Stonewave CLI installer' -and $iexOut -match 'download failed') "the installer really ran through Invoke-Expression (and failed closed, nothing listens there): $iexOut"
    Expect (-not (Test-Path (Join-Path $iexProfile 'Stonewave\bin\stonewave.exe'))) 'and installed nothing'
    $hostOut = & $sb2 -FromDir $d -BinDir (Join-Path $T 'bin-sb2') -NoPath 6>&1 *>&1 | Out-String
    Expect ($hostOut -match 'not by|not issued under the authority|not valid') 'with other pins the program is refused'
    Expect (-not (Test-Path (Join-Path $T 'bin-sb2\stonewave.exe'))) 'and not installed'
}
finally {
    Remove-Item -Recurse -Force $T -ErrorAction SilentlyContinue
}
Write-Host "$script:pass passed, $script:failed failed"
if ($script:failed -ne 0) { exit 1 }
