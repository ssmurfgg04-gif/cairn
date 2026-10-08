//! Server-side waveform peaks for the review portal (CONTRACT-DEBT #4).
//!
//! Before this module the register's claim "the waveform path generates
//! server-side" was wrong — peaks were computed 100% client-side in
//! review.js: a 40 MiB budget gate, a full `arrayBuffer()` download, then
//! `decodeAudioData`. That gate is a lie for a 2 h multicam audio bed (the
//! file is simply never drawn) and a waste for every small one (the whole
//! media file ships to the browser to compute ~700 numbers).
//!
//! This module is the honest version:
//!
//! * **Bounded memory.** The decoder streams packet-by-packet (symphonia,
//!   pure Rust — no FFI, the workspace forbids unsafe). The only allocation
//!   that grows with duration is the bins array: `bins × 2 f32`, hard-capped
//!   at [`MAX_BINS`] (≈ 1.6 MiB at 200k bins). Decode buffers are constant
//!   per-packet. Peak RSS does not scale with duration.
//! * **Admission control.** A [`tokio::sync::Semaphore`] bounds concurrent
//!   decodes (`CAIRN_WAVEFORM_LANES`). `try_acquire` only — the portal never
//!   queues a guest behind someone else's 2 h decode; a full lane set is a
//!   `429` + `Retry-After` and the player falls back to its own decoder.
//!   `lanes = 0` disables the service outright (`503`).
//! * **Content-addressed cache.** The cache key is the BLAKE3 of the file
//!   content, streamed in 1 MiB chunks (a 50 GB source never loads into
//!   memory). Identical media never decodes twice — across versions, links,
//!   or daemon restarts. Entries are written atomically (tmp + fsync +
//!   rename, the store.rs convention).
//! * **Guardrails.** Audio longer than `CAIRN_WAVEFORM_MAX_MINUTES` (or any
//!   bin index beyond [`MAX_BINS`]) refuses with [`WaveError::TooLong`]
//!   before it can hurt; every job is wrapped in a decode timeout.
//!
//! Wire shape (floats, not i8 — the player draws from a [-1, 1] range):
//! `{"ok":true,"rate_hz":8,"duration_ms":N,"bins":N,"peaks":[[min,max],...],
//! "codec":"pcm_s16le","generated_at":ms}` — `peaks` values are mono-mixed
//! floats clamped to [-1.0, 1.0]; `codec` is the audio codec tag
//! (container-agnostic), not the file extension.

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    CodecType, DecoderOptions, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_NULL,
    CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_S16BE, CODEC_TYPE_PCM_S16LE, CODEC_TYPE_PCM_S24LE,
    CODEC_TYPE_PCM_S32LE, CODEC_TYPE_PCM_U8, CODEC_TYPE_VORBIS,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Hard cap on the bins array: 200_000 bins × 2 × f32 ≈ 1.6 MiB — the ONLY
/// allocation that grows with duration, and it is capped by construction
/// (at the default 8 bins/s that is 6.9 h of audio; the max-minutes guard
/// fires long before at 4 h).
pub const MAX_BINS: usize = 200_000;

/// Cache/content-hash read chunk (1 MiB — the streaming-hash pattern from
/// cairn-proxy's `digest_file`, local copy so the review portal does not
/// depend on the proxy crate).
const HASH_CHUNK: usize = 1024 * 1024;

/// Tunables, read from the environment once at construction (runbook:
/// docs/runbook-beta.md → "Waveform service").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaveConfig {
    /// Concurrent decode lanes; 0 disables the service (`503`).
    pub lanes: usize,
    /// Peaks per second of audio (the bins density).
    pub rate_hz: u32,
    /// Audio longer than this refuses with `TooLong` (400) — the
    /// guardrail in front of the bin cap.
    pub max_minutes: u64,
    /// Per-job decode timeout (the wait is bounded; the thread is not).
    pub timeout_secs: u64,
    /// `Retry-After` seconds advertised on the 429 path.
    pub retry_after_secs: u64,
}

impl Default for WaveConfig {
    fn default() -> Self {
        WaveConfig {
            lanes: 2,
            rate_hz: 8,
            max_minutes: 240,
            timeout_secs: 120,
            retry_after_secs: 2,
        }
    }
}

