param([int]$ProcessId, [string]$Executable, [string]$Version, [string]$OriginalHash, [string]$LockFile, [string]$FailureLog)
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
    # Start-Process can lose its process handle and report a null ExitCode in Windows PowerShell.
    $probe = New-Object System.Diagnostics.Process
    $probe.StartInfo.FileName = $Executable
    $probe.StartInfo.Arguments = '--version'
    $probe.StartInfo.UseShellExecute = $false
    $probe.StartInfo.CreateNoWindow = $true
    $probe.StartInfo.RedirectStandardOutput = $true
    $probe.StartInfo.RedirectStandardError = $true
    $null = $probe.Start()
    $output = $probe.StandardOutput.ReadToEndAsync()
    $errors = $probe.StandardError.ReadToEndAsync()
    if (-not $probe.WaitForExit(15000)) {
        $probe.Kill()
        $probe.WaitForExit()
        throw 'The updated executable timed out.'
    }
    # Read ExitCode before disposing the process handle.
    $exitCode = $probe.ExitCode
    $versionText = $output.Result
    $null = $errors.Result
    $probe.Dispose()
    $probe = $null
    if ($exitCode -ne 0 -or $versionText.Length -gt 128 -or $versionText.Trim() -ne "moocp $Version") {
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
    $message | Set-Content -LiteralPath $FailureLog
} finally {
    if ($lock) { $lock.Dispose() }
    if ($success) {
        Remove-Item -LiteralPath $PSScriptRoot -Recurse -Force
        Remove-Item -LiteralPath $FailureLog -Force
    }
}
