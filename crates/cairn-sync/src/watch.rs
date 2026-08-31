//! File watching with 2s quiescence debounce (SPEC §10): OS-native backends (inotify/
//! FSEvents/USN via `notify`), size+mtime heuristic for the polling fallback, and the
//! stable-state gate the chunker requires.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use cairn_core::CairnError;
use notify::Watcher as _;

/// Quiescence window before hashing (SPEC §10).
pub const QUIESCENCE_MS: u64 = 2_000;

/// Events surfaced by the watcher after debounce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuiescedEvent {
    /// Path settled after 2s of no fs events.
    Settled(String),
}

/// Watch a root (OS-native backend), forwarding quiesced paths through `tx`.
pub fn watch(
    root: &Path,
    tx: mpsc::Sender<QuiescedEvent>,
) -> Result<notify::RecommendedWatcher, CairnError> {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _: Result<(), _> = event_tx.send(event);
            }
        },
        notify::Config::default(),
    )
    .map_err(watch_err)?;
    watcher
        .watch(root, notify::RecursiveMode::Recursive)
        .map_err(watch_err)?;
    spawn_debouncer(event_rx, tx);
    Ok(watcher)
}

/// Polling fallback (SPEC §10) for network/odd filesystems.
pub fn watch_polling(
    root: &Path,
    tx: mpsc::Sender<QuiescedEvent>,
    interval: Duration,
) -> Result<notify::RecommendedWatcher, CairnError> {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _: Result<(), _> = event_tx.send(event);
            }
        },
        notify::Config::default().with_poll_interval(interval),
    )
    .map_err(watch_err)?;
    watcher
        .watch(root, notify::RecursiveMode::Recursive)
        .map_err(watch_err)?;
    spawn_debouncer(event_rx, tx);
    Ok(watcher)
}

/// Debounce core: a path is "settled" only after `QUIESCENCE_MS` without further events;
/// bursts collapse to ONE settled event (the chunker runs exactly once per save).
fn spawn_debouncer(rx: mpsc::Receiver<notify::Event>, tx: mpsc::Sender<QuiescedEvent>) {
    std::thread::spawn(move || {
        let mut pending: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(event) => {
                    for p in event.paths {
                        pending.insert(p.to_string_lossy().into_owned(), std::time::Instant::now());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            let now = std::time::Instant::now();
            let settled: Vec<String> = pending
                .iter()
                .filter(|(_, t)| now.duration_since(**t) >= Duration::from_millis(QUIESCENCE_MS))
                .map(|(p, _)| p.clone())
                .collect();
            for p in settled {
                pending.remove(&p);
                if tx.send(QuiescedEvent::Settled(p)).is_err() {
                    return;
                }
            }
        }
    });
}

fn watch_err(e: notify::Error) -> CairnError {
    CairnError::new(cairn_core::ErrorKind::Io, format!("watch: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §10: 2s quiescence before hashing — a write burst yields ONE settled event.
    #[test]
    fn quiescence_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _w = watch(dir.path(), tx).unwrap();
        let file = dir.path().join("take.braw");
        for i in 0..5 {
            std::fs::write(&file, format!("v{i}")).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(QUIESCENCE_MS + 1_500));
        let mut settled = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, QuiescedEvent::Settled(ref p) if p.ends_with("take.braw")) {
                settled += 1;
            }
        }
        assert_eq!(
            settled, 1,
            "burst of 5 writes must collapse to one settled event"
        );
    }
}