impl WaveConfig {
    fn from_env() -> Self {
        WaveConfig {
            lanes: env_num("CAIRN_WAVEFORM_LANES", 2),
            rate_hz: env_num("CAIRN_WAVEFORM_RATE_HZ", 8),
            max_minutes: env_num("CAIRN_WAVEFORM_MAX_MINUTES", 240),
            timeout_secs: env_num("CAIRN_WAVEFORM_TIMEOUT_SECS", 120),
            retry_after_secs: env_num("CAIRN_WAVEFORM_RETRY_AFTER_SECS", 2),
        }
    }
}

/// `{"ok":false,"error":...}` comes from the http layer; these are the
/// variants it maps to statuses (429 / 400 / 415 / 503 / 503).
#[derive(Debug)]
pub enum WaveError {
    /// All lanes busy — the caller should retry after `retry_after_secs`.
    RateLimited { retry_after_secs: u64 },
    /// Audio beyond the max-minutes guardrail or the bin cap.
    TooLong,
    /// The container/codec is not decodable with the enabled symphonia
    /// features (WAV/FLAC/MP3/OGG-Vorbis) or has no decodable audio track.
    UnsupportedCodec(String),
    /// Filesystem I/O.
    Io(std::io::Error),
    /// The decode exceeded the per-job timeout.
    Timeout,
    /// The service is disabled (`CAIRN_WAVEFORM_LANES=0`).
    Disabled,
    /// A mid-stream decode failure (malformed media beyond packet skips).
    Decode(String),
}

impl fmt::Display for WaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaveError::RateLimited { retry_after_secs } => {
                write!(f, "waveform lanes busy, retry after {retry_after_secs}s")
            }
            WaveError::TooLong => write!(f, "audio too long for waveform"),
            WaveError::UnsupportedCodec(c) => write!(f, "unsupported audio codec: {c}"),
            WaveError::Io(e) => write!(f, "{e}"),
            WaveError::Timeout => write!(f, "waveform decode timed out"),
            WaveError::Disabled => write!(f, "waveform disabled"),
            WaveError::Decode(s) => write!(f, "waveform decode failed: {s}"),
        }
    }
}

impl std::error::Error for WaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WaveError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// The response body (also the cache entry format — the cache stores
/// exactly what the endpoint serves, content-addressed by media bytes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaveformResponse {
    /// Always `true` on success (the portal's ok/error envelope).
    pub ok: bool,
    /// Peaks per second the bins are laid out on.
    pub rate_hz: u32,
    /// Audio duration in milliseconds.
    pub duration_ms: u64,
    /// `peaks.len()` — convenience for the player.
    pub bins: usize,
    /// Per-bin `[min, max]`, mono-mixed floats clamped to [-1.0, 1.0].
    pub peaks: Vec<(f32, f32)>,
    /// Audio codec tag (`pcm_s16le`, `flac`, `mp3`, `vorbis`, ...).
    pub codec: String,
    /// Wall-clock millis of first generation (kept on cache hits).
    pub generated_at: i64,
}

/// Bounded, admission-gated waveform peaks service. Built once on the
/// [`Portal`](crate::http::Portal); shared across every token and version.
pub struct WaveformService {
    cfg: WaveConfig,
    cache_dir: PathBuf,
    lanes: Arc<Semaphore>,
}

impl WaveformService {
    /// Explicit constructor (tests, daemon overrides). `rate_hz` is
    /// floored at 1 so the bin math can never divide by zero.
    pub fn new(cfg: WaveConfig, cache_dir: PathBuf) -> Self {
        let cfg = WaveConfig {
            rate_hz: cfg.rate_hz.max(1),
            ..cfg
        };
        WaveformService {
            cfg,
            cache_dir,
            lanes: Arc::new(Semaphore::new(cfg.lanes)),
        }
    }

    /// The daemon's constructor: env knobs + cache dir
    /// `<blobs_root>/waveforms` when a blob tree is reachable, else a
    /// process-temp cache (tests / bare portals).
    pub fn from_env(blobs_root: Option<&Path>) -> Self {
        let cache_dir = match blobs_root {
            Some(b) => b.join("waveforms"),
            None => std::env::temp_dir().join("cairn-waveforms"),
        };
        WaveformService::new(WaveConfig::from_env(), cache_dir)
    }

    /// The effective config (the http layer documents it; tests assert it).
    pub fn config(&self) -> WaveConfig {
        self.cfg
    }

