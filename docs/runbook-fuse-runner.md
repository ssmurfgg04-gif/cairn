# Runbook — self-hosted FUSE runner (live mount verification)

The `fuse-mount-live.yml` workflow runs the ignored `live_mount` test plus a
`cairn-fuse` daemon smoke on a machine that actually has `/dev/fuse`. The
`fuse-linux` job on `ubuntu-latest` remains the compile + unit gate; this runner is
the runtime last mile. One machine, ~15 minutes of setup, and every FUSE commit is
verified through the real kernel boundary.

## 1. Host prep (one-time)

Any Linux box/VM with a writable `/dev/fuse` works (bare metal, VM, container with
the device passed through). Debian/Ubuntu package names shown; adapt for your distro.

```bash
# FUSE userspace + kernel module
sudo apt-get install -y fuse3
sudo modprobe fuse
ls -l /dev/fuse                     # must exist and be openable by the runner user
sudo usermod -aG fuse "$RUNNER_USER"   # group name varies: fuse on Debian

# toolchain (the workflow uses the preinstalled rustup)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source ~/.cargo/env
rustc --version && cargo --version && fusermount3 --version
```

## 2. Register the runner with the required labels

GitHub → repo → **Settings → Actions → Runners → New self-hosted runner** (pick
Linux x64). During `./config.sh` set the labels to exactly:

```
self-hosted,linux,fuse
```

(Or edit `labels` later in `.runner` / via the settings page — the workflow matches
`runs-on: [self-hosted, linux, fuse]` and will not start without ALL THREE.)

Run it as a service so it survives reboots:

```bash
sudo ./svc.sh install "$RUNNER_USER" && sudo ./svc.sh start
```

## 3. Flip the repo variable (arms the workflow)

GitHub → **Settings → Secrets and variables → Actions → Variables → New repository
variable**: `CAIRN_FUSE_LIVE` = `1`.

Until this variable exists, `fuse-mount-live` never starts — a registered runner
without the variable (or the reverse) means the job stays queued or skipped. Both
must be in place. To disarm (e.g. runner maintenance): set the variable to `0`.

## 4. Verify

1. **Actions → fuse-mount-live → Run workflow** (manual dispatch first — don't wait
   for the next FUSE-touching push).
2. Expected log highlights:
   - `Preflight` passes (`/dev/fuse`, `fusermount3`, rust versions)
   - `live_mount_roundtrip_through_kernel ... ok` — byte-identical 1.5MB multi-chunk
     roundtrip, virtual dirs, readdir, **Phase-2 domain EBUSY through the kernel**,
     CAS blob assertion, post-unmount store persistence
   - `daemon smoke OK` — the actual `cairn-fuse` CLI mounted, served a write+read,
     unmounted cleanly
3. A `Cleanup stale mounts` step runs `always()` so a dead run never leaves a stale
   mount to poison subsequent jobs.

## 5. Security (read before exposing the runner)

- **Self-hosted runners run arbitrary repo code on your hardware.** Keep the runner
  on a dedicated, disposable VM — never on a workstation or a machine with
  credentials reachable from the runner user.
- This workflow triggers on `push` to `main` (paths-filtered) and
  `workflow_dispatch` — never on `pull_request`. Do not add PR triggers: GitHub
  blocks fork PRs on self-hosted by default precisely because of this, and
  overriding that is the classic supply-chain footgun.
- The repo is currently public. If that ever changes or the fleet becomes
  interesting to attack, prefer an ephemeral runner (VM snapshot reset per job) or
  make the repo private.

## 6. Troubleshooting

| Symptom | Cause → fix |
|---|---|
| Job queued forever | Labels mismatch (needs all of `self-hosted,linux,fuse`) or `CAIRN_FUSE_LIVE` unset |
| `Preflight` fails on `/dev/fuse` | `sudo modprobe fuse`; in containers, pass `--device /dev/fuse --cap-add SYS_ADMIN` (or use `--privileged`) |
| `Permission denied` opening `/dev/fuse` | Runner user not in the `fuse` group (re-login) or udev rule restricts the device |
| Mount appears then ops hang | Nested-container environments without `user_namespace` — run on VM/bare metal, or add `MountOption::AutoUnmount`-style options via the bin |
| `fusermount3: option -u only allowed for the owner` | The mount was created by a different user (runner user must own both the mountpoint and the session) |
| Stale mount after a crashed run | The `always()` cleanup handles `/tmp/cairn-fuse-smoke`; for the test's tempdir mounts: `fusermount3 -u -z /tmp/<mp>` manually |
