//! The generation pipeline: digest the source, skip-if-current, transcode,
//! index. Persistence mirrors cairn-review's store conventions (atomic
//! write, fail-closed parse, deterministic JSON).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::model::{ProxyEntry, ProxyIndex, ProxyProfile};
use crate::transcode::{TranscodeOutput, Transcoder};

/// `<root>/.cairn/proxies.json`
pub fn index_path(root: &Path) -> PathBuf {
    root.join(".cairn").join("proxies.json")
}

/// Proxy file for a source digest: `<root>/.cairn/proxy-cache/<digest>.mp4`
/// (the digest key means re-renders never collide with old proxies).
pub fn proxy_rel_for(digest: &str) -> String {
    format!(".cairn/proxy-cache/{digest}.mp4")
}

/// blake3 hex digest of a file, streamed (50 GB sources never load into
/// memory).
pub fn digest_file(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Atomic write (tmp + rename) — crash mid-write never leaves a
/// half-written index for the sync engine to journal.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))
}

fn load_index(root: &Path) -> Result<ProxyIndex, String> {
    let p = index_path(root);
    match fs::read(&p) {
        Ok(b) => ProxyIndex::from_json(&b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProxyIndex::default()),
        Err(e) => Err(format!("read {}: {e}", p.display())),
    }
}

/// Status of a media file's proxy: current entry + its state, or None.
pub fn status_of(
    root: &Path,
    media_rel: &str,
) -> Result<Option<(ProxyEntry, crate::model::ProxyStatus)>, String> {
    let src = root.join(media_rel.trim_start_matches(['/', '\\']));
    if !src.is_file() {
        return Ok(None);
    }
    let digest = digest_file(&src)?;
    let idx = load_index(root)?;
    Ok(idx
        .proxies
        .values()
        .filter(|e| e.media_rel == media_rel)
        .max_by_key(|e| e.generated_at_ms)
        .map(|e| (e.clone(), e.status(&digest))))
}

