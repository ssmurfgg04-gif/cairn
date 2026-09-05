//! Review portal CLI (ADR-0020): publish versions, mint guest links,
//! read comments, resolve notes — all against `<root>/.cairn/review.json`
//! and the per-version note files, which the sync engine carries to every
//! peer like any other project file.
//!
//! The daemon side (`cairn daemon --review 0.0.0.0:17778`) serves the
//! player; this module is the editor-side surface plus the root provider
//! the daemon's portal uses.

use std::path::{Path, PathBuf};

use cairn_review::http::RootProvider;
use cairn_review::model::{GuestRole, ReviewFile, ReviewVersion};
use cairn_review::store::Store;

/// The daemon's root provider: attached runtimes, live, plus the local
/// CAS blob tree for annotation overlays (ADR-0028 §D).
pub struct RuntimesProvider {
    /// Daemon home (the store root whose `blobs/` tree the portal serves).
    pub home: PathBuf,
}

#[async_trait::async_trait]
impl RootProvider for RuntimesProvider {
    async fn roots(&self) -> Vec<(String, PathBuf)> {
        let map = crate::projects::RUNTIMES.read().await;
        map.values()
            .map(|rt| (rt.project_id.clone(), rt.workspace.clone()))
            .collect()
    }

    fn blobs_root(&self) -> Option<PathBuf> {
        Some(self.home.join("blobs"))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// ffprobe the watchable media: (fps_num, fps_den, frames). `None` when
/// ffprobe is absent or the file is not probeable media — callers then
/// require explicit `--fps`/`--frames`.
///
/// This is the dogfood fix for the hand-counted-frames class of bugs:
/// publish used to trust the human ("240 frames at 24"), and every wrong
/// number silently corrupted every comment timecode bound after it.
/// The probe is authoritative when present; the flags remain the manual
/// override for prober-less machines.
pub fn probe_media(path: &Path) -> Option<(u32, u32, u64)> {
    let candidates = [
        "ffprobe",
        "ffprobe.exe",
        "/usr/local/bin/ffprobe",
        "/opt/homebrew/bin/ffprobe",
    ];
    let ffprobe = candidates.iter().find(|c| {
        std::process::Command::new(c)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })?;
    let out = std::process::Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate,nb_frames:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let stream = v.get("streams")?.as_array()?.first()?;
    let rate = stream.get("r_frame_rate")?.as_str()?;
    let (n, d) = rate.split_once('/')?;
    let n: u32 = n.parse().ok()?;
    let d: u32 = d.parse().ok().filter(|d| *d > 0)?;
    if n == 0 {
        return None;
    }
    // nb_frames is a container hint, not a promise: derive from duration
    // when missing or absurd
    let from_count: Option<u64> = stream
        .get("nb_frames")
        .and_then(|f| f.as_str())
        .and_then(|f| f.parse().ok())
        .filter(|f| *f > 0);
    let frames = from_count.or_else(|| {
        let dur: f64 = v.get("format")?.get("duration")?.as_str()?.parse().ok()?;
        Some((dur * f64::from(n) / f64::from(d)).round() as u64)
    })?;
    Some((n, d, frames.max(1)))
}

fn load_or_new(root: &Path, title: Option<&str>) -> anyhow::Result<ReviewFile> {
    match Store::load(root).map_err(anyhow::Error::msg)? {
        Some(mut f) => {
            if let Some(t) = title {
                if !t.is_empty() {
                    f.title = t.to_string();
                }
            }
            Ok(f)
        }
        None => {
            let title = title.filter(|t| !t.is_empty()).unwrap_or("Review");
            Ok(ReviewFile {
                schema: cairn_review::model::SCHEMA.into(),
                title: title.into(),
                versions: Vec::new(),
                links: Vec::new(),
            })
        }
    }
}

/// Parse an fps spec: "24", "25", "23.976", or "24000/1001".
pub fn parse_fps(spec: &str) -> anyhow::Result<(u32, u32)> {
    let s = spec.trim();
    if let Some((n, d)) = s.split_once('/') {
        let n: u32 = n.trim().parse()?;
        let d: u32 = d.trim().parse()?;
        if n == 0 || d == 0 {
            anyhow::bail!("fps must be nonzero: {spec}");
        }
        return Ok((n, d));
    }
    if let Ok(v) = s.parse::<u32>() {
        if v == 0 {
            anyhow::bail!("fps must be nonzero: {spec}");
        }
        return Ok((v, 1));
    }
    let f: f64 = s.parse()?;
    if f <= 0.0 {
        anyhow::bail!("fps must be positive: {spec}");
    }
    // decimal → the canonical rational of the nearest standard rate
    if (f - 23.976).abs() < 0.02 {
        Ok((24000, 1001))
    } else if (f - 29.97).abs() < 0.02 {
        Ok((30000, 1001))
    } else if (f - 59.94).abs() < 0.02 {
        Ok((60000, 1001))
    } else {
        let r = f.round();
        if (f - r).abs() > 0.02 {
            anyhow::bail!("non-standard fps {spec}; pass an exact rational like 24000/1001");
        }
        Ok((r as u32, 1))
    }
}

/// `cairn review publish` — append a version to the stack.
pub fn cmd_publish(
    root: &Path,
    title: Option<&str>,
    media: &str,
    proxy: Option<&str>,
    fps: (u32, u32),
    frames: u64,
    label: &str,
    timeline_fingerprint: Option<&str>,
    snapshot: Option<&str>,
    by: &str,
) -> anyhow::Result<u32> {
    if media.starts_with('/') || media.contains("..") {
        anyhow::bail!("--media must be a path RELATIVE to the project root");
    }
    let mut f = load_or_new(root, title)?;
    let v = ReviewVersion {
        number: 0,
        label: if label.is_empty() {
            format!("v{}", f.versions.len() + 1)
        } else {
            label.to_string()
        },
        media_rel: media.to_string(),
        proxy_rel: proxy.map(str::to_string),
        fps_num: fps.0,
        fps_den: fps.1,
        frames,
        timeline_fingerprint: timeline_fingerprint.map(str::to_string),
        snapshot: snapshot.map(str::to_string),
        published_by: by.to_string(),
        published_at: now_ms(),
    };
    let n = f.publish(v);
    Store::save(root, &f).map_err(anyhow::Error::msg)?;
    Ok(n)
}

/// `cairn review link` — mint a guest link; returns (token, expires_at).
pub fn cmd_link(
    root: &Path,
    role: GuestRole,
    note: &str,
    ttl_hours: i64,
    latest_only: bool,
) -> anyhow::Result<(String, i64)> {
    let mut f = load_or_new(root, None)?;
    if f.versions.is_empty() {
        anyhow::bail!("no versions published yet — run `cairn review publish` first");
    }
    let token = f.add_link(
        role,
        note.to_string(),
        ttl_hours.saturating_mul(3_600_000),
        latest_only,
        now_ms(),
    );
    Store::save(root, &f).map_err(anyhow::Error::msg)?;
    let link = f.links.iter().find(|l| l.token == token).unwrap();
    Ok((token, link.expires_at))
}

/// `cairn review list` — print the stack + links.
pub fn cmd_list(root: &Path) -> anyhow::Result<()> {
    let Some(f) = Store::load(root).map_err(anyhow::Error::msg)? else {
        println!(
            "no review session in {} (nothing published)",
            root.display()
        );
        return Ok(());
    };
    println!("title: {}", f.title);
    println!("versions:");
    for v in &f.versions {
        let n_comments = Store::load_comments(root, v.number)
            .map(|s| s.len())
            .unwrap_or(0);
        println!(
            "  v{:>2}  {:<28}  {:>6} frames @{}/{}  {}  notes:{}  by {}",
            v.number,
            v.label,
            v.frames,
            v.fps_num,
            v.fps_den,
            v.timecode(v.frames.saturating_sub(1)),
            n_comments,
            v.published_by,
        );
        if let Some(p) = &v.proxy_rel {
            println!("       proxy: {p}");
        }
        if let Some(fp) = &v.timeline_fingerprint {
            println!("       timeline: {fp}");
        }
    }
    let now = now_ms();
    println!("links:");
    if f.links.is_empty() {
        println!("  (none)");
    }
    for l in &f.links {
        let state = if l.is_expired(now) { "EXPIRED" } else { "live" };
        println!(
            "  {}  {:<10}  {:<24}  {}  {}",
            l.token,
            state,
            l.role.as_str(),
            if l.note.is_empty() { "-" } else { &l.note },
            if l.latest_only {
                "latest-only"
            } else {
                "full-stack"
            },
        );
    }
    Ok(())
}

/// `cairn review comments` — frame-anchored notes with timecodes.
pub fn cmd_comments(root: &Path, version: Option<u32>) -> anyhow::Result<()> {
    let Some(f) = Store::load(root).map_err(anyhow::Error::msg)? else {
        anyhow::bail!("no review session in {}", root.display());
    };
    let versions: Vec<&ReviewVersion> = match version {
        Some(n) => vec![f
            .version(n)
            .ok_or_else(|| anyhow::anyhow!("no version {n} in the stack"))?],
        None => f.versions.iter().collect(),
    };
    for v in versions {
        let set = Store::load_comments(root, v.number).map_err(anyhow::Error::msg)?;
        println!("v{} — {} ({} notes)", v.number, v.label, set.len());
        let mut rows: Vec<_> = set.notes.values().collect();
        rows.sort_by(|a, b| a.anchor.frame.cmp(&b.anchor.frame).then(a.id.cmp(&b.id)));
        for n in rows {
            println!(
                "  {}  {:<12}  {:<8}  {}",
                v.timecode(n.anchor.frame.max(0) as u64),
                n.author,
                n.status.as_str(),
                n.body.replace('\n', " / ")
            );
        }
    }
    Ok(())
}

/// `cairn review resolve` / `reopen`.
pub fn cmd_resolve(
    root: &Path,
    version: u32,
    id: &str,
    status: cairn_tl::notes::NoteStatus,
) -> anyhow::Result<()> {
    Store::set_status(root, version, id, status).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    #[test]
    fn fps_specs_parse_to_exact_rationals() {
        assert_eq!(parse_fps("24").unwrap(), (24, 1));
        assert_eq!(parse_fps("25").unwrap(), (25, 1));
        assert_eq!(parse_fps("23.976").unwrap(), (24000, 1001));
        assert_eq!(parse_fps("29.97").unwrap(), (30000, 1001));
        assert_eq!(parse_fps("59.94").unwrap(), (60000, 1001));
        assert_eq!(parse_fps("24000/1001").unwrap(), (24000, 1001));
        assert!(parse_fps("0").is_err());
        assert!(parse_fps("-5").is_err());
        assert!(parse_fps("23.1234").is_err());
    }

    #[test]
    fn publish_assigns_numbers_and_link_mints_after_publish() {
        let root = tmp();
        let n1 = cmd_publish(
            &root,
            Some("Brand Film"),
            "cuts/v1.mp4",
            None,
            (24, 1),
            240,
            "",
            None,
            None,
            "editor-a",
        )
        .unwrap();
        assert_eq!(n1, 1);
        let n2 = cmd_publish(
            &root,
            None,
            "cuts/v2.mp4",
            Some("proxies/v2.mp4"),
            (24000, 1001),
            242,
            "picture lock",
            Some("fp-abc"),
            None,
            "editor-a",
        )
        .unwrap();
        assert_eq!(n2, 2);

        // no versions → link fails; with versions → mints
        let root2 = tmp();
        assert!(cmd_link(&root2, GuestRole::Commenter, "x", 48, false).is_err());
        let (tok, exp) = cmd_link(&root, GuestRole::Viewer, "acme client", 48, true).unwrap();
        assert_eq!(tok.len(), 32);
        assert!(exp > now_ms());
        let f = Store::load(&root).unwrap().unwrap();
        assert_eq!(f.title, "Brand Film");
        assert_eq!(f.versions.len(), 2);
        assert_eq!(
            f.versions[1].timeline_fingerprint.as_deref(),
            Some("fp-abc")
        );
    }

    #[test]
    fn media_must_be_relative() {
        let root = tmp();
        assert!(cmd_publish(
            &root,
            None,
            "/abs/path.mp4",
            None,
            (24, 1),
            10,
            "",
            None,
            None,
            "a"
        )
        .is_err());
        assert!(cmd_publish(
            &root,
            None,
            "../escape.mp4",
            None,
            (24, 1),
            10,
            "",
            None,
            None,
            "a"
        )
        .is_err());
        assert!(cmd_publish(
            &root,
            None,
            "cuts/v3.mp4",
            None,
            (24, 1),
            10,
            "",
            None,
            None,
            "a"
        )
        .is_ok());
    }

    /// ffprobe round: a real 2 s 25 fps file must probe (25, 1, ~50) —
    /// this is what publish auto-fills from. Skips without ffprobe.
    #[test]
    fn probe_media_reads_real_rate_and_frames() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cut.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=25",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&src)
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("skipping: no ffmpeg");
            return;
        }
        let (n, d, frames) = probe_media(&src).expect("probe must succeed on real media");
        assert_eq!((n, d), (25, 1));
        assert!(
            (45..=55).contains(&frames),
            "2 s at 25 fps ≈ 50 frames, got {frames}"
        );
        // and a non-media file probes to None (fail-closed, not garbage)
        let junk = dir.path().join("junk.txt");
        std::fs::write(&junk, b"not media").unwrap();
        assert!(probe_media(&junk).is_none());
        assert!(probe_media(&dir.path().join("missing.mp4")).is_none());
    }
}
