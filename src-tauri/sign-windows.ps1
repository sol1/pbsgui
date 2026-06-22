#requires -Version 5
# Sign one Windows artifact with the DigiCert KeyLocker code-signing certificate.
#
# Tauri invokes this once per artifact via bundle.windows.signCommand (see
# signing.conf.json), substituting the file path for %1; Tauri runs it from the
# src-tauri directory, so this script lives next to tauri.conf.json. The release
# workflow runs `smctl windows certsync` first (puts the cert in the Windows
# store and activates the KeyLocker KSP) and sets SM_CODE_SIGNING_CERT_SHA1_HASH
# to the signing certificate's thumbprint. signtool then signs through the KSP.
#
# This path only runs when signing is enabled (the DigiCert secrets are present);
# unsigned builds never call it.
[CmdletBinding()]
param([Parameter(Mandatory = $true, Position = 0)][string]$File)

$ErrorActionPreference = 'Stop'

$thumbprint = $env:SM_CODE_SIGNING_CERT_SHA1_HASH
if ([string]::IsNullOrWhiteSpace($thumbprint)) {
    throw "SM_CODE_SIGNING_CERT_SHA1_HASH is not set; cannot sign '$File'."
}

# Use the newest signtool from the installed Windows SDK.
$signtool = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe' -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $signtool) {
    throw "signtool.exe not found (install the Windows SDK)."
}

# RFC3161 timestamp so the signature stays valid after the certificate expires.
& $signtool.FullName sign `
    /sha1 $thumbprint `
    /tr http://timestamp.digicert.com /td sha256 /fd sha256 `
    /v "$File"
if ($LASTEXITCODE -ne 0) {
    throw "signtool failed (exit $LASTEXITCODE) for '$File'."
}
