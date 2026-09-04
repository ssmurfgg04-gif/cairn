//! Intelligent clip search (ADR-0023 §5) — "search by what you see, not by
//! what you named the file", the honest no-AI way:
//!
//! 1. FILE surface: every project file row (path tokens, extension, size).
//! 2. CLIP surface: every `*.otio` / `*.fcpxml` in the project parsed with
//!    cairn-tl — each timeline clip indexed by name + media target + its
//!    timeline position (exact rational frames → timecode). So
//!    "worried closeup" finds `interview_worried_closeup.mov` AND the
//!    `scene3_v2.otio` range where it was cut in at 00:01:12 — offline,
//!    deterministic, zero new dependencies.
//!
//! Ranking: token overlap with prefix/substring boosts, deterministic
//! tie-breaks (score desc, path asc). No index is persisted: a scan over
//! the files table + bounded timeline parse per query (a project's
//! timeline docs are few and small; correctness over staleness).

use std::path::Path;

use cairn_tl::model::{Kind, Timeline, TrackKind};

/// One search hit.
#[derive(Clone, Debug)]
pub struct Hit {
    pub kind: &'static str, // "file" | "clip"
    pub path: String,
    pub score: i64,
    /// For clip hits: the clip name, media url, timecode in, duration.
    pub clip_name: Option<String>,
    pub clip_media: Option<String>,
    pub clip_tc_in: Option<String>,
    pub clip_dur: Option<String>,
}

/// Tokenize for matching: lowercase, split non-alphanumerics, drop empties
/// and pure digits shorter than 2 (a bare "3" is noise in queries like
/// "scene 3" — handled by multi-token scoring).
fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() >= 2)
        .map(str::to_string)
        .collect()
}

/// Score one haystack token set against the query tokens.
fn score(query: &[String], hay_path: &str, hay_name: &str) -> i64 {
    let hay = tokens(hay_path);
    let name_lower = hay_name.to_lowercase();
    let mut s = 0i64;
    for q in query {
        if hay.iter().any(|t| t == q) {
            // full-token match in the path: strong
            s += 10;
        } else if hay.iter().any(|t| t.starts_with(q.as_str())) {
            // prefix match (search*): good
            s += 6;
        } else if name_lower.contains(q.as_str()) {
            // substring inside the file name: decent
            s += 4;
        } else if hay_path.to_lowercase().contains(q.as_str()) {
            // substring elsewhere in the path: weak
            s += 2;
        }
    }
    // all query tokens matched by something: bonus (precision signal)
    if s > 0
        && query.iter().all(|q| {
            name_lower.contains(q.as_str()) || hay_path.to_lowercase().contains(q.as_str())
        })
    {
        s += 5;
    }
    s
}

