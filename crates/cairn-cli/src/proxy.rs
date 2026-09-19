//! Proxy workflow CLI (ADR-0020 §3): generate lightweight proxies for
//! remote editors + the review portal, list them, check staleness.

use std::path::Path;

use cairn_proxy::model::ProxyProfile;
use cairn_proxy::transcode::{CopyTranscoder, FfmpegTranscoder, Transcoder};
use cairn_proxy::{generate, status_of};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `cairn proxy generate` — returns the proxy rel path on success.
pub fn cmd_generate(
    root: &Path,
    media: &str,
    max_height: u32,
    crf: u32,
    force_copy: bool,
) -> anyhow::Result<String> {
    let profile = ProxyProfile {
        max_height,
        codec: "h264".into(),
        crf,
    };
    let ffmpeg = FfmpegTranscoder;
    let copy = CopyTranscoder;
    let t: &dyn Transcoder = if force_copy {
        tracing::warn!("--copy: byte-copy transcoder (tests only — a copy is NOT a proxy)");
        &copy
    } else if ffmpeg.available() {
        &ffmpeg
    } else {
        anyhow::bail!(
            "no ffmpeg on PATH — install ffmpeg for real proxies, or pass --copy for the \
             pipeline smoke test"
        );
    };
    let entry = generate(root, media, &profile, t, now_ms()).map_err(anyhow::Error::msg)?;
    if let Some(e) = &entry.last_error {
        anyhow::bail!("transcode failed: {e}");
    }
    println!(
        "proxy: {} ({} bytes, {}p crf{})",
        entry.proxy_rel, entry.bytes, profile.max_height, profile.crf
    );
    println!(
        "pin it: cairn pin --project <id> --path {}",
        entry.proxy_rel
    );
    Ok(entry.proxy_rel)
}

/// `cairn proxy list` — every indexed proxy with its status.
///
/// Default is stat-fast (no rehash): entries whose stored (len, mtime)
/// match the current file are shown READY without reading media bytes.
/// Pass `verify=true` (`--verify`) to force a full blake3 rehash.
#[allow(clippy::unnecessary_wraps)] // symmetric with cmd_generate/cmd_status
pub fn cmd_list(root: &Path, verify: bool) -> anyhow::Result<()> {
    let idx = std::fs::read(cairn_proxy::pipeline::index_path(root))
        .map_err(anyhow::Error::msg)
        .and_then(|b| cairn_proxy::model::ProxyIndex::from_json(&b).map_err(anyhow::Error::msg))
        .unwrap_or_default();
    if idx.proxies.is_empty() {
        println!(
            "no proxies indexed in {} — run `cairn proxy generate`",
            root.display()
        );
        return Ok(());
    }
    for e in idx.proxies.values() {
        let state = if e.last_error.is_some() {
            "FAILED"
        } else {
            match cairn_proxy::pipeline::status_of_fast(root, &e.media_rel, verify)
                .map_err(anyhow::Error::msg)?
            {
                Some((latest, st)) if latest.source_digest == e.source_digest => st.as_str(),
                // entry is superseded by a newer generation for the same media
                Some(_) => "OLD",
                None => "STALE?",
            }
        };
        println!(
            "{:<10} {:<28} -> {}",
            state,
            e.media_rel.chars().take(28).collect::<String>(),
            e.proxy_rel
        );
        if let Some(err) = &e.last_error {
            println!("            {err}");
        }
    }
    Ok(())
}

/// `cairn proxy status --media X` — one file's proxy state.
pub fn cmd_status(root: &Path, media: &str) -> anyhow::Result<()> {
    match status_of(root, media).map_err(anyhow::Error::msg)? {
        None => println!("{media}: no proxy (generate one)"),
        Some((e, st)) => {
            println!("{media}: {}", st.as_str());
            println!("  proxy: {} ({} bytes)", e.proxy_rel, e.bytes);
            if st == cairn_proxy::model::ProxyStatus::Stale {
                println!("  source changed since generation — regenerate");
            }
        }
    }
    Ok(())
}
