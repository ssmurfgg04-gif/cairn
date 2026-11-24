# Failure domains (review #100)

What can fail independently, what the user sees, and what brings it back.
Every row below is backed by code named in the last column; change the
code and this table in the same commit.

The one rule underneath all of them is invariant **I2**: every change Cairn
has acknowledged is already durable, so any process may be killed at any
moment without losing acknowledged work. Recovery is always "start again",
never "repair state".

| Domain | Fails how | User sees | Recovers by | Code |
|---|---|---|---|---|
| **Daemon process** (`cairn daemon`: ctl gRPC :17777 + console :17778) | crash, kill, OOM, port taken | console: start screen / "Cairn isn't starting"; tray: down state | tray supervisor respawns it with backoff 8s → 15s → 30s → 60s → 120s → 5 min cap, reset on success; the window app also spawns it if nothing answers; a duplicate start loses the exclusive port bind and exits | `cairn-tray/src/supervise.rs`, `cairn-app/src/main.rs` |
| **Tray** | crash or exit | no tray icon | nothing to recover for sync: the daemon is a detached child and outlives the tray; reopening the tray reattaches by probing | `supervise.rs` (Child handle dropped after spawn) |
| **Native window** | crash, closed | window gone | reopen; closing never stops syncing; a second launch focuses the existing window | `cairn-app/src/main.rs` |
| **Console page** | daemon restarted behind an open page (new per-launch token) | every call would 403 | gate answers `STALE_TOKEN`; the page reloads once (60s loop guard) | `dashboard/middleware.rs`, `app.js recoverStaleToken` |
| **One project's runtime** (sync loop, eviction sweep, swarm) | sweep error, swarm error, unreadable root | that project's status turns to attention; other projects unaffected | each loop logs and continues on its next tick; shutdown is per project (`shutdown.changed()`); flag changes restart swarms live | `cairn-cli/src/projects.rs` (`restart_swarms`, per-project loops) |
| **`members.json`** | missing / corrupt / unreadable | missing: default roles; corrupt: actions refused with a fix-it message (detach still allowed); unreadable placeholder: fail-open with a loud audit record | fix or re-attach the root; never a circular lockout | `daemon.rs rbac_guard` |
| **Team server** (unreachable / slow) | network, server down | "Cairn can't reach your team's server ... your work is safe" | outbox keeps local changes and retries; the console says "Back to normal" after >=10s of trouble clears | `cairn-sync` outbox, `app.js` (#59) |
| **Heavy media jobs** (ffmpeg compress, proxies) | slow, failing encodes | button busy; error toast with a support ref | at most two run at once (FIFO queue), the rest wait; a failed job fails alone | `service/mod.rs media_slot` (#48) |
| **Disk full** | local volume nearly full | Home warning + "Free up space" | on-demand and periodic eviction of unpinned, not-recently-used chunks (pinned never touched) | `cairn-store/src/eviction.rs`, `service/actions.rs storage_free` |
| **Review portal** | abusive traffic | 429 with Retry-After | rate limits (60/min link resolves, 30/min comments) reset each window | portal rate limiting (#66/#67) |

## Restart policy in one line per process

- **daemon**: supervised (tray, window app); safe to kill any time (I2).
- **tray / window**: user-restarted; their loss never stops sync.
- **project runtime**: never restarts the daemon; logs and retries on its own tick.
- **server**: deployed separately; clients queue and retry (outbox), never block local work.

## Where to look

Every failed console action carries a support ref (`x-cairn-request-id`)
that matches one line in `<home>/daemon.log` (the daemon logs at `warn`
by default). Settings > Send diagnostics bundles the log tail (secrets
redacted), the doctor report, media-job state and recent errors.
