# sonobus-server

A self-hostable SonoBus rendezvous server. One process runs both services:

| Service | Protocol | Default bind | Open in the firewall |
|---|---|---|---|
| legacy | AOO net protocol of SonoBus 1.7.x (`aoo::server`, `Limits::standalone()`) | `0.0.0.0:10998` | TCP **and** UDP 10998 |
| modern | `sonobus/rdv/1` name + password rendezvous (`sonobus_net::rdv`) on an iroh endpoint | `0.0.0.0:10999` | UDP 10999 |

Neither service relays audio: they only introduce peers, which then connect to each other directly.

```sh
cargo build --release -p sonobus-server
./target/release/sonobus-server --key-file /var/lib/sonobus-server/rdv.key
```

```
2026-11-18T14:20:15.264Z INFO created key file /var/lib/sonobus-server/rdv.key
2026-11-18T14:20:15.280Z INFO ready legacy=0.0.0.0:10998 rendezvous_id=4b48…76d3 rendezvous_udp=10999 address=sonobus2://rdv/4b48…76d3@203.0.113.5:10999
```

Legacy (1.7.x) users enter `your.host:10998` as the connection server. Modern clients take the
`address=` value.

## The rendezvous address

```
sonobus2://rdv/<endpoint id: 64 hex digits>@<host:port>[,<host:port>...][?relay=<url>]
```

The endpoint id is iroh's own text form of the server's public key (lowercase hex). The
`sonobus2://rdv/` prefix is optional when parsing, as is case. The codec is
`sonobus_net::RdvAddr` (`Display` / `FromStr`, next to the invite links): parsing never
resolves host names; `Net::join_by_name` / `Net::register_by_name` take an `RdvAddr` and resolve
on the network thread (on every reconnect), and `RdvAddr::resolve` does it for callers that want
a `PeerAddr` (blocking: not on a UI thread).

By default the printed address lists the endpoint's interface addresses. Behind 1:1 NAT (most
cloud VMs) or with a DNS name, set `--public-addr rdv.example.org:10999` so the printed string
is what users can reach.

The endpoint id derives from the secret key in `--key-file`. The file is created on first start
(`create_new`, mode 0600 on Unix, 64 hex digits; on Windows it only inherits the directory's
access rights) and reused afterwards: **back it up**; losing it changes
the address every user has. A corrupt file is an error, never silently replaced. The key is never
logged.

## Options

`sonobus-server --help` lists everything. Each option is also a config file line and an env var:

| Command line | Config file (`--config`) | Env |
|---|---|---|
| `--legacy-bind 0.0.0.0:10998` | `legacy-bind = 0.0.0.0:10998` | `SONOBUS_SERVER_LEGACY_BIND=0.0.0.0:10998` |
| `--legacy-only` | `legacy-only = true` | `SONOBUS_SERVER_LEGACY_ONLY=1` |

Precedence: command line > env > config file > defaults. Config file: `name = value` lines,
`#` comment lines, values may be quoted. The config file itself comes from `--config` or
`SONOBUS_SERVER_CONFIG`. Unknown names (also unknown `SONOBUS_SERVER_*` env vars) are errors.

| Option | Default | Why |
|---|---|---|
| `legacy-bind` | `0.0.0.0:10998` | The port of SonoBus' public server (what 1.7.x users know). IPv4: the aoo crate's IPv6 socket is not dual-stack everywhere. |
| `modern-bind` | `0.0.0.0:10999` | sonobus-net's default port. A SonoBus client on the same host then falls back to an ephemeral port (clients do that); the server never does (`RendezvousServer::start_strict`): its port is part of the address users paste. |
| `key-file` | `sonobus-rdv.key` (current dir) | Point it at a persistent volume. |
| `relay` | `none` | The server should be directly reachable. `n0` = n0's public relays, or a comma list of relay URLs, for a server without a public address. `self` (with `--features relay`) = the `--relay-http` relay of this process. |
| `public-addr` | (interface addresses) | See above. |
| `legacy-only` / `modern-only` | off | Run one service. |
| `stats-interval` | `5m` | Stats line period, `0` = only on SIGUSR1. |
| `legacy-*` limits | `aoo::server::Limits::standalone()` | 256 connections, 16 per IP, 512 groups, … (see `--help`). |
| `modern-*` limits | `sonobus_net::rdv::Limits::default()` | 4096 connections, 16 per IP, 256 per tag, … |

Durations: `250ms`, `10s`, `5m` or plain seconds. Rates: `burst/per_sec`.

## Operation

* Logs: one line per event on stderr, `<UTC time> <LEVEL> <message>`. Startup, `ready`
  (both services bound), periodic `stats`, `shutting down`, `stopped`. No peer addresses, names,
  passwords or keys are logged (the daemon never sees per-connection events).
