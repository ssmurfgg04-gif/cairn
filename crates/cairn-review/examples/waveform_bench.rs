//! Waveform peaks benchmark (CONTRACT-DEBT #4).
//!
//! Generates a synthetic sine WAV (mono 48 kHz 16-bit, `--minutes` long)
//! under the OS temp dir with a STREAMED write (generation memory stays
//! flat too), then runs it through the real
//! [`cairn_review::waveform::WaveformService::peaks_for`] N times, with a
//! fresh cache each iteration, and reports per-iteration wall time plus
//! the peak-RSS high-water mark.
//!
//! The claim under test: peak RSS must NOT scale with audio duration
//! beyond the tiny bins array (`bins × 2 f32`, capped at 200k bins).
//! `VmHWM` (/proc/self/status) is the kernel's high-water mark — it never
//! shrinks, so the after−before delta is attributable to the run. On
//! non-Linux hosts the RSS column prints `n/a` (the wall time still works).
//!
//! Usage:
//! ```sh
//! cargo run --release -p cairn-review --example waveform_bench -- \
//!     --minutes 130 --rate-hz 8 --iters 3
//! ```
//! Output is script-friendly: `#` comment lines plus one markdown table
//! row per configuration.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant;

struct Args {
    minutes: u64,
    rate_hz: u32,
    iters: usize,
}

fn parse_args() -> Args {
    let mut minutes = 10u64;
    let mut rate_hz = 8u32;
    let mut iters = 3usize;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut idx = 0;
    while idx < argv.len() {
        let arg = argv[idx].clone();
        // support "--minutes 130" as well as "--minutes=130"
        let (key, value) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let v = argv
                    .get(idx + 1)
                    .cloned()
                    .unwrap_or_else(|| panic!("missing value for {arg}"));
                idx += 1;
                (arg, v)
            }
        };
        match key.as_str() {
            "--minutes" => minutes = value.parse().expect("--minutes must be an integer"),
            "--rate-hz" => rate_hz = value.parse().expect("--rate-hz must be an integer"),
            "--iters" => iters = value.parse().expect("--iters must be an integer"),
            other => panic!("unknown arg {other} (usage: [--minutes N] [--rate-hz H] [--iters K])"),
        }
        idx += 1;
    }
    Args {
        minutes,
        rate_hz,
        iters: iters.clamp(1, 20),
    }
}

/// RIFF/WAVE PCM 16-bit mono sine, streamed: the header first, then one
/// second of audio per write. Peak RSS of the BENCH ITSELF stays flat even
/// for a 2-hour file.
fn write_sine_wav(path: &std::path::Path, sample_rate: u32, seconds: u64) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let total_frames = u64::from(sample_rate) * seconds;
    let data_bytes = total_frames * 2; // mono, 16-bit
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes as u32).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&(sample_rate * 2).to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&(data_bytes as u32).to_le_bytes())?;

    let freq = 220.0f32;
    let phase_step = 2.0 * std::f32::consts::PI * freq / sample_rate as f32;
    let mut phase = 0.0f32;
    // one second of samples per iteration (96 KiB) — flat memory
    let per_sec = sample_rate as usize;
    for chunk_start in (0..total_frames).step_by(per_sec) {
        let frames = ((total_frames - chunk_start) as usize).min(per_sec);
        let mut buf = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let sample = (phase.sin() * 0.8 * f32::from(i16::MAX)).round() as i16;
            buf.extend_from_slice(&sample.to_le_bytes());
            phase += phase_step;
            if phase > 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
        }
        f.write_all(&buf)?;
    }
    f.flush()?;
    f.into_inner()?.sync_all()?;
    Ok(())
}

