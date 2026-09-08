# Runbook: OS network tuning for high-throughput sync

Cairn's engine (FastCDC + parallel outbox + QUIC) can saturate a link the
OS won't give it. Defaults are tuned for web browsing, not 50 GB
timelines. This page is the checklist; every setting is reversible.

## Linux (both studios + VPS)

```sh
# /etc/sysctl.d/99-cairn.conf — BDP-sized buffers for Long Fat Networks
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 87380 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216
net.ipv4.tcp_window_scaling = 1
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_no_metrics_save = 1
net.ipv4.tcp_moderate_rcvbuf = 1

sudo sysctl -p /etc/sysctl.d/99-cairn.conf
sysctl net.ipv4.tcp_rmem net.ipv4.tcp_wmem   # verify
```

Why: the default 128 KiB–6 MiB autotune ceiling caps a 100 ms RTT link at
~10–50 MB/s no matter how fast Cairn chunks. 16 MiB covers ~1 Gbps at
120 ms RTT.

## Windows (studio machines)

Run PowerShell **as Administrator**:

```powershell
netsh int tcp set global autotuninglevel=normal
netsh int tcp set global rss=enabled
netsh int tcp set global ecncapability=enabled
```

Disable Remote Differential Compression (RDC). RDC hooks file writes to
compute its own rolling deltas and fights FastCDC for the same CPU;
on media trees it slows ingest and never helps (chunks are already
content-defined):

```powershell
Disable-WindowsOptionalFeature -Online -FeatureName RDC
# or per-run, no reboot:
cairn daemon --disable-rdc
```

Verify: `Get-WindowsOptionalFeature -Online -FeatureName RDC | Select State`
should read `Disabled`.

## macOS

```sh
sudo sysctl -w net.inet.tcp.recvspace=1048576
sudo sysctl -w net.inet.tcp.sendspace=1048576
sudo sysctl -w kern.maxsockbuf=16777216
```

## Compression: when to turn it off

`packing_enabled` + `compression_enabled` (zstd) are ON by default and
correct for project files, timelines, and audio. For **already-compressed
camera media** (BRAW, ProRes, H.264/5) compression burns CPU for ~0%
gain — flip it per job, not globally:

Flip `compression_enabled` off in dashboard Settings (or the `set_flag`
ctl) for the bulk camera-media pass, back on after — flags apply on the
next job run, no restart. Everything else keeps the flags on.

Rule of thumb: text-like bytes (OTIO, EDL, WAV) compress; camera bytes
don't. The proxy ladder (`cairn proxy generate`) already emits
right-sized H.264/H.265 — never re-compress proxies.

## QUIC vs TCP note

Swarm chunk transport rides QUIC (`cairn-quic`): no head-of-line
blocking, connection migration across Wi-Fi/Ethernet, 25 s keepalives
for NAT mappings. TCP tuning above still matters for the storage
server path (gRPC/HTTP/2) and TURN relay fallback. If QUIC UDP is
blocked (strict enterprise firewall), the relay over TLS/443 is the
automatic fallback — slower, but it connects.

## Verify the gain

```sh
cairn doctor                 # HEALTHY + buffer report where detectable
# then re-run the 50 MB LAN check from the beta runbook:
# before tuning record MB/s, after tuning compare — same files, same box
```
