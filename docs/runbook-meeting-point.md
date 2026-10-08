# Meeting-point runbook — self-hosting the Cairn server

A *meeting point* is the machine every studio box can reach: it runs the
**metadata plane** (`cairn server` — journal, leases, admission, upload/
download plumbing) and, optionally, the **swarm leg** (`cairn signal` — the
UDP rendezvous + relay for direct peer transport, ADR-0017). Media bytes at
rest live in your object store (or the meeting point's disk in dev); the
clients' projects live on the editors' machines. Nothing here is required
for the localhost beta in `docs/BETA.md` — this runbook is for the next
step: two studios in different places syncing through something that is not
someone's laptop.

Everything below is verified against the code it names. The deploy artifacts
live in `deploy/`:

| Artifact | What it is |
|---|---|
| `deploy/cairn-server.service` | hardened systemd unit (shape A wired; shape B commented) |
| `deploy/cairn-signal.service` | hardened systemd unit for the swarm leg |
| `deploy/Dockerfile` | multi-stage image (rust builder → bookworm-slim runtime) |
| `deploy/Dockerfile.dockerignore` | keeps `target/` out of the build context |
| `deploy/docker-compose.yml` | server + nginx (TLS termination) + named volumes |
| `deploy/nginx-cairn.conf` | TLS termination for the TCP (gRPC) leg only |

## 1. The two roles — and the protocol mix (read before touching a firewall)

**Metadata plane** (`cairn server`):
- `7443/tcp` — gRPC (tonic, HTTP/2). This is the journal + leases + auth +
  chunk upload/download plane. Proxied fine (it is ordinary h2).
- `7444/tcp` — the **dev** local-fs object endpoint. Its presigned URLs bake
  in `http://<objects_addr>/` by construction (`crates/cairn-server/src/
  run.rs` builds the base URL with `format!("http://…")`), so this backend
  is plaintext HTTP and dev-only. Production uses a real S3-compatible
  bucket via `CAIRN_S3_*` (HTTPS by the bucket vendor, ADR-0005) and 7444
  is simply not exposed at all.

**Swarm leg** (`cairn signal`, optional):
- `17780/udp` — signal directory; `17781/udp` — encrypted relay pass-through.
  Both are UDP (the library's QUIC relay leg, `cairn-p2p` relay.rs
  `spawn_with_quic` + cairn-quic, is also UDP when wired; `cairn signal`
  serves the UDP relay today).
- **Do NOT proxy the UDP legs.** The signal protocol authenticates and
  canonicalizes peers by the OBSERVED source address of each datagram
  (`crates/cairn-p2p/src/signal.rs` — unspecified advertised IPs are
  replaced with the IP the registration actually came from, which is the
  NAT-discovery mechanism). A proxy rewrites the source and poisons every
  candidate. Expose the UDP ports directly:

  ```sh
  ufw allow 17780/udp && ufw allow 17781/udp      # ubuntu ufw
  firewall-cmd --add-port=17780/udp --permanent   # firewalld (both ports)
  firewall-cmd --add-port=17781/udp --permanent && firewall-cmd --reload
  ```

**Client-facing contract** (shape A, proxy TLS): editors log in with

```sh
cairn login --server https://cairn.example.com --code enr-… --name box1
```

The `https` scheme means TLS; a publicly trusted chain (Let's Encrypt)
validates against the client's webpki roots with no `--ca` flag
(`crates/cairn-sync/src/plane_grpc.rs`). A self-signed dev pair works on a
LAN box — clients then pass `--ca <pem>`.

## 2. Sizing (honest)

- **CPU/RAM:** small. The metadata plane is a single-writer SQLite (WAL)
  plus async gRPC; 2 vCPU / 4 GB serves a small studio's journals,
  presigning, and streaming chunk relay. The waveform/bench-style loads do
  NOT run here (server-side peaks belong to the client review portal).
- **Disk:** only what you choose. With a real bucket (`CAIRN_S3_*`) the
  meeting point holds `meta.db` + keys + the cold tier you opt into —
  gigabytes, not terabytes. With the dev local-fs backend it holds every
  object: size it like a NAS and do not use it for anything but a test.
- **`meta.db` is the journal of record.** SQLite WAL is a local-disk
  format — never put the data dir on NFS. Back it up (§6).

## 3. TLS — the two supported shapes

**Shape A — proxy terminates TLS (the default the artifacts wire):**
`cairn-server.service` binds `127.0.0.1:7443` plaintext; nginx (or any
h2-capable proxy) owns the cert and serves 443. Renewals are pure proxy
ops; the client needs no `--ca` for a publicly trusted chain.

**Shape B — server terminates TLS:** switch `--grpc-addr 0.0.0.0:7443`,
add `--tls-cert/--tls-key` (run.rs enables TLS on the metadata plane when
both are set — `crates/cairn-server/src/run.rs:158`), open 7443 in the
firewall, and skip the proxy for the gRPC leg. Clients pass `--ca` when the
cert is not publicly trusted. Dev certs: `just tls-dev-cert`.

## 4. Admission — the honest state of enrollment

`enroll_code` (the RPC that mints join codes) requires an authenticated
**admin** device OR `--dev-insecure` bootstrap mode
(`crates/cairn-server/src/services.rs:499`). The honest part: **the only
minting CLI that exists today is the hidden `cairn dev-enroll-code`
command, which works against a `--dev-insecure` server** (it asks for
`sync`-scoped codes; it does not authenticate as an admin). The codes
themselves are single-use and expiring (`crates/cairn-server/src/auth.rs`
— `codes.remove(code); // single use`). Admin-scoped minting on a clean
server is tracked contract debt (CONTRACT-DEBT, second-review #4/#5).

Practical bootstrap until that lands:

1. Start the server briefly with `--dev-insecure` and `--grpc-addr
   127.0.0.1:7443` (loopback, shape A).
2. `cairn dev-enroll-code --server 127.0.0.1:7443` → one `enr-…` code per
   device, single-use; run it once per editor and hand each code to one
   person.
3. Editors `cairn login --server https://cairn.example.com --code …`.
4. Restart the server clean (no `--dev-insecure`). It now refuses code
   minting (`admin scope required`).
5. Until admin minting ships, new devices mean repeating steps 1–4 — and
   if the meeting point is public, keep the dev-insecure window short and
   loopback-only, or additionally gate 7443 at the firewall (allowlist /
   WireGuard). Do not run `--dev-insecure` on a publicly reachable port.

## 5. Deploy

### 5a. systemd (bare metal)

```sh
sudo useradd --system --home /var/lib/cairn --shell /usr/sbin/nologin cairn
sudo install -m 0755 cairn /usr/local/bin/cairn        # the built binary
sudo mkdir -p /etc/cairn
sudo cp deploy/cairn-server.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now cairn-server
```

The unit runs as `User=cairn` with `StateDirectory=cairn` (→
`/var/lib/cairn`: `meta.db` + WAL, `keys/`, `objects/`, `cold/`),
`Restart=on-failure`, and a strict sandbox (`ProtectSystem=strict`,
`NoNewPrivileges`, empty `CapabilityBoundingSet`, syscall allowlist; the
full set is in the unit). Everything durable the service needs is the state
dir — that is the only writable path it gets.

Optional swarm leg: `deploy/cairn-signal.service` the same way. It is
stateless by design (all state evaporates on restart; every client
re-registers within one keepalive — `PEER_TTL` is 10 s,
`crates/cairn-p2p/src/signal.rs:44`). Pin the team's join code with
`--join-code` (validated at start; a typo fails fast with the checksum
message) or let it print a fresh code per start and read it once:
`journalctl -u cairn-signal -n 50`.

### 5b. Docker / compose

```sh
docker compose -f deploy/docker-compose.yml up -d --build
```

Multi-stage build (the toolchain is pinned by `rust-toolchain.toml`; the
`--cfg tokio_unstable` flag rides in from `.cargo/config.toml` — do NOT set
env `RUSTFLAGS` in the image, it would *replace* the config flags and break
the tokio io-uring build). The compose stack is shape A: nginx terminates
TLS on 443 and proxies h2 to the server's 7443; state lives in the named
`cairn-state` volume (back it up); a TCP healthcheck probes the gRPC
listener (the server has no dedicated `/healthz` yet — the only HTTP
surface is the signed objects route, which a plain probe cannot
authenticate). Mount real certs at `./certs` (compose) as
`live/<your-domain>/fullchain.pem` + `privkey.pem`, and edit
`server_name`/domains in `deploy/nginx-cairn.conf`.

For shape B instead: publish `"7443:7443"`, add `--tls-cert/--tls-key` via
`command:`, and drop the nginx service for the gRPC leg.

## 6. Knobs the code actually reads (verified)

| Knob | Where | Effect |
|---|---|---|
| `--data-dir` | `cairn server` (cli `Server` variant) | state root: `meta.db`, `keys/`, `objects/`, `cold/` |
| `--grpc-addr` | same | metadata-plane bind (default `127.0.0.1:7443`) |
| `--objects-addr` | same | dev local-fs objects bind (default `127.0.0.1:7444`) |
| `--tls-cert` / `--tls-key` | same | TLS on 7443 when BOTH are set |
| `--dev-insecure` | same | admission without an admin token — never in production |
| `CAIRN_COLD_DIR` | `run.rs:125` | cold-tier dir; unset + S3 backend → tiering skips (never fake-cold) |
| `CAIRN_S3_ENDPOINT` / `_BUCKET` / `_REGION` / `_ACCESS_KEY_ID` / `_SECRET_ACCESS_KEY` | `storage.rs::from_env` | a COMPLETE set switches the object store to SigV4 S3 — no half-wired states |
| `CAIRN_S3_PATH_STYLE` | same | `1` = path-style addressing (MinIO-class); default virtual-host |

Keep secrets in a root-only `EnvironmentFile` (the unit's commented
`/etc/cairn/cairn.env`), never in the unit itself.

## 7. Backups + upgrades

- **Backup:** stop the server (or snapshot atomically) and copy
  `/var/lib/cairn` — `meta.db` (+ WAL) is the journal of record; `keys/`
  holds the object-signing key (without it the dev local-fs URLs break).
  With an S3 backend the bucket vendor holds the bytes; the journal is
  still yours.
- **Upgrade:** replace the binary, `systemctl restart cairn-server`.
  Migrations are idempotent DDL applied at every boot (db.rs); state
  format changes are journal-proto-versioned. The swarm leg has nothing to
  migrate — restart it freely.
- **Client impact:** a restarted meeting point costs clients one reconnect;
  the engine retries with backoff and resumes uploads at chunk granularity
  (persisted upload sessions).

## 8. Known gaps (so nobody presumes)

- No dedicated `/healthz` (the Dockerfile healthcheck is a TCP probe on the
  gRPC listener; tracked TODO).
- No admin-scoped code minting on a clean server (§4) — tracked debt.
- The dev local-fs objects endpoint speaks plaintext HTTP by construction —
  production uses a real bucket (§1).
- The QUIC relay leg is library-side today; `cairn signal` serves UDP only.
