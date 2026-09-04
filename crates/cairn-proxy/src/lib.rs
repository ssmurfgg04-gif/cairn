//! The proxy workflow (ADR-0020 §3): no professional editor scrubs 50 GB
//! ARRIRAW over a coffee-shop link. Cairn's answer leans on the engine
//! instead of fighting it:
//!
//! * A proxy is an **ordinary project file** — generated next to the media
//!   in `.cairn/proxy-cache/`, journaled, synced, and pinned like anything
//!   else. Remote machines pull megabytes of proxy because that is the
//!   only small file in the set; the 50 GB original stays a cold,
//!   on-demand recall. "Smart syncing" is the existing sparse/pin model —
//!   proxies simply give it something light to sync.
//! * Generation is **pluggable**: [`transcode::FfmpegTranscoder`] shells
//!   out to ffmpeg (1080p H.264, `-movflags +faststart` so the review
//!   player's HTTP-range scrubbing works), and [`transcode::CopyTranscoder`]
//!   proves the pipeline end-to-end where no ffmpeg exists (CI, tests).
//! * The **index** (`.cairn/proxies.json`) maps a blake3 digest of the
//!   SOURCE media to its proxy: content-derived, so a re-render of the
//!   same media reuses the proxy, and any edit (new digest) marks the old
//!   proxy stale. Deterministic JSON — it syncs and merges like the
//!   review session file.
//! * The **review portal streams the proxy first** (cairn-review
//!   `ReviewVersion::stream_rel`); `?full=1` opts into the original.

// Pure Rust: transcoders are subprocess drivers (ffmpeg), never FFI.
#![forbid(unsafe_code)]

pub mod model;
pub mod pipeline;
pub mod transcode;

pub use model::{ProxyEntry, ProxyIndex, ProxyProfile, ProxyStatus};
pub use pipeline::{generate, proxy_rel_for, status_of};
pub use transcode::{CopyTranscoder, FfmpegTranscoder, Transcoder};
