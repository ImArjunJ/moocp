# Agent setup

Install Moocp and connect it to the CLI running this conversation. Complete the
work yourself; the student supplies their Moodle URL and browser sign-in.

## 1. Check what's already there

Identify the current client: `codex`, `claude` (Claude Code), or `gemini`.
Use an existing Moodle URL from the conversation, otherwise ask for it once.
Never ask for passwords, MFA codes, or cookies in chat.

Find an existing `moocp` executable, or check its normal install location below.
If present, run `moocp status --check`. A healthy connection only needs client
registration; an expired saved account needs `moocp login`, not a new profile.
Keep existing accounts and other MCP entries.

## 2. Install

Use the native release installer first. Download the script, inspect it, and run
it. Do not use sudo, disable TLS verification, or edit shell startup files.

macOS/Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/ImArjunJ/moocp/main/install.sh -o /tmp/moocp-install.sh
sh /tmp/moocp-install.sh
```

The binary is at `${XDG_BIN_HOME:-$HOME/.local/bin}/moocp`, unless
`MOOCP_INSTALL_DIR` was explicitly set. Use its absolute path for the next steps;
PATH changes are unnecessary.

Windows PowerShell:

```powershell
$script = Join-Path $env:TEMP 'moocp-install.ps1'
Invoke-WebRequest https://raw.githubusercontent.com/ImArjunJ/moocp/main/install.ps1 -OutFile $script
& $script
```

The binary is at `$env:LOCALAPPDATA\Programs\Moocp\moocp.exe`. If the terminal's
execution policy blocks the downloaded script, use a process-local
`powershell -NoProfile -ExecutionPolicy Bypass -File $script`; do not change the
machine's policy. Both installers verify the release SHA-256 before installation.

If a native asset is unavailable or incompatible, build from source in a fresh
temporary checkout:

```sh
git clone https://github.com/ImArjunJ/moocp.git
cd moocp
cargo install --path . --locked
```

Source builds need Rust 1.88+, a C/C++ build toolchain, and CMake. If dependencies
are missing, use the machine's standard package manager within the permissions
available to you. Prefer the release instead of installing build tools needlessly.

## 3. Connect

Run the installed executable with the student's real URL and the current client:

```sh
/absolute/path/to/moocp setup https://the-students-moodle.example --client codex
```

Use `--client claude` or `--client gemini` as appropriate. Keep the default OS
credential store. If it is unavailable, explain the issue; do not silently switch
to plaintext `--credential-store file`. If the same account is already connected,
use `moocp install CLIENT`; use `--replace` only for an existing Moocp entry that
needs the new executable path, preserving other servers.

Setup opens a visible browser. Tell the student to complete SSO/MFA there, then
wait for the command to finish. Launch it using your environment's background
process/PTY facility if needed so the student can interact while it runs. Do not
type credentials for them or terminate the waiting login process early.

Chrome, Chromium, or Edge is needed. `moocp doctor` checks discovery. If one is
installed but not found, pass `--browser /absolute/path/to/browser`; otherwise
install a Chromium-family browser using the platform's usual installer.

## 4. Verify and finish

Run `moocp status --check`. Verify registration using the current client's MCP
listing. If your MCP connection can refresh, call `moodle_list_courses`; otherwise
tell the student to restart this CLI session once. A saved session or config entry
alone is not proof of a working connection. State exactly which checks passed.

Finish with one useful prompt, such as “what coursework is due this week?”
Never run `moocp review` for the student or approve their coursework actions.
