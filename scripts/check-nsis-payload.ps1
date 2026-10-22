# Fails the build if the NSIS installer would not put signal-agent.exe next to
# the app exe, which is where Claude's config and the logon task launch it.
# With -RequireSigned, both payload executables must also carry a valid
# Authenticode signature, so an unsigned binary can't hide inside a signed
# installer.
param(
    [Parameter(Mandatory)][string]$Installer,
    [switch]$RequireSigned
)
$ErrorActionPreference = 'Stop'

$out = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
& 7z x -y "-o$out" $Installer | Out-Null
if ($LASTEXITCODE -ne 0) { throw "7z could not unpack $Installer" }

$expected = @('relay-app.exe', 'signal-agent.exe')
foreach ($name in $expected) {
    $hits = @(Get-ChildItem -Path $out -Recurse -Filter $name)
    if ($hits.Count -ne 1) { throw "$Installer payload has $($hits.Count) copies of $name, expected 1" }
}
$dirs = @($expected | ForEach-Object { (Get-ChildItem -Path $out -Recurse -Filter $_).DirectoryName } | Sort-Object -Unique)
if ($dirs.Count -ne 1) { throw "relay-app.exe and signal-agent.exe install to different directories" }

if ($RequireSigned) {
    foreach ($name in $expected) {
        $file = Get-ChildItem -Path $out -Recurse -Filter $name
        $sig = Get-AuthenticodeSignature -FilePath $file.FullName
        if ($sig.Status -ne 'Valid') { throw "$name inside $Installer has signature $($sig.Status)" }
    }
}
Write-Output "payload ok: $($expected -join ', ') install side by side"