/// Generate (or reuse) the proxy for one media file.
///
/// * Deterministic reuse: if the index has a current entry for this
///   source digest and the proxy file still exists, it is returned
///   untouched (idempotent re-runs are free).
/// * Stale handling: a changed source digest generates a NEW proxy
///   (new key); old entries stay for history until pruned.
pub fn generate(
    root: &Path,
    media_rel: &str,
    profile: &ProxyProfile,
    transcoder: &dyn Transcoder,
    now_ms: i64,
) -> Result<ProxyEntry, String> {
    if media_rel.starts_with('/') || media_rel.contains("..") {
        return Err("media path must be RELATIVE to the project root".into());
    }
    if media_rel.starts_with(".cairn/") {
        return Err("refusing to proxy cairn state files".into());
    }
    let src = root.join(media_rel);
    let meta = fs::metadata(&src).map_err(|e| format!("media {media_rel}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("media {media_rel} is not a file"));
    }

    let digest = digest_file(&src)?;
    let mut idx = load_index(root)?;

    // reuse: current entry + file present
    if let Some(existing) = idx.current_for(media_rel, &digest) {
        let existing = existing.clone();
        if root.join(&existing.proxy_rel).is_file() {
            return Ok(existing);
        }
        // indexed but the file vanished (cache prune, partial sync):
        // fall through and regenerate at the same key
    }

    if !transcoder.available() {
        return Err(format!(
            "transcoder '{}' unavailable in this environment",
            transcoder.name()
        ));
    }
    let proxy_rel = proxy_rel_for(&digest);
    let dst = root.join(&proxy_rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir proxy-cache: {e}"))?;
    }
    let transcode_result = transcoder.transcode(&src, &dst, profile);
    let bytes = match transcode_result {
        Ok(TranscodeOutput { bytes }) => bytes,
        Err(err) => {
            // record the failure honestly in the index, then surface it
            let fail = ProxyEntry {
                source_digest: digest.clone(),
                media_rel: media_rel.to_string(),
                proxy_rel: proxy_rel.clone(),
                profile: profile.clone(),
                bytes: 0,
                generated_at_ms: now_ms,
                last_error: Some(err.clone()),
            };
            idx.proxies.insert(digest.clone(), fail);
            if let Ok(json) = idx.to_json() {
                let _ = atomic_write(&index_path(root), &json);
            }
            return Err(err);
        }
    };

    let entry = ProxyEntry {
        source_digest: digest,
        media_rel: media_rel.to_string(),
        proxy_rel,
        profile: profile.clone(),
        bytes,
        generated_at_ms: now_ms,
        last_error: None,
    };
    idx.proxies
        .insert(entry.source_digest.clone(), entry.clone());
    atomic_write(&index_path(root), &idx.to_json()?)?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcode::CopyTranscoder;

    fn setup() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        std::fs::create_dir_all(root.join("cuts")).unwrap();
        std::fs::write(root.join("cuts/v1.mov"), b"AAAA-media-bytes-v1").unwrap();
        root
    }

    #[test]
    fn generate_is_idempotent_and_reuses_current_proxy() {
        let root = setup();
        let t = CopyTranscoder;
        let p = ProxyProfile::default();
        let e1 = generate(&root, "cuts/v1.mov", &p, &t, 1000).unwrap();
        assert_eq!(e1.bytes, b"AAAA-media-bytes-v1".len() as u64);
        assert_eq!(e1.source_digest.len(), 64); // blake3 hex

        // second run: SAME entry (reused, not regenerated)
        let e2 = generate(&root, "cuts/v1.mov", &p, &t, 2000).unwrap();
        assert_eq!(e1, e2);

        // proxy file exists at the digest-keyed path and matches the source
        let proxy = root.join(&e1.proxy_rel);
        assert!(proxy.is_file());
        assert_eq!(std::fs::read(&proxy).unwrap(), b"AAAA-media-bytes-v1");
    }

    #[test]
    fn source_edit_marks_stale_and_regenerates() {
        let root = setup();
        let t = CopyTranscoder;
        let p = ProxyProfile::default();
        let e1 = generate(&root, "cuts/v1.mov", &p, &t, 1000).unwrap();

        // status: current
        let (entry, st) = status_of(&root, "cuts/v1.mov").unwrap().unwrap();
        assert_eq!(st, crate::model::ProxyStatus::Ready);

        // edit the media (new digest)
        std::fs::write(root.join("cuts/v1.mov"), b"BBBB-media-bytes-v2").unwrap();
        let (_, st2) = status_of(&root, "cuts/v1.mov").unwrap().unwrap();
        assert_eq!(st2, crate::model::ProxyStatus::Stale);
        assert_eq!(entry.source_digest, e1.source_digest);

        // regenerate: NEW digest key, old entry retained in the index
        let e2 = generate(&root, "cuts/v1.mov", &p, &t, 3000).unwrap();
        assert_ne!(e1.source_digest, e2.source_digest);
        let idx = load_index(&root).unwrap();
        assert_eq!(idx.proxies.len(), 2);
        assert!(idx.current_for("cuts/v1.mov", &e1.source_digest).is_some());
    }

    #[test]
    fn missing_proxy_file_regenerates_at_same_key() {
        let root = setup();
        let t = CopyTranscoder;
        let p = ProxyProfile::default();
        let e1 = generate(&root, "cuts/v1.mov", &p, &t, 1000).unwrap();
        std::fs::remove_file(root.join(&e1.proxy_rel)).unwrap();
        let e2 = generate(&root, "cuts/v1.mov", &p, &t, 2000).unwrap();
        assert_eq!(e1.source_digest, e2.source_digest);
        assert!(root.join(&e2.proxy_rel).is_file());
    }

    #[test]
    fn rejects_escapes_state_files_and_missing_media() {
        let root = setup();
        let t = CopyTranscoder;
        let p = ProxyProfile::default();
        assert!(generate(&root, "../out.mov", &p, &t, 1).is_err());
        assert!(generate(&root, "/abs.mov", &p, &t, 1).is_err());
        assert!(generate(&root, ".cairn/review.json", &p, &t, 1).is_err());
        assert!(generate(&root, "cuts/missing.mov", &p, &t, 1).is_err());
        // status of missing media: None, not an error
        assert!(status_of(&root, "cuts/missing.mov").unwrap().is_none());
    }

    #[test]
    fn corrupt_index_fails_closed() {
        let root = setup();
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        std::fs::write(index_path(&root), b"{ nope").unwrap();
        assert!(status_of(&root, "cuts/v1.mov").is_err());
    }

    #[test]
    fn digest_is_streamed_and_stable() {
        let root = setup();
        let d1 = digest_file(&root.join("cuts/v1.mov")).unwrap();
        let d2 = digest_file(&root.join("cuts/v1.mov")).unwrap();
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
    }
}
