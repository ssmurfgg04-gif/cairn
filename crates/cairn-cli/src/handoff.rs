//! AAF/OMF handoff CLI (ADR-0020 §6) + the marker bridge surface
//! (`cairn review export-markers`).

use std::path::Path;

use cairn_tl::handoff::{
    blake3_digest, timeline_digest, HandoffFile, HandoffKind, HandoffManifest, HandoffStatus,
};
use cairn_tl::markers::{fcpxml_rate_fields, notes_to_fcpxml, notes_to_otio_at, true_rate};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn handoff_path(root: &Path) -> std::path::PathBuf {
    root.join(".cairn").join("handoffs.json")
}

/// Resolve a --timeline path: absolute stays, relative joins the project
/// root (the flag means "the cut in THIS project").
fn resolve(root: &Path, p: &str) -> String {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        root.join(path).to_string_lossy().into_owned()
    }
}

fn load(root: &Path) -> anyhow::Result<HandoffFile> {
    match std::fs::read(handoff_path(root)) {
        Ok(b) => HandoffFile::from_json(&b).map_err(anyhow::Error::msg),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HandoffFile::default()),
        Err(e) => Err(anyhow::anyhow!("read handoffs: {e}")),
    }
}

/// `cairn handoff record` — bind an exported AAF/OMF to the cut.
pub fn cmd_record(
    root: &Path,
    file: &str,
    timeline: Option<&str>,
    snapshot: Option<&str>,
    note: &str,
    by: &str,
) -> anyhow::Result<()> {
    if file.starts_with('/') || file.contains("..") {
        anyhow::bail!("--file must be RELATIVE to the project root");
    }
    let bytes = std::fs::read(root.join(file)).map_err(|e| anyhow::anyhow!("read {file}: {e}"))?;
    let kind = HandoffKind::sniff(&bytes).ok_or_else(|| {
        anyhow::anyhow!(
            "{file} is neither AAF (SSA/CFB magic) nor OMF (OMFI chunk) — pass the \
             exported interchange file"
        )
    })?;
    let (tl, fp) = match timeline {
        Some(p) => {
            let path = resolve(root, p);
            let (tl, _) = crate::load_timeline_sidecar(&path)?;
            let fp = timeline_digest(&tl);
            (Some(tl), fp)
        }
        None => (None, String::new()),
    };
    let _ = tl;
    let m = HandoffManifest {
        file_digest: blake3_digest(&bytes),
        file_bytes: bytes.len() as u64,
        kind,
        file_rel: file.to_string(),
        timeline_fingerprint: fp,
        snapshot: snapshot.map(str::to_string),
        exported_by: by.to_string(),
        exported_at_ms: now_ms(),
        note: note.to_string(),
    };
    let mut f = load(root)?;
    f.record(m);
    let json = f.to_json().map_err(anyhow::Error::msg)?;
    cairn_proxy::pipeline::atomic_write(&handoff_path(root), &json).map_err(anyhow::Error::msg)?;
    println!("recorded {kind:?} handoff: {file} ({} bytes)", bytes.len());
    if timeline.is_none() {
        println!(
            "note: no --timeline bound — the picture-lock check needs it; re-record with \
             --timeline cut.otio"
        );
    }
    Ok(())
}

/// `cairn handoff verify` — the sound-team contract check.
pub fn cmd_verify(root: &Path, file: Option<&str>, timeline: Option<&str>) -> anyhow::Result<()> {
    let ledger = load(root)?;
    if ledger.handoffs.is_empty() {
        println!("no handoffs recorded");
        return Ok(());
    }
    let current_tl = match timeline {
        Some(p) => Some(crate::load_timeline_sidecar(&resolve(root, p))?.0),
        None => None,
    };
    let mut bad = 0;
    for m in ledger.handoffs.values() {
        if let Some(want) = file {
            if m.file_rel != want {
                continue;
            }
        }
        let bytes_now = std::fs::read(root.join(&m.file_rel)).ok();
        let status = cairn_tl::handoff::verify(m, bytes_now.as_deref(), current_tl.as_ref());
        let verdict = match status {
            HandoffStatus::Current => "CURRENT",
            HandoffStatus::FileChanged => "FILE CHANGED",
            HandoffStatus::TimelineMoved => "TIMELINE MOVED (sound is cutting against an old cut!)",
        };
        if !matches!(status, HandoffStatus::Current) {
            bad += 1;
        }
        println!(
            "{:<12} {} ({}, {} bytes, by {}, {}) {}",
            verdict,
            m.file_rel,
            m.kind.as_str(),
            m.file_bytes,
            m.exported_by,
            m.note,
            if m.snapshot.is_some() {
                "snapshot-bound"
            } else {
                ""
            }
        );
    }
    if bad > 0 {
        anyhow::bail!("{bad} handoff(s) out of date — re-export or re-record");
    }
    Ok(())
}

/// `cairn handoff list`.
pub fn cmd_list(root: &Path) -> anyhow::Result<()> {
    let ledger = load(root)?;
    if ledger.handoffs.is_empty() {
        println!("no handoffs recorded");
        return Ok(());
    }
    for m in ledger.handoffs.values() {
        println!(
            "{}  {:<10} {:<28} timeline:{} {}",
            &m.file_digest[..12.min(m.file_digest.len())],
            m.kind.as_str(),
            m.file_rel.chars().take(28).collect::<String>(),
            &m.timeline_fingerprint[..12.min(m.timeline_fingerprint.len())],
            m.note
        );
    }
    Ok(())
}

