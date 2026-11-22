#!/bin/sh
# Installs or upgrades MC Server Center on a systemd Linux host.
# Usage (as root, from the extracted release folder):  sudo ./install.sh
# Idempotent: re-running upgrades the binaries and restarts only the manager; running
# Minecraft servers keep running (KillMode=process) and are re-attached.
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "Please run as root: sudo ./install.sh" >&2
    exit 1
fi
here=$(cd "$(dirname "$0")" && pwd)
for f in msmd msmctl msmd.service; do
    [ -f "$here/$f" ] || { echo "missing $f next to install.sh" >&2; exit 1; }
done

if ! id msm >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/msm --shell /usr/sbin/nologin msm
    echo "created system user 'msm'"
fi

install -m 0755 "$here/msmd" /usr/local/bin/msmd
install -m 0755 "$here/msmctl" /usr/local/bin/msmctl
install -m 0644 "$here/msmd.service" /etc/systemd/system/msmd.service
systemctl daemon-reload
systemctl enable msmd >/dev/null
systemctl restart msmd

echo
echo "Installed. Next steps:"
echo "  1. Allow remote control:   sudo -u msm msmctl --data-dir /var/lib/msm remote-on 47625"
echo "     and open TCP port 47625 in your firewall (for example: sudo ufw allow 47625/tcp)."
echo "  2. Create a pairing code:  sudo -u msm msmctl --data-dir /var/lib/msm pair-code \"My PC\""
echo "  3. In the desktop app: Managing -> Add a remote server, enter <this-host>:47625 and the code."
