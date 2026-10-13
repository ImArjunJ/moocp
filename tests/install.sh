#!/bin/sh
# Exercise installation with local release fixtures, including failed verification.
set -eu
task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
task_temp=$(mktemp -d)
trap 'rm -rf "$task_temp"' EXIT HUP INT TERM
mkdir -p "$task_temp/bin" "$task_temp/releases" "$task_temp/package"
printf '#!/bin/sh\nprintf "moocp 0.1.0\\n"\n' > "$task_temp/package/moocp"
chmod +x "$task_temp/package/moocp"
for task_target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin x86_64-apple-darwin; do
    task_asset="moocp-$task_target.tar.gz"
    tar -czf "$task_temp/releases/$task_asset" -C "$task_temp/package" moocp
    (cd "$task_temp/releases" && shasum -a 256 "$task_asset") > "$task_temp/releases/$task_asset.sha256"
done

cat > "$task_temp/bin/curl" <<'SH'
#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        https://*) task_url=$1; shift ;;
        -o) task_destination=$2; shift 2 ;;
        *) shift ;;
    esac
done
cp "$MOOCP_TEST_RELEASES/${task_url##*/}" "$task_destination"
SH
cat > "$task_temp/bin/uname" <<'SH'
#!/bin/sh
case "$1" in -s) printf '%s\n' "$MOOCP_TEST_SYSTEM" ;; -m) printf '%s\n' "$MOOCP_TEST_ARCH" ;; esac
SH
chmod +x "$task_temp/bin/curl" "$task_temp/bin/uname"
export PATH="$task_temp/bin:$PATH" MOOCP_TEST_RELEASES="$task_temp/releases"
export MOOCP_INSTALL_DIR="$task_temp/install with spaces"
for task_machine in Linux:x86_64 Linux:aarch64 Darwin:arm64 Darwin:x86_64; do
    export MOOCP_TEST_SYSTEM=${task_machine%:*} MOOCP_TEST_ARCH=${task_machine#*:}
    sh "$task_root/install.sh" >/dev/null
    [ "$("$MOOCP_INSTALL_DIR/moocp" --version)" = 'moocp 0.1.0' ]
done
# Reinstallation replaces the executable. A corrupt download must preserve it.
printf 'changed release bytes' >> "$task_temp/releases/moocp-x86_64-apple-darwin.tar.gz"
if sh "$task_root/install.sh" >/dev/null 2>&1; then
    printf 'Expected a checksum failure.\n' >&2; exit 1
fi
[ "$("$MOOCP_INSTALL_DIR/moocp" --version)" = 'moocp 0.1.0' ]
printf 'Installer platform selection, replacement, spaces, and checksum rejection: passed\n'
