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

/// Watch a root (OS-native backend) with an observable metrics handle.
/// Identical to [`watch`] except the caller owns the [`WatchMetrics`].
pub fn watch_with_metrics(
    root: &Path,
    tx: mpsc::Sender<QuiescedEvent>,
    metrics: std::sync::Arc<std::sync::Mutex<WatchMetrics>>,
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
    spawn_debouncer_inner(event_rx, tx, metrics);
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
    spawn_debouncer_inner(rx, tx, WatchMetrics::shared());
}

/// Depth/throughput gauges for the debouncer (P1 measurement, perf triage
/// #7). The daemon logs `max_pending_depth` the same way it watches the
/// 512-slot watch mailbox: growth here means a copy burst is holding paths
/// in quiescence, not that events are lost.
///
/// No eviction cap by design: dropping a pending path would silently skip a
/// save. The heap keeps the per-tick cost at O(due) instead of the old
/// O(pending) full-map scan every 250 ms.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchMetrics {
    /// Largest `pending` size observed (gauge high-water mark).
    pub max_pending_depth: usize,
    /// Total settled events forwarded.
    pub settled_total: u64,
}

impl WatchMetrics {
    fn shared() -> std::sync::Arc<std::sync::Mutex<WatchMetrics>> {
        std::sync::Arc::new(std::sync::Mutex::new(WatchMetrics::default()))
    }
}

/// Earliest-deadline-first entry. `BinaryHeap` is a max-heap, so the
/// ordering is reversed to pop the smallest deadline first. `seq` retires
/// superseded entries: a re-evented path bumps its seq and only the newest
/// (deadline, seq) pair can settle it.
struct HeapEntry {
    deadline: std::time::Instant,
    seq: u64,
    path: String,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // reversed: earliest deadline pops first
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

fn spawn_debouncer_inner(
    rx: mpsc::Receiver<notify::Event>,
    tx: mpsc::Sender<QuiescedEvent>,
    metrics: std::sync::Arc<std::sync::Mutex<WatchMetrics>>,
) {
    std::thread::spawn(move || {
        let mut pending: std::collections::HashMap<String, (std::time::Instant, u64)> =
            std::collections::HashMap::new();
        let mut heap: std::collections::BinaryHeap<HeapEntry> = std::collections::BinaryHeap::new();
        let mut seq: u64 = 0;
        loop {
            // sleep until the earliest deadline (new events still wake us);
            // idle cadence stays 250 ms exactly like before
            let timeout = heap
                .peek()
                .map(|e| {
                    e.deadline
                        .saturating_duration_since(std::time::Instant::now())
                })
                .unwrap_or(Duration::from_millis(250));
            match rx.recv_timeout(timeout) {
                Ok(event) => {
                    let now = std::time::Instant::now();
                    for p in event.paths {
                        seq += 1;
                        let deadline = now + Duration::from_millis(QUIESCENCE_MS);
                        pending.insert(p.to_string_lossy().into_owned(), (deadline, seq));
                        heap.push(HeapEntry {
                            deadline,
                            seq,
                            path: p.to_string_lossy().into_owned(),
                        });
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            let now = std::time::Instant::now();
            let mut settled_now: u64 = 0;
            while let Some(top) = heap.peek() {
                if top.deadline > now {
                    break;
                }
                let entry = heap.pop().expect("peeked");
                match pending.get(&entry.path) {
                    // current generation, deadline reached → settle exactly once
                    Some((deadline, s)) if *s == entry.seq && *deadline <= now => {
                        pending.remove(&entry.path);
                        settled_now += 1;
                        if tx.send(QuiescedEvent::Settled(entry.path)).is_err() {
                            return;
                        }
                    }
                    // superseded or re-armed entry →ignore
                    _ => {}
                }
            }
            if settled_now > 0 || !pending.is_empty() {
                if let Ok(mut m) = metrics.lock() {
                    m.settled_total += settled_now;
                    m.max_pending_depth = m.max_pending_depth.max(pending.len());
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

    /// Heap debouncer: N distinct files written in a burst all settle exactly
    /// once each, and the metrics gauge observed the burst depth.
    #[test]
    fn burst_of_many_files_all_settle_once() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let metrics = std::sync::Arc::new(std::sync::Mutex::new(WatchMetrics::default()));
        let _w = watch_with_metrics(dir.path(), tx, std::sync::Arc::clone(&metrics)).unwrap();
        const N: usize = 20;
        for i in 0..N {
            std::fs::write(dir.path().join(format!("clip{i:02}.braw")), format!("v{i}")).unwrap();
        }
        std::thread::sleep(Duration::from_millis(QUIESCENCE_MS + 2_500));
        let mut settled = std::collections::HashSet::new();
        while let Ok(ev) = rx.try_recv() {
            let QuiescedEvent::Settled(p) = ev;
            settled.insert(p);
        }
        assert_eq!(settled.len(), N, "every file must settle exactly once");
        let m = metrics.lock().unwrap();
        assert_eq!(m.settled_total, N as u64);
        assert!(
            m.max_pending_depth >= 2,
            "burst must register depth, got {}",
            m.max_pending_depth
        );
    }

    /// Min-heap order: earliest deadline pops first even when pushed last.
    #[test]
    fn heap_pops_earliest_deadline_first() {
        use std::collections::BinaryHeap;
        let now = std::time::Instant::now();
        let mut heap = BinaryHeap::new();
        heap.push(HeapEntry {
            deadline: now + Duration::from_millis(2000),
            seq: 1,
            path: "late".into(),
        });
        heap.push(HeapEntry {
            deadline: now + Duration::from_millis(10),
            seq: 2,
            path: "early".into(),
        });
        assert_eq!(heap.pop().unwrap().path, "early");
        assert_eq!(heap.pop().unwrap().path, "late");
    }
}