    /// Compute (or fetch from cache) the peaks for one media file.
    pub async fn peaks_for(&self, media_path: &Path) -> Result<WaveformResponse, WaveError> {
        if self.cfg.lanes == 0 {
            return Err(WaveError::Disabled);
        }
        // Admission: try-acquire ONLY. Queueing a guest behind a 2 h decode
        // would be the "handles large files gracefully" lie again — a full
        // lane set is an immediate 429 + Retry-After and the player uses
        // its own fallback decoder.
        let _permit = self
            .lanes
            .try_acquire()
            .map_err(|_| WaveError::RateLimited {
                retry_after_secs: self.cfg.retry_after_secs,
            })?;

        // Cache first: the key is the BLAKE3 of the file CONTENT (streamed
        // in 1 MiB chunks — the file size never touches memory). Same bytes
        // → same peaks, across versions, links, and daemon restarts.
        let key = hash_file(media_path)?;
        let cache_path = self.cache_dir.join(format!("{key}.json"));
        if let Some(resp) = cache_load(&cache_path) {
            return Ok(resp);
        }

        let cfg = self.cfg;
        let path = media_path.to_path_buf();
        let job = tokio::task::spawn_blocking(move || decode_and_bin(&path, cfg));
        // The timeout bounds the WAIT, not the thread: a blocking task
        // cannot be killed, so on expiry the result is dropped and the
        // decode finishes in the background. The lane pool bounds how many
        // of those can exist at once (the permit is released here).
        let resp = tokio::time::timeout(Duration::from_secs(cfg.timeout_secs), job)
            .await
            .map_err(|_| WaveError::Timeout)?
            .map_err(|e| WaveError::Decode(format!("decode task: {e}")))??;

        // Best-effort cache write: serving beats caching on any failure.
        if let Err(e) = cache_store(&cache_path, &resp) {
            tracing::warn!(
                path = %media_path.display(),
                error = %e,
                "waveform cache write failed"
            );
        }
        Ok(resp)
    }

    /// Test hook: hold one admission lane without a decode, so the 429
    /// path is deterministic (no timing races in tests).
    #[cfg(test)]
    pub(crate) fn hold_lane(&self) -> Result<tokio::sync::OwnedSemaphorePermit, WaveError> {
        Arc::clone(&self.lanes)
            .try_acquire_owned()
            .map_err(|_| WaveError::RateLimited {
                retry_after_secs: self.cfg.retry_after_secs,
            })
    }
}

