# The release gate for a Windows program, run on the REAL file before a release is published (see docs/release.md):
# prints what Windows says about its signature and what the installer will compare, and fails when the installer would refuse it.
#
#   powershell -File scripts/check-windows-signature.ps1 -Path stonewave.exe [-Pin <thumbprint of the issuing authority>] [-Version 0.2.0]
#
# It uses the installer's own functions (install/cli/install.ps1.in), so what it says is what `irm .../install.ps1 | iex` will do.
# Nothing is installed or run. Without -Pin the authority found in the chain is shown and not checked.
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [string]$Pin = '',
    [string]$Version = ''
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Path = (Resolve-Path -LiteralPath $Path).Path

# the installer's functions and constants: the template with the wrapper lines taken off
$lines = @(Get-Content -LiteralPath (Join-Path $here 'install\cli\install.ps1.in'))
$first = [array]::FindIndex($lines, [Predicate[string]]{ param($l) $l -like '*stonewave-installer-begin' })
$last = [array]::FindIndex($lines, [Predicate[string]]{ param($l) $l -like '*stonewave-installer-end' })
if ($first -lt 0 -or $last -le $first) { throw 'install.ps1.in: the wrapper markers are not there' }
$inner = Join-Path ([IO.Path]::GetTempPath()) ("check-sig-" + [guid]::NewGuid().ToString('N') + '.ps1')
try {
    ($lines[($first + 1)..($last - 1)]) | Set-Content -Encoding UTF8 -LiteralPath $inner
    . $inner
} finally {
    Remove-Item -LiteralPath $inner -Force -ErrorAction SilentlyContinue
}

$sig = Get-AuthenticodeSignature -FilePath $Path
Write-Host "file:               $Path"
Write-Host ("sha256:             " + (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant())
Write-Host "Windows says:       $($sig.Status)  $($sig.StatusMessage)"
if (-not $sig.SignerCertificate) { Write-Host 'no signer certificate'; exit 1 }
$cert = $sig.SignerCertificate
Write-Host "subject:            $($cert.Subject)"
Write-Host "subject in the installer: $SignerSubject"
$subjectOk = ($cert.Subject -ceq $SignerSubject)
Write-Host "subject equal (exact, case too): $subjectOk"
Write-Host "certificate:        $($cert.Thumbprint)  valid $($cert.NotBefore.ToString('yyyy-MM-dd HH:mm')) .. $($cert.NotAfter.ToString('yyyy-MM-dd HH:mm'))  ($(if ($cert.NotAfter -lt (Get-Date)) { 'EXPIRED now' } else { 'not expired yet' }))"
if ($sig.TimeStamperCertificate) { Write-Host "timestamp by:       $($sig.TimeStamperCertificate.Subject)" } else { Write-Host 'timestamp:          none' }
$embedded = @(Get-EmbeddedCertificates $Path)
Write-Host "certificates inside the signature: $($embedded.Count)"

$chain = Get-SignerChain $cert -Extra $embedded
Write-Host "chain ok:           $($chain.Ok)  $($chain.Status)  $($chain.Warning)"
$i = 0
$n = @($chain.Thumbprints).Count
foreach ($c in @($chain.Thumbprints)) {
    $role = if ($i -eq 0) { 'certificate' } elseif ($i -eq $n - 1) { 'root' } else { 'authority (counts as a pin)' }
    Write-Host ("  [{0}] {1}  {2}" -f $i, $c, $role)
    $i++
}
$fail = $false
if ($sig.Status -ne 'Valid') { $fail = $true }
if (-not $subjectOk) { $fail = $true }
if (-not $chain.Ok) { $fail = $true }
if ($Pin) {
    $pins = @($Pin.ToUpperInvariant() -split '[,\s]+' | Where-Object { $_ })
    try {
        $what = Test-SignerPins $cert $chain $SignerSubject $pins @()
        Write-Host "installer would accept: $what"
    } catch {
        Write-Host "installer would REFUSE: $($_.Exception.Message)"
        $fail = $true
    }
} else {
    Write-Host 'no -Pin given: the authority is not checked (the authorities above are the candidates)'
}
if ($Version -and $LegacyLeaf.ContainsKey($Version)) {
    Write-Host "legacy leaf pin for ${Version}: $($LegacyLeaf[$Version])  ($(if ($cert.Thumbprint.ToUpperInvariant() -eq $LegacyLeaf[$Version]) { 'this file' } else { 'NOT this file' }))"
}
if ($fail) { Write-Host 'RESULT: the installer would refuse this file'; exit 1 }
Write-Host 'RESULT: ok'
