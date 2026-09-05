//! Persistence for the review portal: `.cairn/review.json` (session) and
//! `.cairn/review-notes/v{N}.json` (per-version comment sets).
//!
//! Both are plain deterministic JSON inside the project root. HONEST
//! (ADR-0022): they live under `.cairn/`, which the sync scan ignore-lists —
//! per-machine today, the synced-review-state follow-up is named in the ADR.
//! The portal
//! state converges with the same journal/merge machinery as the rest of
//! the project (zero new transport code).
//!
//! Writes are atomic (tmp + rename), reads fail closed on corrupt input,
//! and comment sets load through the cairn-tl NoteSet machinery so the
//! round-14 three-way note merge applies to client comments unchanged.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use cairn_tl::notes::{Note, NoteAnchor, NoteKind, NoteSet, NoteVisibility};

use crate::model::ReviewFile;

/// `<root>/.cairn/review.json`
pub fn session_path(root: &Path) -> PathBuf {
    root.join(".cairn").join("review.json")
}

/// `<root>/.cairn/review-notes/v{N}.json`
pub fn comment_path(root: &Path, version: u32) -> PathBuf {
    root.join(".cairn")
        .join("review-notes")
        .join(format!("v{version}.json"))
}

/// Atomic write: serialize to `<path>.tmp`, fsync, rename over the target.
/// A crash mid-write never leaves a half-written review file for the sync
/// engine to journal.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

/// Store operations over a project root.
#[derive(Clone, Copy, Debug)]
pub struct Store;

/// The compose envelope for [`Store::add_note`] (ADR-0028). Every field
/// at its default is the plain v1 shape: a point comment, public, no
/// attachment. The writer — not the reader — bears the version cost.
#[derive(Clone, Debug)]
pub struct NoteDraft {
    pub kind: NoteKind,
    /// Inclusive end frame; `None` (or equal to the start) is a point note.
    pub range_end: Option<u64>,
    /// Normalized on-frame position, clamped to 0.0..=1.0 on write.
    pub pin: Option<(f32, f32)>,
    /// BLAKE3 hex of an overlay blob already in the project CAS.
    pub attachment: Option<String>,
    pub visibility: NoteVisibility,
}

impl Default for NoteDraft {
    fn default() -> Self {
        NoteDraft {
            kind: NoteKind::Comment,
            range_end: None,
            pin: None,
            attachment: None,
            visibility: NoteVisibility::Public,
        }
    }
}

