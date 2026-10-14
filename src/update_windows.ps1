param([int]$ProcessId, [string]$Executable, [string]$Version, [string]$OriginalHash, [string]$LockFile)
$ErrorActionPreference = 'Stop'
$lock = $null
$probe = $null
$backup = Join-Path $PSScriptRoot 'previous.exe'
$candidate = Join-Path $PSScriptRoot 'moocp.exe'
$success = $false
try {
    Wait-Process -Id $ProcessId -ErrorAction SilentlyContinue
    $lock = [System.IO.File]::Open($LockFile, 'OpenOrCreate', 'ReadWrite', 'ReadWrite')
    $lock.Lock(0, 1)
    for ($attempt = 0; $attempt -lt 120; $attempt++) {
        if ((Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant() -ne $OriginalHash) {
            throw 'Moocp changed while the update was waiting. Run update again.'
        }
        try {
            [System.IO.File]::Replace($candidate, $Executable, $backup)
            break
        } catch {
            if ($attempt -eq 119) { throw 'Close CLI sessions using Moocp and run update again. The previous version was kept.' }
            Start-Sleep -Milliseconds 500
        }
    }
    $output = Join-Path $PSScriptRoot 'version.txt'
    $probe = Start-Process -FilePath $Executable -ArgumentList '--version' -NoNewWindow -PassThru `
        -RedirectStandardOutput $output -RedirectStandardError (Join-Path $PSScriptRoot 'version-error.txt')
    if (-not $probe.WaitForExit(15000)) {
        $probe.Kill()
        $probe.WaitForExit()
        throw 'The updated executable timed out.'
    }
    # Read ExitCode before disposing the process handle.
    $exitCode = $probe.ExitCode
    $probe.Dispose()
    $probe = $null
    if ($exitCode -ne 0 -or (Get-Item -LiteralPath $output).Length -gt 128 -or
        (Get-Content -LiteralPath $output -Raw).Trim() -ne "moocp $Version") {
        throw 'The updated executable failed its version check.'
    }
    $success = $true
} catch {
    $message = $_.Exception.Message
    if ($probe) { $probe.Dispose(); $probe = $null }
    if (Test-Path -LiteralPath $backup) {
        try {
            [System.IO.File]::Replace($backup, $Executable, (Join-Path $PSScriptRoot 'rejected.exe'))
            $message += ' The previous version was restored.'
        } catch { $message += " Rollback could not finish. Previous executable: $backup" }
    }
    $message | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'failure.txt')
} finally {
    if ($lock) { $lock.Dispose() }
    if ($success) { Remove-Item -LiteralPath $PSScriptRoot -Recurse -Force }
}
