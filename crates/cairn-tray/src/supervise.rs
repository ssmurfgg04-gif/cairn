//! Daemon supervision (round 26): the tray keeps the daemon alive.
//!
//! Before this, the tray was a *watcher*: `status --json` said "daemon not
//! running" and the tooltip told the user to go open a terminal. That broke
//! the product's one promise — install it and it just works. The installer
//! starts the daemon once (install.ps1 step 6), the tray owns it after
//! that: probe → spawn → probe, forever, with backoff.
//!
//! Design rules (extends the ADR-0016 boundary, doesn't bend it):
//! * still a THIN layer: supervision spawns `cairn.exe daemon` as a
//!   subprocess — no engine linkage, no store access, no gRPC.
//! * the child OUTLIVES the tray: we drop the `Child` handle right after
//!   spawn (Windows does not kill orphans), so quitting the tray never
//!   takes sync down — the original "tray crash can never take sync down"
//!   guarantee now reads "tray *exit* can never take sync down" either.
//! * never fight a real daemon: we only spawn after a probe says the
//!   daemon is DOWN. If the user runs `cairn daemon` in a terminal, the
//!   probe sees it and we stay quiet. If both race, the port bind is
//!   exclusive: one exits, the next probe agrees on the survivor.
//! * crash-loop protection: exponential backoff (8s boot grace → 15s →
//!   30s → 60s → 120s → 5 min cap). Success (probe sees the daemon up)
//!   resets the ladder. A daemon that dies instantly produces a handful
//!   of attempts in the first minutes, then one retry every 5 minutes —
//!   never a fork bomb, never silent.
//! * the child's stderr is appended to `<home>/daemon.log` so "why did my
//!   daemon die" is answerable without a terminal (`Status Details`
//!   material). Matches the CLI's home resolution (CAIRN_HOME or
//!   ~/.cairn), computed here only for a log path — not store access.
//!
//! Pure state machine, no Win32, no I/O: every platform compiles and unit
//! tests it (the windows-only tray wires it into the 3s poll worker).

use std::time::{Duration, Instant};

/// What the supervisor decided on this poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Daemon is down and the backoff elapsed: spawn `cairn daemon` now.
    Spawn,
    /// Daemon is up, or the backoff window has not elapsed: do nothing.
    Wait,
}

/// Backoff ladder, indexed by consecutive spawn attempts that never saw
/// the daemon come up. The FIRST entry (8s) doubles as boot grace: a
/// freshly spawned daemon gets two full poll cycles before a re-spawn is
/// even considered, so a slow cold start is not judged as a crash.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];
/// Everything beyond the ladder: retry at a calm, constant cadence.
const BACKOFF_CAP: Duration = Duration::from_secs(300);

#[derive(Debug, Default)]
pub struct Supervision {
    /// Consecutive spawn attempts without ever seeing the daemon up.
    attempts: u32,
    /// When the last spawn attempt happened (`None` = never spawned).
    last_attempt: Option<Instant>,
}

impl Supervision {
    pub fn new() -> Self {
        Supervision {
            attempts: 0,
            last_attempt: None,
        }
    }

    /// Feed one poll result; get the decision. `now` is injected so the
    /// tests drive the clock instead of sleeping.
    pub fn observe(&mut self, daemon_up: bool, now: Instant) -> Action {
        if daemon_up {
            // the daemon is alive — we may or may not have started it, but
            // whatever we did worked. Reset the crash-loop ladder.
            self.attempts = 0;
            self.last_attempt = None;
            return Action::Wait;
        }
        let elapsed = self
            .last_attempt
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(Duration::MAX);
        let backoff = backoff_for(self.attempts);
        if elapsed >= backoff {
            self.attempts += 1;
            self.last_attempt = Some(now);
            Action::Spawn
        } else {
            Action::Wait
        }
    }

