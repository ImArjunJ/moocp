$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ('moocp-test-' + [guid]::NewGuid())
$fixture = Join-Path $temporary 'fixture'
New-Item -ItemType Directory -Path $fixture -Force | Out-Null
$asset = 'moocp-x86_64-pc-windows-msvc.zip'
try {
    Copy-Item (Join-Path $repo 'target\debug\moocp.exe') (Join-Path $fixture 'moocp.exe')
    Compress-Archive -Path (Join-Path $fixture 'moocp.exe') -DestinationPath (Join-Path $fixture $asset)
    $hash = (Get-FileHash (Join-Path $fixture $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $asset" | Set-Content (Join-Path $fixture "$asset.sha256")
    function Invoke-WebRequest {
        param([string]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
        Copy-Item (Join-Path $fixture ([uri]$Uri).Segments[-1]) $OutFile
    }
    $destination = Join-Path $temporary 'install with spaces'
    & (Join-Path $repo 'install.ps1') -InstallDir $destination
    & (Join-Path $repo 'install.ps1') -InstallDir $destination
    $binary = Join-Path $destination 'moocp.exe'
    if ((& $binary --version) -ne 'moocp 0.1.0') { throw 'Version check failed.' }
    $before = (Get-FileHash $binary -Algorithm SHA256).Hash
    ('0' * 64 + "  $asset") | Set-Content (Join-Path $fixture "$asset.sha256")
    $rejected = $false
    try { & (Join-Path $repo 'install.ps1') -InstallDir $destination }
    catch { $rejected = $_.Exception.Message -like '*checksum*' }
    if (-not $rejected) { throw 'Expected a checksum failure.' }
    if ((Get-FileHash $binary -Algorithm SHA256).Hash -ne $before) { throw 'Existing binary changed.' }
    Write-Host 'Windows installer replacement, spaces, and checksum rejection: passed'
} finally {
    Remove-Item $temporary -Recurse -Force
}