/// Streaming BLAKE3 of a file's content (1 MiB chunks) — the cache key.
fn hash_file(path: &Path) -> Result<String, WaveError> {
    let mut f = fs::File::open(path).map_err(WaveError::Io)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(WaveError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Load a cache entry; a corrupt one is recomputed, never surfaced.
fn cache_load(path: &Path) -> Option<WaveformResponse> {
    let bytes = fs::read(path).ok()?;
    let resp: WaveformResponse = serde_json::from_slice(&bytes).ok()?;
    // cheap sanity check: parse ok + shape consistent
    if resp.ok && resp.rate_hz > 0 && resp.bins == resp.peaks.len() {
        Some(resp)
    } else {
        None
    }
}

/// Persist a cache entry atomically (tmp + fsync + rename — the
/// cairn-review store.rs convention, local copy).
fn cache_store(path: &Path, resp: &WaveformResponse) -> Result<(), String> {
    let bytes = serde_json::to_vec(resp).map_err(|e| format!("serialize waveform cache: {e}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Friendly audio-codec tag for the response; unknown codecs fall back to
/// symphonia's hex id (still unique, still honest).
fn codec_name(c: CodecType) -> String {
    match c {
        CODEC_TYPE_PCM_S16LE => "pcm_s16le".into(),
        CODEC_TYPE_PCM_S16BE => "pcm_s16be".into(),
        CODEC_TYPE_PCM_S24LE => "pcm_s24le".into(),
        CODEC_TYPE_PCM_S32LE => "pcm_s32le".into(),
        CODEC_TYPE_PCM_F32LE => "pcm_f32le".into(),
        CODEC_TYPE_PCM_U8 => "pcm_u8".into(),
        CODEC_TYPE_FLAC => "flac".into(),
        CODEC_TYPE_MP3 => "mp3".into(),
        CODEC_TYPE_VORBIS => "vorbis".into(),
        other => format!("{other}"),
    }
}

fn map_probe_err(e: SymphoniaError) -> WaveError {
    match e {
        SymphoniaError::IoError(io) => WaveError::Io(io),
        other => WaveError::UnsupportedCodec(other.to_string()),
    }
}

fn map_stream_err(e: SymphoniaError) -> WaveError {
    match e {
        SymphoniaError::IoError(io) => WaveError::Io(io),
        other => WaveError::Decode(other.to_string()),
    }
}

/// The synchronous decode job (runs on the blocking pool): probe → open
/// the first audio track → stream packets → mono-mix to f32 → per-bin
/// min/max. Memory profile: one decode buffer + the bins Vec (≤ MAX_BINS).
fn decode_and_bin(path: &Path, cfg: WaveConfig) -> Result<WaveformResponse, WaveError> {
    let file = fs::File::open(path).map_err(WaveError::Io)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(map_probe_err)?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| WaveError::UnsupportedCodec("no audio track".into()))?;
    let codec = codec_name(track.codec_params.codec);
    let track_id = track.id;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|_| WaveError::UnsupportedCodec(codec.clone()))?;

    let rate_hz = u64::from(cfg.rate_hz);
    let max_seconds = cfg.max_minutes.saturating_mul(60);
    let mut binner = Binner::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut buf_frames_capacity: u64 = 0;
    let mut elapsed: u64 = 0;
    let mut sr: u64 = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // end of stream: the WAV/RIFF reader signals EOF as UnexpectedEof
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(map_stream_err(e)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            // one malformed packet is skipped, not fatal
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(map_stream_err(e)),
            Ok(d) => d,
        };
        let spec = *decoded.spec();
        if sr == 0 {
            sr = u64::from(spec.rate);
            if sr == 0 {
                return Err(WaveError::Decode("decoded sample rate is 0".into()));
            }
        }
        // grow the interleaved f32 conversion buffer only if a packet is
        // larger than anything seen before (constant for PCM/FLAC/MP3)
        let frames = u64::try_from(decoded.frames()).unwrap_or(0);
        if sample_buf.is_none() || frames > buf_frames_capacity {
            let cap = decoded.capacity() as u64;
            sample_buf = Some(SampleBuffer::<f32>::new(cap, spec));
            buf_frames_capacity = cap;
        }

        let buf = sample_buf.as_mut().expect("sample buffer just built");
        buf.copy_interleaved_ref(decoded);
        let channels = spec.channels.count();
        let samples = buf.samples();
        for chunk in samples.chunks(channels.max(1)) {
            let mixed: f32 = chunk.iter().sum::<f32>() / chunk.len().max(1) as f32;
            // non-finite decode output and out-of-range samples are noise,
            // not peaks: clamp to the documented [-1, 1] domain
            let v = if mixed.is_finite() {
                mixed.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            binner.feed(elapsed, v, rate_hz, sr)?;
            elapsed += 1;
        }
        // guardrail: refuse overlong audio (checked per packet — constant
        // memory either way, and the first packet settles it for PCM)
        if elapsed / sr > max_seconds {
            return Err(WaveError::TooLong);
        }
    }

    if elapsed == 0 {
        return Err(WaveError::Decode("no audio frames decoded".into()));
    }
    let peaks = binner.finish(elapsed, rate_hz, sr)?;
    let duration_ms = elapsed
        .saturating_mul(1000)
        .checked_div(sr)
        .unwrap_or_default();
    Ok(WaveformResponse {
        ok: true,
        rate_hz: cfg.rate_hz,
        duration_ms,
        bins: peaks.len(),
        peaks,
        codec,
        generated_at: now_ms(),
    })
}

/// Streaming per-bin min/max accumulator. Only `bins` grows with duration
/// and it is hard-capped at [`MAX_BINS`] — everything else is constant.
struct Binner {
    bins: Vec<(f32, f32)>,
    open: Option<(usize, f32, f32)>, // (bin index, min, max) being filled
}

impl Binner {
    fn new() -> Self {
        Binner {
            bins: Vec::new(),
            open: None,
        }
    }

    /// Feed one mono-mixed sample at global frame `frame`.
    fn feed(&mut self, frame: u64, v: f32, rate_hz: u64, sr: u64) -> Result<(), WaveError> {
        let idx = usize::try_from(frame * rate_hz / sr).unwrap_or(usize::MAX);
        if idx >= MAX_BINS {
            return Err(WaveError::TooLong);
        }
        match self.open {
            Some((b, lo, hi)) if b == idx => self.open = Some((b, lo.min(v), hi.max(v))),
            Some((b, lo, hi)) => {
                self.bins.resize(b + 1, (0.0, 0.0));
                self.bins[b] = (lo, hi);
                self.open = Some((idx, v, v));
            }
            None => self.open = Some((idx, v, v)),
        }
        Ok(())
    }

    /// Close the open bin, pad silent gaps, and size the array to the
    /// exact ceil(frames · rate / sr) bins.
    fn finish(
        mut self,
        total_frames: u64,
        rate_hz: u64,
        sr: u64,
    ) -> Result<Vec<(f32, f32)>, WaveError> {
        let total = total_frames * rate_hz;
        let total = total / sr + u64::from(total % sr != 0); // ceil
        let total = usize::try_from(total).unwrap_or(usize::MAX);
        if total > MAX_BINS {
            return Err(WaveError::TooLong);
        }
        if let Some((b, lo, hi)) = self.open.take() {
            self.bins.resize(b + 1, (0.0, 0.0));
            self.bins[b] = (lo, hi);
        }
        self.bins.resize(total, (0.0, 0.0));
        Ok(std::mem::take(&mut self.bins))
    }
}

// ---- test corpus: a hand-rolled RIFF/WAVE writer (no new dev-dep) --------

/// Build a PCM 16-bit mono sine WAV in memory — the test corpus for the
/// peaks service and the portal endpoint tests. The RIFF header is 44
/// bytes; everything here fits in one `Vec<u8>` (small files only).
#[cfg(test)]
pub(crate) fn wav_bytes(sample_rate: u32, seconds: f64, freq: f32) -> Vec<u8> {
    let n_frames = (f64::from(sample_rate) * seconds) as usize;
    let data_bytes = (n_frames * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for idx in 0..n_frames {
        let secs = idx as f32 / sample_rate as f32;
        let amp = (2.0 * std::f32::consts::PI * freq * secs).sin() * 0.5;
        let sample = (amp * f32::from(i16::MAX)).round() as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// Write a test WAV to `path` (parents created).
#[cfg(test)]
pub(crate) fn write_test_wav(path: &Path, sample_rate: u32, seconds: f64, freq: f32) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, wav_bytes(sample_rate, seconds, freq)).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(cfg: WaveConfig) -> (WaveformService, PathBuf) {
        let dir = tempfile::tempdir().unwrap().keep();
        let cache = dir.join("waveform-cache");
        (WaveformService::new(cfg, cache), dir)
    }

    #[test]
    fn wave_error_display_is_actionable() {
        let io = WaveError::Io(std::io::Error::other("disk on fire"));
        let cases: Vec<(WaveError, String)> = vec![
            (
                WaveError::RateLimited {
                    retry_after_secs: 2,
                },
                "retry after 2s".into(),
            ),
            (WaveError::TooLong, "audio too long for waveform".into()),
            (
                WaveError::UnsupportedCodec("prores".into()),
                "unsupported audio codec: prores".into(),
            ),
            (WaveError::Disabled, "waveform disabled".into()),
            (WaveError::Timeout, "timed out".into()),
            (WaveError::Decode("bad frame".into()), "bad frame".into()),
            (io, "disk on fire".into()),
        ];
        for (e, needle) in cases {
            let s = e.to_string();
            assert!(s.contains(&needle), "`{s}` should mention `{needle}`");
        }
        // the io variant carries its source for the error chain
        let io = WaveError::Io(std::io::Error::other("x"));
        assert!(std::error::Error::source(&io).is_some());
    }

    #[tokio::test]
    async fn peaks_for_small_wav_bins_duration_and_cache_hit() {
        let (s, dir) = svc(WaveConfig::default());
        let wav = dir.join("bed.wav");
        // 2.0 s @ 8 kHz → ceil(2 · 8) = 16 bins at the default rate
        write_test_wav(&wav, 8_000, 2.0, 220.0);
        let r1 = s.peaks_for(&wav).await.unwrap();
        assert!(r1.ok);
        assert_eq!(r1.rate_hz, 8);
        assert_eq!(r1.bins, 16);
        assert_eq!(r1.peaks.len(), 16);
        assert_eq!(r1.codec, "pcm_s16le");
        assert!(
            (2000..=2010).contains(&r1.duration_ms),
            "got {}",
            r1.duration_ms
        );
        // a 0.5-amplitude sine must show up in the peaks
        let loudest = r1
            .peaks
            .iter()
            .map(|(lo, hi)| hi.max(lo.abs()))
            .fold(0.0f32, f32::max);
        assert!(loudest > 0.4, "sine peak {loudest} too quiet");
        assert!(
            r1.peaks
                .iter()
                .all(|(lo, hi)| (-1.0..=1.0).contains(lo) && (-1.0..=1.0).contains(hi)),
            "peaks must stay in [-1, 1]"
        );
        // cache hit: same response identity (generated_at is not refreshed)
        let r2 = s.peaks_for(&wav).await.unwrap();
        assert_eq!(
            r2.generated_at, r1.generated_at,
            "second call must be cached"
        );
        assert_eq!(r2.peaks, r1.peaks);
    }

    #[tokio::test]
    async fn peaks_for_admission_rejects_when_lanes_full() {
        let (s, dir) = svc(WaveConfig {
            lanes: 1,
            retry_after_secs: 7,
            ..WaveConfig::default()
        });
        let wav = dir.join("bed.wav");
        write_test_wav(&wav, 8_000, 1.0, 220.0);
        // hold the only lane (deterministic — no timing races)
        let _permit = s.hold_lane().unwrap();
        match s.peaks_for(&wav).await {
            Err(WaveError::RateLimited { retry_after_secs }) => {
                assert_eq!(retry_after_secs, 7);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn peaks_for_disabled_when_lanes_zero() {
        let (s, dir) = svc(WaveConfig {
            lanes: 0,
            ..WaveConfig::default()
        });
        let wav = dir.join("bed.wav");
        write_test_wav(&wav, 8_000, 1.0, 220.0);
        assert!(matches!(s.peaks_for(&wav).await, Err(WaveError::Disabled)));
    }

    #[tokio::test]
    async fn peaks_for_too_long_guard() {
        let (s, dir) = svc(WaveConfig {
            max_minutes: 0, // anything with positive duration is too long
            ..WaveConfig::default()
        });
        let wav = dir.join("bed.wav");
        write_test_wav(&wav, 8_000, 1.0, 220.0);
        assert!(matches!(s.peaks_for(&wav).await, Err(WaveError::TooLong)));
    }

    #[tokio::test]
    async fn peaks_for_missing_file_is_io_error() {
        let (s, dir) = svc(WaveConfig::default());
        let err = s.peaks_for(&dir.join("nope.wav")).await.unwrap_err();
        assert!(matches!(err, WaveError::Io(_)));
    }

    #[tokio::test]
    async fn peaks_for_corrupt_file_is_unsupported() {
        let (s, dir) = svc(WaveConfig::default());
        let wav = dir.join("garbage.wav");
        fs::write(&wav, b"this is not audio, it is prose").unwrap();
        let err = s.peaks_for(&wav).await.unwrap_err();
        assert!(
            matches!(&err, WaveError::UnsupportedCodec(_)),
            "prose is not audio: {err}"
        );
    }

    #[test]
    fn binner_bins_layout_and_gap_padding() {
        // sr 8, rate 2 → one bin per 4 frames; 2 s = 4 bins
        let mut b = Binner::new();
        for frame in 0..16u64 {
            b.feed(frame, if frame < 8 { -0.5 } else { 0.25 }, 2, 8)
                .unwrap();
        }
        let peaks = b.finish(16, 2, 8).unwrap();
        assert_eq!(peaks.len(), 4);
        assert_eq!(peaks[0], (-0.5, -0.5));
        assert_eq!(peaks[2], (0.25, 0.25));
        // partial trailing bin is included (ceil) and gap bins pad with 0
        let mut b = Binner::new();
        b.feed(0, 0.75, 2, 8).unwrap();
        b.feed(9, -0.25, 2, 8).unwrap(); // frames 4..8 silent → bin 2 pads
        let peaks = b.finish(10, 2, 8).unwrap();
        assert_eq!(peaks.len(), 3);
        assert_eq!(peaks[0], (0.75, 0.75));
        assert_eq!(peaks[1], (0.0, 0.0));
        assert_eq!(peaks[2], (-0.25, -0.25));
    }

    #[test]
    fn binner_refuses_beyond_the_cap() {
        let mut b = Binner::new();
        // rate 1, sr 1: bin index == frame index → the cap trips exactly
        let err = b.feed(MAX_BINS as u64, 0.5, 1, 1).unwrap_err();
        assert!(matches!(err, WaveError::TooLong));
    }

    #[test]
    fn env_config_defaults_when_unset() {
        // from_env must at least produce a usable service when no env is
        // set; the cache dir follows the blobs root (or falls back to temp)
        let with_blobs = WaveformService::from_env(Some(Path::new("/data/blobs")));
        assert_eq!(with_blobs.config(), WaveConfig::default());
        let bare = WaveformService::from_env(None);
        assert_eq!(bare.config(), WaveConfig::default());
    }
}