/// Kernel peak-RSS high-water mark in KiB (Linux only).
#[cfg(target_os = "linux")]
fn vm_hwm_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line["VmHWM:".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn vm_hwm_kib() -> Option<u64> {
    None
}

fn median_us(times: &[u128]) -> u128 {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let sample_rate = 48_000u32;
    let expected_bins = args.minutes * 60 * u64::from(args.rate_hz);

    // 2h10m mono 48 kHz 16-bit ≈ 750 MB — the bench deletes it at the end
    let wav_path: PathBuf =
        std::env::temp_dir().join(format!("cairn-waveform-bench-{}m.wav", args.minutes));
    let bytes_written = {
        print!("# generating {} min sine WAV ... ", args.minutes);
        std::io::stdout().flush().unwrap();
        let t0 = Instant::now();
        write_sine_wav(&wav_path, sample_rate, args.minutes * 60)
            .unwrap_or_else(|e| panic!("write {}: {e}", wav_path.display()));
        let meta = std::fs::metadata(&wav_path).expect("wav metadata");
        println!(
            "{} MiB in {:.1}s",
            meta.len() / (1024 * 1024),
            t0.elapsed().as_secs_f64()
        );
        meta.len()
    };

    let cache_dir =
        std::env::temp_dir().join(format!("cairn-waveform-bench-cache-{}m", args.minutes));
    let svc = cairn_review::waveform::WaveformService::new(
        cairn_review::waveform::WaveConfig {
            rate_hz: args.rate_hz,
            // generous decode timeout for slow hosts; lanes default (2)
            ..cairn_review::waveform::WaveConfig::default()
        },
        cache_dir.clone(),
    );

    let hwm_before = vm_hwm_kib().unwrap_or(0);
    let mut walls: Vec<u128> = Vec::with_capacity(args.iters);
    let mut bins_seen = 0u64;
    let mut codec_seen = String::new();
    for i in 0..args.iters {
        // fresh cache each iteration: the first decode of a REAL user is
        // the cold path, so the bench only ever measures the cold path
        let _ = std::fs::remove_dir_all(&cache_dir);
        let t0 = Instant::now();
        let resp = svc
            .peaks_for(&wav_path)
            .await
            .unwrap_or_else(|e| panic!("peaks_for iter {i}: {e}"));
        let wall = t0.elapsed().as_micros();
        let cache_bytes: u64 = std::fs::read_dir(&cache_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0);
        println!(
            "# iter {}: {} ms ({} bins, codec {}, cache {} bytes)",
            i + 1,
            wall / 1000,
            resp.bins,
            resp.codec,
            cache_bytes,
        );
        walls.push(wall);
        bins_seen = resp.bins as u64;
        codec_seen = resp.codec.clone();
    }
    let hwm_after = vm_hwm_kib().unwrap_or(0);

    // acceptance assertions: exact bin math + the hard cap
    assert!(
        bins_seen == expected_bins,
        "bins {bins_seen} != ceil(minutes·rate) {expected_bins}"
    );
    assert!(
        bins_seen <= cairn_review::waveform::MAX_BINS as u64,
        "bins exceed the hard cap"
    );

    let hwm_delta = hwm_after.saturating_sub(hwm_before);
    let rss_note = match (hwm_before, hwm_after) {
        (0, _) | (_, 0) => "n/a (non-Linux)".to_string(),
        _ => format!("{hwm_delta} KiB ({:.1} MiB)", hwm_delta as f64 / 1024.0),
    };
    let median_ms = median_us(&walls) / 1000;
    println!(
        "| {} min mono 48 kHz 16-bit sine ({} MiB) | {} | **{} ms** | **{}** | {} ({codec}) |",
        args.minutes,
        bytes_written / (1024 * 1024),
        args.iters,
        median_ms,
        rss_note,
        bins_seen,
        codec = codec_seen,
    );

    let _ = std::fs::remove_file(&wav_path);
    let _ = std::fs::remove_dir_all(&cache_dir);
    println!(
        "# done: temp WAV + cache deleted; peak RSS is process high-water (VmHWM), so the Δ after−before is attributable to the runs"
    );
}
