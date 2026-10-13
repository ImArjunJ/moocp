#!/bin/sh
# Install the current release for this machine. No root access or PATH edits.
set -eu

fail() { printf 'moocp: %s\n' "$*" >&2; exit 1; }
for task_command in curl tar mktemp; do
    command -v "$task_command" >/dev/null 2>&1 || fail "Install $task_command and try again."
done

case "$(uname -s):$(uname -m)" in
    Linux:x86_64) task_target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) task_target=aarch64-unknown-linux-gnu ;;
    Darwin:arm64) task_target=aarch64-apple-darwin ;;
    Darwin:x86_64) task_target=x86_64-apple-darwin ;;
    *) fail "No native release for this machine. See https://github.com/ImArjunJ/moocp/blob/main/SETUP.md for the source install." ;;
esac

task_install_dir=${MOOCP_INSTALL_DIR:-${XDG_BIN_HOME:-"$HOME/.local/bin"}}
case "$task_install_dir" in /*) ;; *) fail 'MOOCP_INSTALL_DIR must be an absolute path.' ;; esac
[ ! -d "$task_install_dir/moocp" ] || fail 'The destination moocp is a directory.'
task_temp=$(mktemp -d)
trap 'rm -rf "$task_temp"' EXIT HUP INT TERM
task_asset="moocp-$task_target.tar.gz"
task_base=https://github.com/ImArjunJ/moocp/releases/latest/download

printf 'Downloading moocp for %s…\n' "$task_target"
curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 \
    "$task_base/$task_asset" -o "$task_temp/$task_asset"
curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 \
    "$task_base/$task_asset.sha256" -o "$task_temp/$task_asset.sha256"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$task_temp" && sha256sum -c "$task_asset.sha256")
elif command -v shasum >/dev/null 2>&1; then
    (cd "$task_temp" && shasum -a 256 -c "$task_asset.sha256")
else
    fail 'Install sha256sum or shasum to verify the download.'
fi

tar -xzf "$task_temp/$task_asset" -C "$task_temp"
[ -f "$task_temp/moocp" ] || fail 'The release archive contains no moocp executable.'
chmod +x "$task_temp/moocp"
"$task_temp/moocp" --version || fail 'This build cannot run here. Follow the source install in SETUP.md.'
mkdir -p "$task_install_dir"
# Stage beside the destination so replacement is atomic, including across disks.
task_staged=$(mktemp "$task_install_dir/.moocp.XXXXXXXX")
trap 'rm -rf "$task_temp"; rm -f "$task_staged"' EXIT HUP INT TERM
cp "$task_temp/moocp" "$task_staged"
chmod 755 "$task_staged"
mv -f "$task_staged" "$task_install_dir/moocp"
printf '\nInstalled: %s/moocp\nNext: "%s/moocp" setup https://your-moodle.example\n' "$task_install_dir" "$task_install_dir"