* `stats` line, per service: legacy connections, logged-in users, groups; modern connections,
  sources (IPv4 address / IPv6 /64), tags, registrations; and for both the refusals and drops
  since start by reason (`*_refused=reason:count,...` or `none`):
  ```
  stats uptime=300s legacy_conns=3 legacy_users=2 legacy_groups=1 legacy_refused=login_timeout:1 modern_conns=4 modern_sources=3 modern_tags=2 modern_regs=4 modern_refused=per_ip_cap:2,idle_timeout:1
  ```
  Legacy reasons: `global_cap per_ip_cap login_timeout idle_timeout send_queue_full join_rate
  groups_cap groups_per_ip_cap groups_per_conn_cap bad_address name_too_long
  password_too_long`. Modern: `global_cap per_ip_cap handshake register_timeout register_rate
  tag_full id_mismatch protocol idle_timeout`.
* Signals: SIGINT/SIGTERM stop gracefully (all threads joined, exit 0); SIGUSR1 prints a stats
  line (without moving the periodic schedule). On Windows Ctrl-C stops gracefully and there is
  no stats signal.
* Exit codes: 0 after a graceful stop, 1 when a service cannot start (port taken, bad key
  file: the message names it), 2 for usage errors.
* Run it as an unprivileged user: all default ports are above 1024. Dropping privileges is not
  implemented.

### systemd

```ini
# /etc/systemd/system/sonobus-server.service
[Unit]
Description=SonoBus rendezvous server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/sonobus-server --key-file /var/lib/sonobus-server/rdv.key
# ExecStart=... --public-addr rdv.example.org:10999
DynamicUser=yes
StateDirectory=sonobus-server
Restart=on-failure
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
```

`systemctl kill -s USR1 sonobus-server` prints a stats line into the journal.

### Docker

Not built or run as part of this change. Build from the workspace root:

```dockerfile
# Dockerfile
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends libopus-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p sonobus-server && strip target/release/sonobus-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data sonobus && mkdir /data && chown sonobus /data
COPY --from=build /src/target/release/sonobus-server /usr/local/bin/
USER sonobus
WORKDIR /data
VOLUME /data
ENV SONOBUS_SERVER_KEY_FILE=/data/rdv.key
EXPOSE 10998/tcp 10998/udp 10999/udp
ENTRYPOINT ["sonobus-server"]
```

```sh
docker build -f Dockerfile -t sonobus-server .
docker run -d --name sonobus-server -v sonobus-data:/data \
  -p 10998:10998/tcp -p 10998:10998/udp -p 10999:10999/udp \
  -e SONOBUS_SERVER_PUBLIC_ADDR=rdv.example.org:10999 sonobus-server
```

libopus is needed to build (the `aoo` crate links it) but not at run time: the server uses no
codec and the linker drops it (`ldd` shows only libc, libm, libgcc_s). No secret is baked into
the image; the key file lives on the volume. Docker's userland proxy can hide client source
addresses from the per-IP limits; prefer `--network host` on a dedicated host.

## Relay

An iroh relay forwards (end-to-end encrypted) traffic between peers that cannot reach each other
directly. Measured on x86_64 Linux, release profile, stripped:

| Build | Binary | Crates (normal deps) |
|---|---|---|
| default | 13.4 MB | 321 |
| `--features relay` | 16.0 MB (+2.6 MB) | 400 (+79: clap, toml, rcgen, tokio-rustls-acme, tracing-subscriber, tokio-websockets, …; iroh-relay's `server` feature pulls them all) |

`--features relay` adds `--relay-http <ip:port>`: an iroh relay over **plain HTTP, for a LAN or
behind a TLS-terminating reverse proxy only (put TLS in front)**. It is off by default. What it
does not do: TLS / ACME certificates, QUIC address discovery (needs TLS), metrics, rate limits,
access control (anyone who can reach it can relay through it). On a LAN peers usually reach each
other directly anyway, so its main use is behind a reverse proxy (nginx/Caddy with websocket
upgrade) that holds the certificate. `--relay self` makes the rendezvous endpoint use it as
`http://<relay-http address>` (so `--relay-http` must name an address clients can reach, not
`0.0.0.0`); behind a reverse proxy pass `--relay <public https url>` instead.

For a relay on the internet, run n0's `iroh-relay` binary (same version as the iroh this
workspace links, 1.4.x) next to this server instead: it does TLS with ACME (Let's Encrypt) or
manual certificates, QUIC address discovery, rate limits and metrics. Clients then use
`RelayChoice::Custom(vec!["https://relay.example.org".into()])`.

## Not verified

Real internet exposure, IPv6 binds, the systemd unit and the Docker image (neither was run).