impl Store {
    /// Load the session file; `None` when no review exists yet (portal
    /// not opened for this project).
    pub fn load(root: &Path) -> Result<Option<ReviewFile>, String> {
        let p = session_path(root);
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("read {}: {e}", p.display())),
        };
        ReviewFile::from_json(&bytes).map(Some)
    }

    /// Persist the session file atomically.
    pub fn save(root: &Path, f: &ReviewFile) -> Result<(), String> {
        atomic_write(&session_path(root), &f.to_json()?)
    }

    /// Load one version's comment set (empty when none yet).
    pub fn load_comments(root: &Path, version: u32) -> Result<NoteSet, String> {
        let p = comment_path(root, version);
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NoteSet::default()),
            Err(e) => return Err(format!("read {}: {e}", p.display())),
        };
        NoteSet::from_json(&bytes)
    }

    /// Persist one version's comment set atomically.
    pub fn save_comments(root: &Path, version: u32, set: &NoteSet) -> Result<(), String> {
        atomic_write(&comment_path(root, version), &set.to_json()?)
    }

    /// Append a comment to a version's set. Ids are content-derived
    /// (blake3 of anchor|body|author), so an identical re-submit from a
    /// flaky browser is a no-op, and the same comment made on two
    /// offline machines converges to one entry after sync.
    ///
    /// Round 20: the load-modify-save runs under the version's advisory
    /// file lock — two guests commenting the same version concurrently used
    /// to silently drop the slower comment (last writer wins). The lock is
    /// a lockfile + bounded spin (5 s): portal scale, not database scale,
    /// and a dead holder self-clears via O_EXCL retry + steal.
    pub fn add_comment(
        root: &Path,
        version: u32,
        author: &str,
        body: &str,
        frame: u64,
        rate: i128,
        created_ms: i64,
    ) -> Result<Note, String> {
        Self::add_note(
            root,
            version,
            author,
            body,
            frame,
            rate,
            created_ms,
            NoteDraft::default(),
        )
    }

    /// The v2 compose path (ADR-0028 §A): the FIRST v2 writer. A draft at
    /// its defaults writes the plain v1 shape (same id, smallest bytes);
    /// any v2 feature — range, pin, annotation attachment, internal
    /// visibility, non-comment kind — switches the note to the versioned
    /// v2 id material. Range ends are validated inclusive (end >= frame).
    pub fn add_note(
        root: &Path,
        version: u32,
        author: &str,
        body: &str,
        frame: u64,
        rate: i128,
        created_ms: i64,
        draft: NoteDraft,
    ) -> Result<Note, String> {
        let range = match draft.range_end {
            Some(end) if end >= frame => Some((i128::from(frame), i128::from(end))),
            Some(end) => return Err(format!("range end {end} before start frame {frame}")),
            None => None,
        };
        let pin = draft
            .pin
            .map(|(x, y)| (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
        let _guard = VersionLock::acquire(root, version)?;
        let mut set = Self::load_comments(root, version)?;
        let note = Note::with_envelope(
            author,
            body,
            NoteAnchor {
                clip: None,
                frame: i128::from(frame),
                rate,
                range,
            },
            cairn_tl::notes::NoteStatus::Open,
            created_ms,
            draft.kind,
            pin,
            draft.attachment,
            draft.visibility,
        );
        set.notes.insert(note.id.clone(), note.clone());
        Self::save_comments(root, version, &set)?;
        Ok(note)
    }

    /// Resolve (or re-open) a comment by id.
    pub fn set_status(
        root: &Path,
        version: u32,
        id: &str,
        status: cairn_tl::notes::NoteStatus,
    ) -> Result<(), String> {
        let _guard = VersionLock::acquire(root, version)?;
        let mut set = Self::load_comments(root, version)?;
        let note = set
            .notes
            .get_mut(id)
            .ok_or_else(|| format!("no comment {id} in v{version}"))?;
        note.status = status;
        Self::save_comments(root, version, &set)
    }
}

/// Advisory lock over a version's note file (`.cairn/review-notes/vN.lock`):
/// O_EXCL create + bounded spin; a stale lock (holder died) is stolen after
/// 5 s. Correctness for the portal's 2-writer race, not a database.
struct VersionLock {
    path: std::path::PathBuf,
}

impl VersionLock {
    fn acquire(root: &Path, version: u32) -> Result<VersionLock, String> {
        let dir = root.join(".cairn").join("review-notes");
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
        let path = dir.join(format!("v{version}.lock"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(VersionLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // stale? (older than 5s — a holder that died mid-write)
                    if let Ok(meta) = std::fs::metadata(&path) {
                        if let Ok(modified) = meta.modified() {
                            if modified.elapsed().unwrap_or_default()
                                > std::time::Duration::from_secs(5)
                            {
                                let _ = std::fs::remove_file(&path);
                                continue;
                            }
                        }
                    }
                    if std::time::Instant::now() > deadline {
                        // steal rather than fail the guest's comment
                        let _ = std::fs::remove_file(&path);
                        let _ = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path);
                        return Ok(VersionLock { path });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => return Err(format!("lock: {e}")),
            }
        }
    }
}

impl Drop for VersionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReviewVersion;

    fn tmp_root() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    fn version() -> ReviewVersion {
        ReviewVersion {
            number: 0,
            label: "v1".into(),
            media_rel: "cuts/v1.mp4".into(),
            proxy_rel: None,
            fps_num: 24,
            fps_den: 1,
            frames: 100,
            timeline_fingerprint: None,
            snapshot: None,
            published_by: "a".into(),
            published_at: 1,
        }
    }

    #[test]
    fn session_roundtrip_and_missing_root() {
        let root = tmp_root();
        assert!(Store::load(&root).unwrap().is_none());
        let mut f = crate::model::ReviewFile {
            title: "T".into(),
            ..Default::default()
        };
        f.publish(version());
        Store::save(&root, &f).unwrap();
        assert_eq!(Store::load(&root).unwrap().unwrap(), f);
        // no tmp litter
        assert!(!root.join(".cairn").join("review.json.tmp").exists());
    }

    #[test]
    fn corrupt_session_fails_closed() {
        let root = tmp_root();
        fs::create_dir_all(root.join(".cairn")).unwrap();
        fs::write(session_path(&root), b"{ not json").unwrap();
        assert!(Store::load(&root).is_err());
    }

    #[test]
    fn comments_roundtrip_and_dedupe_by_content() {
        let root = tmp_root();
        let n1 = Store::add_comment(&root, 1, "jane", "tighten the cut here", 42, 24, 100).unwrap();
        // same content again: id-derived dedupe, still one note
        let n2 = Store::add_comment(&root, 1, "jane", "tighten the cut here", 42, 24, 200).unwrap();
        assert_eq!(n1.id, n2.id);
        let set = Store::load_comments(&root, 1).unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set.notes.values().next().unwrap().anchor.frame, 42);
        // different body -> different id
        let n3 = Store::add_comment(&root, 1, "jane", "also fix color", 42, 24, 300).unwrap();
        assert_ne!(n1.id, n3.id);
        assert_eq!(Store::load_comments(&root, 1).unwrap().len(), 2);
    }

    #[test]
    fn resolve_and_reopen_status() {
        let root = tmp_root();
        let n = Store::add_comment(&root, 2, "bob", "music too loud", 10, 24, 1).unwrap();
        use cairn_tl::notes::NoteStatus;
        Store::set_status(&root, 2, &n.id, NoteStatus::Resolved).unwrap();
        let set = Store::load_comments(&root, 2).unwrap();
        assert_eq!(set.notes[&n.id].status, NoteStatus::Resolved);
        // unknown id -> error, not panic
        assert!(Store::set_status(&root, 2, "nope", NoteStatus::Resolved).is_err());
    }

    #[test]
    fn comment_paths_are_version_scoped() {
        let root = tmp_root();
        Store::add_comment(&root, 1, "a", "x", 1, 24, 1).unwrap();
        Store::add_comment(&root, 2, "a", "x", 1, 24, 1).unwrap();
        assert!(comment_path(&root, 1).exists());
        assert!(comment_path(&root, 2).exists());
        assert_eq!(Store::load_comments(&root, 3).unwrap().len(), 0);
    }
}
