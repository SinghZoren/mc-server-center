#!/bin/sh
# MC Server Center: one-command setup for a Linux VPS or home server.
#
#   curl -fsSL https://github.com/SinghZoren/mc-server-center/releases/latest/download/install-linux.sh | sudo sh
#
# Downloads the latest Linux release, checks it against the release's SHA256SUMS, installs the
# service (see install.sh), turns on remote control, and prints the address and a one-time
# pairing code to enter in the desktop app (Managing -> Add a VPS / remote server).
#
# Options (environment variables):
#   MCSC_PORT=47625     remote-control port
#   MCSC_VERSION=v0.1.1 install a specific release instead of the latest
#
# If ufw is active, the remote-control port is opened in it.
set -eu

REPO="SinghZoren/mc-server-center"
PORT="${MCSC_PORT:-47625}"
if [ -n "${MCSC_VERSION:-}" ]; then
    BASE="https://github.com/$REPO/releases/download/$MCSC_VERSION"
else
    BASE="https://github.com/$REPO/releases/latest/download"
fi
ASSET="MCServerCenter-linux-x86_64.tar.gz"

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { printf '\nError: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "run this as root, for example: curl -fsSL ... | sudo sh"
[ "$(uname -m)" = "x86_64" ] || fail "only x86_64 Linux is supported right now (this machine is $(uname -m))"
command -v systemctl >/dev/null 2>&1 || fail "systemd is required"
command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"
case "$PORT" in ''|*[!0-9]*) fail "MCSC_PORT must be a number" ;; esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

say "Downloading MC Server Center..."
curl -fsSL -o "$ASSET" "$BASE/$ASSET"
curl -fsSL -o SHA256SUMS "$BASE/SHA256SUMS"
grep " $ASSET\$" SHA256SUMS > check.txt || fail "the release has no checksum for $ASSET"
sha256sum -c check.txt >/dev/null || fail "the download is corrupted (checksum mismatch); try again"
echo "Checksum verified."

tar -xzf "$ASSET"
dir=$(find . -maxdepth 1 -type d -name 'MCServerCenter-*-linux-x86_64' | head -n 1)
[ -n "$dir" ] || fail "unexpected archive layout"

say "Installing..."
sh "$dir/install.sh" >/dev/null

ctl() { runuser -u msm -- /usr/local/bin/msmctl --data-dir /var/lib/msm "$@"; }

# Wait for the service to come up.
i=0
until ctl ping >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -lt 30 ] || fail "the service did not start; see: journalctl -u msmd"
    sleep 1
done

say "Turning on remote control (TCP port $PORT)..."
ctl remote-on "$PORT" >/dev/null

if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
    ufw allow "$PORT/tcp" >/dev/null && echo "Opened port $PORT in ufw."
fi

code=$(ctl pair-code "Desktop" | grep -Eo '[0-9A-Z]{5}-[0-9A-Z]{5}-[0-9A-Z]{5}-[0-9A-Z]{5}' | head -n 1)
[ -n "$code" ] || fail "could not create a pairing code"
ip=$(curl -fsS --max-time 5 https://api.ipify.org 2>/dev/null || true)

say "Done! In the MC Server Center app: Managing -> Add a VPS / remote server, then enter:"
printf '\n    Address:       %s:%s\n' "${ip:-<this server's IP>}" "$PORT"
printf '    Pairing code:  %s   (works once, for 10 minutes)\n\n' "$code"
echo "Also allow TCP $PORT (and each Minecraft server's port, e.g. 25565) in your VPS provider's"
echo "firewall / security group if it has one."
echo "New code later:  sudo -u msm msmctl --data-dir /var/lib/msm pair-code \"My PC\""
