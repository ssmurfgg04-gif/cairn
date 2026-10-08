//! In-memory fixed-window rate limiting for the guest portal (review
//! #66/#67, light version): no new dependencies, no persistence, no tuning
//! surface — a `Mutex<HashMap<key, window>>` and a per-bucket per-minute
//! budget.
//!
//! Buckets (see `http.rs` for where each applies):
//! * `resolve`  — 60/min per source IP, on every `/r/:token` route that
//!   validates the token (session / comment / resolve / presence /
//!   attachment / media). This is the brute-force budget: a guessed token
//!   costs one attempt, and a hostile client cannot try faster than 1/s.
//! * `comment`  — 30/min per IP+token on the comment POST (an abusive
//!   reviewer cannot flood the note set even from one link).
//! * media rides the same `resolve` bucket: the browser issues one
//!   open-ended range request per play/seek (the media handler serves
//!   `bytes=start-`), so 60/min is far above legitimate playback traffic.
//! * waveform is deliberately NOT here — it already has its own bounded
//!   admission control (lane semaphore → 429, CONTRACT-DEBT #4).
//!
//! Identity is the TCP peer address (`ConnectInfo<SocketAddr>`) — the
//! portal is a direct LAN/port-forward surface, no reverse proxy in front,
//! so there is no XFF to parse. Fixed window: bursts up to the budget are
//! allowed instantly, sustained traffic is capped at `limit` per 60 s.
//!
//! Keys may embed token strings (the comment bucket is per IP+token); the
//! map lives in memory ONLY and is never logged — tokens must not reach
//! logs (the portal's standing rule).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One fixed window = one minute (the budget vocabulary is "per minute").
const WINDOW: Duration = Duration::from_secs(60);

/// Hard cap on tracked windows. Each window is a handful of bytes; the cap
/// exists so a spoofed-source flood (many IPs behind one socket is not
/// possible, but many distinct IP+token pairs are) cannot grow the map
/// without bound. Pruning keeps only live windows, like the presence map.
const MAX_WINDOWS: usize = 8192;

#[derive(Default)]
pub struct Limiter {
    windows: Mutex<HashMap<String, Window>>,
}

struct Window {
    start: Instant,
    count: u32,
}

impl Limiter {
    /// Charge one request against `key` with budget `limit` per window.
    /// `Ok(())` = allowed; `Err(secs)` = over budget, retry after `secs`
    /// (the value for the `Retry-After` header, always >= 1).
    pub fn check(&self, key: &str, limit: u32) -> Result<(), u64> {
        let mut guard = self.windows.lock().expect("rate limiter lock");
        if guard.len() >= MAX_WINDOWS {
            // at cap: drop expired windows first, then evict the OLDEST
            // live window. A hostile identity flood must not grow the map
            // without bound; an evicted window simply refills — the same
            // bounded-memory trade the presence map makes.
            guard.retain(|_, w| w.start.elapsed() < WINDOW);
            while guard.len() >= MAX_WINDOWS {
                let oldest = guard
                    .iter()
                    .min_by_key(|(_, w)| w.start)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => {
                        guard.remove(&k);
                    }
                    None => break,
                }
            }
        }
        let now = Instant::now();
        let w = guard.entry(key.to_string()).or_insert(Window {
            start: now,
            count: 0,
        });
        if now.duration_since(w.start) >= WINDOW {
            // new window: budget resets
            w.start = now;
            w.count = 0;
        }
        if w.count >= limit {
            let left = WINDOW.saturating_sub(now.duration_since(w.start));
            return Err(left.as_secs().max(1));
        }
        w.count += 1;
        Ok(())
    }

    /// Requests already charged against `key` in the current window (tests).
    #[cfg(test)]
    pub fn charged(&self, key: &str) -> u32 {
        self.windows
            .lock()
            .expect("rate limiter lock")
            .get(key)
            .map(|w| w.count)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> String {
        format!("resolve|{name}")
    }

    /// Burst up to the budget is allowed immediately; the next request is
    /// refused with a sane Retry-After.
    #[test]
    fn burst_up_to_limit_allowed_then_blocked() {
        let l = Limiter::default();
        for i in 0..60 {
            assert!(l.check(&key("a"), 60).is_ok(), "request {i} within budget");
        }
        assert_eq!(l.charged(&key("a")), 60);
        let retry = l.check(&key("a"), 60).expect_err("61st must be blocked");
        assert!(
            (1..=60).contains(&retry),
            "Retry-After must be 1..window secs, got {retry}"
        );
        // an independent identity has its own budget
        assert!(l.check(&key("b"), 60).is_ok());
    }

    /// Sustained traffic is capped: after the window rolls, the budget is
    /// fresh (fixed-window semantics — a new minute is a new budget).
    #[test]
    fn window_roll_resets_the_budget() {
        let l = Limiter::default();
        for _ in 0..5 {
            assert!(l.check(&key("roll"), 5).is_ok());
        }
        assert!(l.check(&key("roll"), 5).is_err(), "6th in-window blocked");
        // force the window to look expired (no sleeping: backdate the start)
        {
            let mut guard = l.windows.lock().unwrap();
            let w = guard.get_mut(&key("roll")).unwrap();
            w.start = Instant::now() - WINDOW - Duration::from_millis(1);
        }
        assert!(l.check(&key("roll"), 5).is_ok(), "fresh window charges");
        assert_eq!(l.charged(&key("roll")), 1, "count restarted at 1");
    }

    /// The Retry-After value counts down the remaining window (never 0).
    #[test]
    fn retry_after_reflects_remaining_window() {
        let l = Limiter::default();
        for _ in 0..2 {
            l.check(&key("ra"), 2).unwrap();
        }
        // exhaust, then backdate to force a near-end-of-window refusal
        let retry = l.check(&key("ra"), 2).expect_err("blocked");
        assert!(retry >= 1);
        {
            let mut guard = l.windows.lock().unwrap();
            let w = guard.get_mut(&key("ra")).unwrap();
            w.start = Instant::now() - Duration::from_secs(59);
        }
        let retry = l.check(&key("ra"), 2).expect_err("still blocked");
        assert_eq!(
            retry, 1,
            "one second left in the window -> Retry-After: 1"
        );
    }

    /// Pruning: floods of distinct identities cannot grow the map without
    /// bound — beyond the cap, expired windows are dropped first and the
    /// oldest live window is evicted to make room (bounded by construction).
    #[test]
    fn map_is_bounded_and_prunes_expired_windows() {
        let l = Limiter::default();
        for i in 0..(MAX_WINDOWS + 100) {
            let _ = l.check(&format!("k{i}"), 1);
        }
        let len_after_flood = l.windows.lock().unwrap().len();
        assert!(
            len_after_flood <= MAX_WINDOWS,
            "flood must not exceed the cap, got {len_after_flood}"
        );
        // age everything, then one more check prunes them all
        {
            let mut guard = l.windows.lock().unwrap();
            for w in guard.values_mut() {
                w.start = Instant::now() - WINDOW - Duration::from_millis(1);
            }
        }
        let _ = l.check("fresh", 1);
        assert!(l.windows.lock().unwrap().len() <= 1, "expired windows pruned");
    }
}