    /// Consecutive spawn attempts without ever seeing the daemon up.
    /// Public since round 27: the tray's spawn path reads it to decide
    /// whether a stale `cairn.exe` should be swept before the next
    /// spawn (crash-loop path only — attempt 1 spawns clean; the
    /// daemon-side self-dedup resolves the startup race).
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

fn backoff_for(attempts: u32) -> Duration {
    match attempts {
        0 => Duration::ZERO, // never spawned: the first spawn is immediate
        n => BACKOFF
            .get((n - 1) as usize)
            .copied()
            .unwrap_or(BACKOFF_CAP),
    }
}

/// Resolve the daemon's stderr log path the way the CLI resolves its home
/// (CAIRN_HOME override, else `~/.cairn`) — the tray needs it ONLY to
/// point the child's stderr somewhere answerable. Creating the directory
/// is the daemon's job (and `init`'s); if it is missing we simply fall
/// back to no redirection rather than guessing further.
/// `cfg(windows)`-only callers (the tray's `spawn_daemon`); on other
/// targets the function would be dead weight — the warnings stay clean.
#[cfg(windows)]
pub fn daemon_log_path() -> Option<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("CAIRN_HOME") {
        return Some(std::path::PathBuf::from(home).join("daemon.log"));
    }
    dirs_home().map(|h| h.join(".cairn").join("daemon.log"))
}

/// `dirs::home_dir()` without the dependency: USERPROFILE (Windows) or
/// HOME (everywhere else) — exactly what the CLI's `default_home` falls
/// back to. Good enough for a log path; not store logic.
#[cfg(windows)]
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minutes(n: u64) -> Instant {
        Instant::now() + Duration::from_secs(n * 60)
    }

    #[test]
    fn first_down_poll_spawns_immediately() {
        let mut sv = Supervision::new();
        // the tray just started at login, daemon not up yet
        assert_eq!(sv.observe(false, minutes(0)), Action::Spawn);
        assert_eq!(sv.attempts(), 1);
    }

    #[test]
    fn up_poll_resets_the_ladder() {
        let mut sv = Supervision::new();
        sv.observe(false, minutes(0));
        // daemon came up (maybe we started it, maybe the user did)
        assert_eq!(sv.observe(true, minutes(1)), Action::Wait);
        // and went down again much later: immediate re-spawn, no memory of
        // the old failures
        assert_eq!(sv.observe(false, minutes(500)), Action::Spawn);
        assert_eq!(sv.attempts(), 1);
    }

    #[test]
    fn boot_grace_is_two_polls_not_a_crash() {
        let mut sv = Supervision::new();
        sv.observe(false, minutes(0)); // spawn
                                       // 3s later (next poll): the daemon is still binding — WAIT
        assert_eq!(
            sv.observe(false, minutes(0) + Duration::from_secs(3)),
            Action::Wait
        );
        // 8s after the attempt: backoff elapsed, re-spawn is allowed
        assert_eq!(
            sv.observe(false, minutes(0) + Duration::from_secs(8)),
            Action::Spawn
        );
        assert_eq!(sv.attempts(), 2);
    }

    #[test]
    fn crash_loop_backs_off_and_caps() {
        let mut sv = Supervision::new();
        let mut t = minutes(0);
        sv.observe(false, t); // attempt 1
        let waits = [8, 15, 30, 60, 120, 300];
        for (i, w) in waits.iter().enumerate() {
            // one second before the backoff elapses: WAIT
            assert_eq!(
                sv.observe(false, t + Duration::from_secs(w - 1)),
                Action::Wait,
                "attempt {}, one second early",
                i + 1
            );
            // at the backoff: SPAWN
            t += Duration::from_secs(*w);
            assert_eq!(sv.observe(false, t), Action::Spawn, "attempt {}", i + 2);
        }
        // and it stays capped at 5 minutes forever
        t += Duration::from_secs(300);
        assert_eq!(sv.observe(false, t), Action::Spawn);
        assert_eq!(
            sv.observe(false, t + Duration::from_secs(299)),
            Action::Wait
        );
        assert_eq!(sv.attempts(), 8);
    }

    #[test]
    fn never_fights_a_live_daemon() {
        // the user runs `cairn daemon` in a terminal: every poll sees UP,
        // no spawn is ever issued no matter the history
        let mut sv = Supervision::new();
        sv.observe(false, minutes(0)); // we spawned at login
        for m in 1..20 {
            assert_eq!(sv.observe(true, minutes(m)), Action::Wait);
        }
        assert_eq!(sv.attempts(), 0);
    }

    #[test]
    fn saturating_clock_never_panics() {
        // a time-travelling clock (NTP jump backwards) must not panic
        let mut sv = Supervision::new();
        sv.observe(false, minutes(10));
        assert_eq!(sv.observe(false, minutes(0)), Action::Wait);
    }
}
