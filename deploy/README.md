# Deploying the DNS tunnel relay on a VPS

This sets up the headless authoritative relay so desktop clients can tunnel through
**real public DNS** using a domain you control.

## 1. DNS delegation (at your registrar / DNS provider)

You need the relay's nameserver to be authoritative for a dedicated subdomain.
Example: tunnel on `t.example.com`, relay host `ns1.example.com` at `203.0.113.10`.

| Type  | Name               | Value                 |
|-------|--------------------|-----------------------|
| NS    | `t.example.com`    | `ns1.example.com`     |
| A     | `ns1.example.com`  | `203.0.113.10`        |
| AAAA  | `ns1.example.com`  | `2001:db8::10` (opt.) |

Some providers require the nameserver name to live inside the same zone, e.g.
`ns1.t.example.com`. In that case delegate `t.example.com NS ns1.t.example.com`
and add the glue `ns1.t.example.com A <VPS_IP>`.

Wait for delegation to propagate, then sanity-check:

```
dig +short NS t.example.com
dig @ns1.example.com test.t.example.com SOA
```

## 2. Build on the VPS

The GUI is a default feature and pulls X11/Wayland deps, so build the headless
binary without it:

```
cargo build --release --no-default-features
sudo install -m 0755 target/release/dns_lab /usr/local/bin/dns_lab
```

Alternatively, build a portable archive with an installer on the build host:

```
./deploy/package-headless.sh
# -> dist/dns_lab-headless-<version>-<arch>.tar.gz
```

Copy the tarball to the VPS, unpack it, and run
`sudo ./install.sh --zone t.example.com --shared-key '...'`. See
`deploy/headless/README.md` for details (`dns-lab-config` edits the config).

## 3. Relay config

`/etc/dns_lab/dns_lab.toml`:

```toml
display_name  = "relay"
listen_addr   = "0.0.0.0:53"
zone          = "t.example.com"
authoritative = true
shared_key    = "an-ultra-secure-secret-key-32a!!"
db_path       = "/var/lib/dns_lab/dns_lab.db"
```

Binding :53 requires privilege. Either run as root or grant
`CAP_NET_BIND_SERVICE` (see the systemd unit).

## 4. Firewall

Allow DNS inbound on both transports:

```
sudo ufw allow 53/tcp
sudo ufw allow 53/udp
```

## 5. systemd

```
sudo mkdir -p /etc/dns_lab /var/lib/dns_lab
sudo cp deploy/dns_lab.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now dns_lab
journalctl -u dns_lab -f
```

## 6. Client config (desktop)

Point the client at a recursive resolver; it will forward queries under your
delegated zone to the relay. Polling for inbound mail is enabled automatically in
`resolver` mode.

```toml
display_name     = "alice"
listen_addr      = "127.0.0.1:1054"
zone             = "t.example.com"
egress           = "resolver"
resolver         = "1.1.1.1:53"
poll_interval_secs = 2
shared_key       = "an-ultra-secure-secret-key-32a!!"
db_path          = "alice.db"
```

Start the app with `dns_lab` (GUI) or headless sending with:

```
dns_lab send --to <recipient-node-id> --body "hello through public DNS"
dns_lab id        # prints your node id to share with peers
```

## 7. Verify before running the app

```
# Upstream chunk (relay should return a TXT ACK)
dig @1.1.1.1 m.deadbeef.0.1.jbswy3dp.t.example.com TXT

# Mailbox poll (relay returns queued messages / cursor)
dig @1.1.1.1 p.<your-node-id>.0.0.t.example.com TXT
```

## Operational notes

- **Timeouts/truncation:** clients advertise EDNS0 and automatically retry over TCP
  when a reply sets the TC bit.
- **Caching:** every query name is unique and answers use TTL 0, so recursive
  caching never collides.
- **Case:** public mode uses Base32 (case-insensitive) because resolvers may apply
  0x20 case randomization.
- **Rate limits / policy:** public resolvers may rate-limit or flag tunnel traffic;
  some networks block or poison DNS. Behaviour depends on the operator.
- **Metadata:** the payload is encrypted, but query timing, sizes and counts are
  observable on the wire.
- **Shared key:** the default key is lab-grade. Use a strong `shared_key` and keep
  it identical on all nodes.