/// `cairn review export-markers` — comments back into the NLE as markers
/// (FCP7 XML by default; OTIO when --otio).
///
/// The rate is the version's TRUE fps from `review.json` — never a
/// default. The first dogfood run caught the bug this guards against:
/// markers on a 25 fps cut exported at a hardcoded 24 landed 1.7 s late
/// at the one-minute mark, and a 23.976 cut carried `ntsc=FALSE`.
pub fn cmd_export_markers(
    root: &Path,
    version: u32,
    out: &str,
    as_otio: bool,
    timeline: Option<&str>,
) -> anyhow::Result<()> {
    let set =
        cairn_review::store::Store::load_comments(root, version).map_err(anyhow::Error::msg)?;
    if set.is_empty() {
        anyhow::bail!("no comments on v{version} to export");
    }
    // the version's true rate — the review session is the source of truth
    let review = cairn_review::store::Store::load(root).map_err(anyhow::Error::msg)?;
    let (fps_num, fps_den) = review
        .as_ref()
        .and_then(|f| f.version(version))
        .map(|v| (i64::from(v.fps_num.max(1)), i64::from(v.fps_den.max(1))))
        .unwrap_or((24, 1));
    let (timebase, ntsc) = fcpxml_rate_fields(fps_num, fps_den);
    if as_otio {
        let tl = match timeline {
            Some(p) => crate::load_timeline_sidecar(&resolve(root, p))?.0,
            None => cairn_tl::model::Timeline {
                name: format!("review v{version} markers"),
                global_start_time: None,
                metadata: cairn_tl::model::JsonMap::new(),
                tracks: cairn_tl::model::Element::container(
                    cairn_tl::model::Kind::Stack,
                    "tracks",
                    Vec::new(),
                ),
                extra: cairn_tl::model::JsonMap::new(),
            },
        };
        let with = notes_to_otio_at(&tl, &set, Some(true_rate(fps_num, fps_den)));
        let json = cairn_tl::canon::serialize_file(&with).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        std::fs::write(out, json).map_err(|e| anyhow::anyhow!("write {out}: {e}"))?;
    } else {
        let xml = notes_to_fcpxml(&set, timebase, ntsc, &format!("cairn review v{version}"));
        std::fs::write(out, xml).map_err(|e| anyhow::anyhow!("write {out}: {e}"))?;
    }
    println!(
        "exported {} marker(s) @ {}/{} fps (timebase {timebase}, ntsc {ntsc}) -> {out} \
         (import into Premiere/Resolve/FCP)",
        set.len(),
        fps_num,
        fps_den
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_review::model::ReviewVersion;

    fn tmp() -> std::path::PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    fn publish(root: &Path, fps: (u32, u32)) {
        let mut f = cairn_review::model::ReviewFile {
            title: "T".into(),
            ..Default::default()
        };
        f.publish(ReviewVersion {
            number: 0,
            label: "v1".into(),
            media_rel: "cuts/v1.mp4".into(),
            proxy_rel: None,
            fps_num: fps.0,
            fps_den: fps.1,
            frames: 100,
            timeline_fingerprint: None,
            snapshot: None,
            published_by: "a".into(),
            published_at: 1,
        });
        cairn_review::store::Store::save(root, &f).unwrap();
    }

    /// The dogfood TC-drift regression: a 25 fps cut must export markers
    /// at timebase 25 (the old code hardcoded 24), and a 23.976 cut must
    /// carry ntsc=TRUE (the old inference marked PAL 25 as NTSC).
    #[test]
    fn export_markers_use_the_versions_true_rate() {
        let root = tmp();
        publish(&root, (25, 1));
        cairn_review::store::Store::add_comment(&root, 1, "jane", "tighten", 42, 25, 100).unwrap();
        let out = root.join("markers.xml");
        cmd_export_markers(&root, 1, &out.to_string_lossy(), false, None).unwrap();
        let xml = std::fs::read_to_string(&out).unwrap();
        assert!(xml.contains("<timebase>25</timebase>"));
        assert!(xml.contains("<ntsc>FALSE</ntsc>"));
        assert!(xml.contains("<start>42</start>"));

        // 23.976: NDF 24 basis + ntsc=TRUE
        let root2 = tmp();
        publish(&root2, (24000, 1001));
        cairn_review::store::Store::add_comment(&root2, 1, "jane", "tighten", 96, 24, 100).unwrap();
        let out2 = root2.join("markers.xml");
        cmd_export_markers(&root2, 1, &out2.to_string_lossy(), false, None).unwrap();
        let xml2 = std::fs::read_to_string(&out2).unwrap();
        assert!(xml2.contains("<timebase>24</timebase>"));
        assert!(xml2.contains("<ntsc>TRUE</ntsc>"));

        // OTIO: markers carry the true rational rate (wire form is OTIO's
        // own f64: 24000/1001 -> 23.976...), frame index preserved
        let out3 = root2.join("markers.otio");
        cmd_export_markers(&root2, 1, &out3.to_string_lossy(), true, None).unwrap();
        let otio = std::fs::read_to_string(&out3).unwrap();
        assert!(otio.contains("\"rate\": 23.976"));
        assert!(otio.contains("\"value\": 96.0"));
    }
}
