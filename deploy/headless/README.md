# Headless relay package

This archive installs the DNS Lab **authoritative relay** on a headless
Linux host (VPS). It contains a self-contained binary — no Rust toolchain or
database files are needed on the target.

## Contents

| File                  | Purpose                                            |
|-----------------------|----------------------------------------------------|
| `dns_lab`             | Release binary built with `--no-default-features`  |
| `install.sh`          | Installs binaries, user, config, systemd unit      |
| `dns-lab-config`      | Interactive config editor (`/etc/dns_lab/dns_lab.toml`) |
| `dns_lab.toml.example`| Commented reference config                         |
| `dns_lab.service`     | Hardened systemd unit (CAP_NET_BIND_SERVICE)       |
| `README.md`           | This file                                          |

## Install

```sh
tar xzf dns_lab-headless-*.tar.gz
cd dns_lab-headless-*
sudo ./install.sh --zone t.example.com
```

No key is installed by default: the relay routes on opaque tags and forwards
ciphertext it cannot read. Pass `--shared-key KEY` only if this host also sends
or reads mail — then the key must be **identical** to the clients'.

Flags:

- `--zone ZONE` — delegated zone (default `t.example.com`)
- `--shared-key KEY` — give the relay a key too (default: none, blind relay)
- `--listen ADDR` — bind address (default `0.0.0.0:53`)
- `--no-start` — install without enabling/starting the service
- `--force-config` — overwrite an existing config (backs it up first)
- `PREFIX=/opt/dns_lab` — install binaries elsewhere (default `/usr/local`)

The installer:

1. installs `dns_lab` and `dns-lab-config` to `$PREFIX/bin`,
2. creates the system user/group `dns_lab`,
3. creates `/etc/dns_lab` and `/var/lib/dns_lab`,
4. writes `/etc/dns_lab/dns_lab.toml`,
5. installs and starts the `dns_lab.service` unit.

## Configure

```sh
sudo dns-lab-config          # interactive menu
sudo dns-lab-config --show   # print current config
dns-lab-config --gen-key     # print a fresh shared key
sudo systemctl restart dns_lab
```

## Verify

```sh
systemctl status dns_lab
journalctl -u dns_lab -f
dig @<this-host-ip> test.t.example.com SOA
```

Point an `NS` record for your zone at this host and allow DNS inbound
(`ufw allow 53/tcp`, `ufw allow 53/udp`). See the repository's
`deploy/README.md` for full delegation details.

## Uninstall

```sh
sudo systemctl disable --now dns_lab
sudo rm /etc/systemd/system/dns_lab.service
sudo rm -rf /etc/dns_lab /var/lib/dns_lab
sudo rm /usr/local/bin/dns_lab /usr/local/bin/dns-lab-config
sudo userdel dns_lab
sudo systemctl daemon-reload
```