/// Query a project root: files come from the caller (the store's rows);
/// timelines are parsed from disk here (bounded, failures skipped honestly).
#[must_use]
pub fn search_project(
    root: &Path,
    files: &[(String, u64)], // (project-relative path, size)
    query: &str,
    max_hits: usize,
) -> Vec<Hit> {
    let q = tokens(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<Hit> = Vec::new();

    // ---- file surface ----
    for (path, size) in files {
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let s = score(&q, path, &name);
        if s > 0 {
            hits.push(Hit {
                kind: "file",
                path: path.clone(),
                score: s,
                clip_name: None,
                clip_media: None,
                clip_tc_in: None,
                clip_dur: None,
            });
            let _ = size;
        }
    }

    // ---- clip surface: parse timelines under the root ----
    const TIMELINE_BUDGET: usize = 200; // docs per query
    let mut scanned = 0usize;
    scan_timelines(root, &mut |tl_path, tl| {
        if scanned >= TIMELINE_BUDGET {
            return;
        }
        scanned += 1;
        let rel = tl_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| tl_path.to_string_lossy().into_owned());
        // video-track walk with exact rational positions
        for (track_el, track_idx) in tracks_of(&tl) {
            let is_video = matches!(track_el.kind, Kind::Track(TrackKind::Video));
            let mut pos = cairn_tl::rational::Rational::ZERO;
            for item in &track_el.children {
                let dur = item
                    .source_range
                    .as_ref()
                    .and_then(|r| r.duration.seconds().ok())
                    .or_else(|| {
                        item.media
                            .as_ref()
                            .and_then(|m| m.available_range.as_ref())
                            .and_then(|r| r.duration.seconds().ok())
                    });
                let name = item.name.clone();
                let media = item
                    .active_media_url()
                    .map(|u| u.split('/').next_back().unwrap_or(&u).to_string());
                let (scored_name, score_src) = (
                    name.clone(),
                    format!(
                        "{rel} {} {}",
                        name.clone(),
                        media.clone().unwrap_or_default()
                    ),
                );
                let s = score(&q, &score_src, &name);
                if s > 0 && !name.is_empty() {
                    // timecode at the clip's timeline position, at 24fps
                    // display basis when the doc carries no rate
                    let rate = item
                        .source_range
                        .as_ref()
                        .map(|r| r.start.rate)
                        .filter(|r| !r.is_zero())
                        .unwrap_or(cairn_tl::rational::Rational::new(24, 1).unwrap_or_default());
                    let frame = pos.checked_mul(rate).map(|f| f.num).unwrap_or(0);
                    let secs = dur.map(|d| d.to_f64_approx());
                    // frame index at the clip's rate; timecode at that rate
                    hits.push(Hit {
                        kind: "clip",
                        path: rel.clone(),
                        score: s + if is_video { 2 } else { 0 },
                        clip_name: Some(scored_name),
                        clip_media: media,
                        clip_tc_in: Some(cairn_tl::notes::csv::timecode(frame, rate.num)),
                        clip_dur: secs.map(|d| format!("{d:.2}s")),
                    });
                }
                if let Some(d) = dur {
                    pos = pos.checked_add(d).unwrap_or(pos);
                }
            }
            let _ = track_idx;
        }
    });

    // deterministic: score desc, then path, then clip name
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.clip_name.cmp(&b.clip_name))
    });
    hits.truncate(max_hits);
    hits
}

fn tracks_of(tl: &Timeline) -> Vec<(&cairn_tl::model::Element, usize)> {
    tl.tracks
        .children
        .iter()
        .filter(|c| matches!(c.kind, Kind::Track(_)))
        .enumerate()
        .map(|(i, c)| (c, i))
        .collect()
}

/// Walk the root for .otio/.fcpxml files, parse each, call back. Bounded and
/// honest: unreadable/oversized/corrupt files are skipped (the caller's file
/// surface still lists them).
fn scan_timelines(root: &Path, cb: &mut dyn FnMut(&Path, Timeline)) {
    fn walk(dir: &Path, cb: &mut dyn FnMut(&Path, Timeline), budget: &mut usize) {
        if *budget == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                // branch stores + cairn internals are not the edit surface
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".cairn") {
                    continue;
                }
                walk(&path, cb, budget);
            } else if let Some(ext) = path.extension().and_then(|x| x.to_str()) {
                if *budget == 0 {
                    return;
                }
                if ext.eq_ignore_ascii_case("otio") || ext.eq_ignore_ascii_case("fcpxml") {
                    if meta.len() > 20 * 1024 * 1024 {
                        continue;
                    }
                    if let Ok(bytes) = std::fs::read_to_string(&path) {
                        let parsed = if ext.eq_ignore_ascii_case("fcpxml") {
                            cairn_tl::fcpxml::parse_fcpxml(&bytes).ok()
                        } else {
                            cairn_tl::parse::parse_otio(&bytes).ok()
                        };
                        if let Some(tl) = parsed {
                            *budget -= 1;
                            cb(&path, tl);
                        }
                    }
                }
            }
        }
    }
    let mut budget = 200usize;
    walk(root, cb, &mut budget);
}

/// Render hits for the CLI.
pub fn render(hits: &[Hit]) {
    if hits.is_empty() {
        println!("no matches — try fewer/different words (tokens: name, path, media)");
        return;
    }
    for h in hits {
        match h.kind {
            "clip" => println!(
                "[clip] {}/{}  {}  {}{}",
                h.path,
                h.clip_name.as_deref().unwrap_or("?"),
                h.clip_tc_in.as_deref().unwrap_or(""),
                h.clip_media.as_deref().unwrap_or(""),
                h.clip_dur
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ),
            _ => println!("[file] {}", h.path),
        }
    }
}
