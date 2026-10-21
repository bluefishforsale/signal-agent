# Refuses to pass along any Windows executable that lacks a valid Authenticode
# signature, so an unsigned file can't reach a release (PRD acceptance 11).
param([Parameter(Mandatory)][string]$Folder)
$ErrorActionPreference = 'Stop'

$exes = @(Get-ChildItem -Path $Folder -Recurse -Filter *.exe)
if ($exes.Count -eq 0) { throw "no .exe files under $Folder" }

foreach ($exe in $exes) {
    $sig = Get-AuthenticodeSignature -FilePath $exe.FullName
    if ($sig.Status -ne 'Valid') { throw "$($exe.Name): signature is $($sig.Status)" }
    Write-Output "$($exe.Name): $($sig.SignerCertificate.Subject)"
}
