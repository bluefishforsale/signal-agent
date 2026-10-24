# Tauri's bundle.windows.signCommand. It is the only hook that reaches the
# uninstaller NSIS embeds in the installer (via !uninstfinalize), which the
# artifact-signing-action jobs can't see (#76). With -Record it signs nothing and
# appends the path to a log instead, so unsigned builds can prove the hook still
# reaches the uninstaller.
param(
    [Parameter(Mandatory)][string]$Path,
    [string]$Record
)
$ErrorActionPreference = 'Stop'

if ($Record) {
    Add-Content -Path $Record -Value $Path
    exit 0
}

# NOTE: keep in step with the version Azure/artifact-signing-action@v2 pins, so
# the installer and its payload are signed by the same tooling.
Import-Module ArtifactSigning -RequiredVersion 0.1.20
Invoke-ArtifactSigning `
    -Endpoint $env:AZURE_SIGNING_ENDPOINT `
    -CodeSigningAccountName $env:AZURE_SIGNING_ACCOUNT `
    -CertificateProfileName $env:AZURE_CERT_PROFILE `
    -Files $Path `
    -FileDigest SHA256 `
    -TimestampRfc3161 'http://timestamp.acs.microsoft.com' `
    -TimestampDigest SHA256 `
    -ExcludeEnvironmentCredential:$true

$sig = Get-AuthenticodeSignature -FilePath $Path
if ($sig.Status -ne 'Valid') { throw "$Path has signature $($sig.Status) after signing" }
Write-Output "signed $Path"
