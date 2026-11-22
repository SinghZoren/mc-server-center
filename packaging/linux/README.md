# Running MC Server Center on Linux (headless)

Supported: x86_64 Linux with systemd (tested target: Ubuntu 24.04 LTS). The Windows desktop app
manages the Linux host securely over the network.

## Install or upgrade

1. Download the Linux release and check it: `sha256sum -c SHA256SUMS`.
2. Extract it and run `sudo ./install.sh`.

This installs `msmd` (the service) and `msmctl` (the command-line tool) in `/usr/local/bin`. It
also creates a `msm` system user that owns `/var/lib/msm`, and enables the `msmd` systemd service.

Upgrades use the same steps. Only the manager restarts; running Minecraft servers keep running and
are re-attached.

## Connect the desktop app

```sh
sudo -u msm msmctl --data-dir /var/lib/msm remote-on 47625       # listen on TCP 47625
sudo ufw allow 47625/tcp                                         # if you use ufw
sudo -u msm msmctl --data-dir /var/lib/msm pair-code "My PC"     # one-time code, 10 minutes
```

In the desktop app, choose **Managing → Add a VPS / remote server**, enter `your-host:47625` and the
code.

- The connection is TLS 1.3. Both sides pin each other's certificate.
- Codes work once.
- Paired devices can do everything except administration (pairing and revoking).

## Manage devices

```sh
sudo -u msm msmctl --data-dir /var/lib/msm devices
sudo -u msm msmctl --data-dir /var/lib/msm revoke <device-id>    # disconnects it immediately
sudo -u msm msmctl --data-dir /var/lib/msm remote-off
```

## Players' firewall

Open each Minecraft server's port in your firewall too (for example
`sudo ufw allow 25565/tcp`).

## Notes and limitations

- Commands on the host must run as the `msm` user. The local socket only accepts that user.
- Secrets (the host's TLS key and DNS tokens) are stored in `/var/lib/msm/secrets`, readable only
  by `msm`. They are not in an OS keychain, because headless servers usually have none.
- `systemctl stop msmd` stops the manager, not your servers. To stop every server, run
  `sudo -u msm msmctl --data-dir /var/lib/msm` with a stop command per server, or use the
  desktop app's tray "Stop all servers" while managing this host.
