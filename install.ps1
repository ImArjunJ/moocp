# Install the current Windows release into the user's profile.
param([string]$InstallDir = "$env:LOCALAPPDATA\Programs\Moocp")
$ErrorActionPreference = 'Stop'
if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
    throw 'InstallDir must be an absolute path.'
}
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$asset = 'moocp-x86_64-pc-windows-msvc.zip'
$base = 'https://github.com/ImArjunJ/moocp/releases/latest/download'
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ('moocp-' + [guid]::NewGuid())
$staged = $null
$backup = $null
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    Write-Host 'Downloading moocp for Windows…'
    $archive = Join-Path $temporary $asset
    Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile $archive
    $checksum = Join-Path $temporary "$asset.sha256"
    Invoke-WebRequest -UseBasicParsing "$base/$asset.sha256" -OutFile $checksum
    $expected = (Get-Content $checksum -Raw).Trim().Split(' ')[0].ToLowerInvariant()
    $actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -notmatch '^[a-f0-9]{64}$' -or $actual -ne $expected) {
        throw 'Release checksum did not match. Nothing was installed.'
    }
    Expand-Archive -LiteralPath $archive -DestinationPath (Join-Path $temporary 'package')
    $binary = Join-Path $temporary 'package\moocp.exe'
    & $binary --version
    if ($LASTEXITCODE -ne 0) { throw 'The downloaded executable cannot run on this machine.' }
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $destination = Join-Path $InstallDir 'moocp.exe'
    $staged = Join-Path $InstallDir ('.moocp-' + [guid]::NewGuid() + '.exe')
    Copy-Item -LiteralPath $binary -Destination $staged
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        # PowerShell coerces a null string argument into an empty backup path.
        $backup = Join-Path $InstallDir ('.moocp-previous-' + [guid]::NewGuid() + '.exe')
        [System.IO.File]::Replace($staged, $destination, $backup)
    } else {
        [System.IO.File]::Move($staged, $destination)
    }
    Write-Host "`nInstalled: $destination"
    Write-Host "Next: & `"$destination`" setup https://your-moodle.example"
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force
    if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force }
    if ($backup -and (Test-Path -LiteralPath $backup)) { Remove-Item -LiteralPath $backup -Force }
}
