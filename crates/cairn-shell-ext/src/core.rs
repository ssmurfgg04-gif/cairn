//! Cross-platform core of the shell extension: the root/overlay state files
//! and the context-menu command construction. Everything here is unit-tested
//! on every platform; the COM layer is a thin adapter over these types.

use std::path::{Path, PathBuf};

/// Per-root identity marker, written at attach: `<root>/.cairn/root.json`.
pub const ROOT_MARKER_DIR: &str = ".cairn";
pub const ROOT_MARKER_FILE: &str = "root.json";
/// Per-root overlay state, rewritten (best-effort) after each sync pass.
pub const OVERLAY_FILE: &str = "overlay.json";

/// The overlay states surfaced as Explorer icons (wire values are stable —
/// the JSON file format is the contract between daemon and extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    Synced,
    Conflict,
    Fetching,
    Pinned,
}

impl OverlayState {
    /// Wire tag (overlay.json values).
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayState::Synced => "synced",
            OverlayState::Conflict => "conflict",
            OverlayState::Fetching => "fetching",
            OverlayState::Pinned => "pinned",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "synced" => Some(OverlayState::Synced),
            "conflict" => Some(OverlayState::Conflict),
            "fetching" => Some(OverlayState::Fetching),
            "pinned" => Some(OverlayState::Pinned),
            _ => None,
        }
    }

    /// Icon priority — higher wins when several states could apply.
    pub fn priority(self) -> u8 {
        match self {
            OverlayState::Conflict => 4,
            OverlayState::Fetching => 3,
            OverlayState::Pinned => 2,
            OverlayState::Synced => 1,
        }
    }

    /// The icon resource name (cairn-shell-ext.dll's icon indices).
    pub fn icon_resource(self) -> &'static str {
        match self {
            OverlayState::Synced => "cairn-synced",
            OverlayState::Conflict => "cairn-conflict",
            OverlayState::Fetching => "cairn-fetching",
            OverlayState::Pinned => "cairn-pinned",
        }
    }
}

/// Parsed `<root>/.cairn/root.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    pub project_id: String,
}

impl RootInfo {
    /// Read the marker; `None` when the path is not a cairn root (Explorer
    /// calls overlays for EVERY file system — cheap early exit).
    pub fn read(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(root)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let project_id = v.get("project_id")?.as_str()?.to_string();
        if project_id.is_empty() {
            return None;
        }
        Some(Self { project_id })
    }

    /// Write the marker (attach time).
    pub fn write(root: &Path, project_id: &str) -> std::io::Result<()> {
        let dir = root.join(ROOT_MARKER_DIR);
        std::fs::create_dir_all(&dir)?;
        let body = serde_json::json!({ "project_id": project_id, "v": 1 });
        std::fs::write(Self::path(root), body.to_string())
    }

    pub fn path(root: &Path) -> PathBuf {
        root.join(ROOT_MARKER_DIR).join(ROOT_MARKER_FILE)
    }
}

/// Parsed `<root>/.cairn/overlay.json`: per-path state map + generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayStateFile {
    /// Project-relative path → state tag.
    pub files: std::collections::HashMap<String, OverlayState>,
    /// Monotonic pass counter (staleness diagnostics).
    pub generation: u64,
}

impl OverlayStateFile {
    pub fn path(root: &Path) -> PathBuf {
        root.join(ROOT_MARKER_DIR).join(OVERLAY_FILE)
    }

    /// Read + parse; a missing/corrupt file yields `None` (no icons —
    /// fail quiet, never mask with a wrong state).
    pub fn read(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(root)).ok()?;
        Self::parse(&text)
    }

    /// Parse the wire format (the daemon-side writer is
    /// [`write_state_file`] — one home for the format).
    pub fn parse(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let obj = v.as_object()?;
        let generation = obj.get("generation").and_then(|g| g.as_u64()).unwrap_or(0);
        let mut files = std::collections::HashMap::new();
        if let Some(map) = obj.get("files").and_then(|f| f.as_object()) {
            for (k, tag) in map {
                if let Some(state) = tag.as_str().and_then(OverlayState::parse) {
                    files.insert(k.clone(), state);
                }
            }
        }
        Some(Self { files, generation })
    }

    /// Serialize (roundtrips [`parse`]).
    pub fn to_json(&self) -> String {
        let mut files = serde_json::Map::new();
        for (k, v) in &self.files {
            files.insert(k.clone(), serde_json::Value::String(v.as_str().into()));
        }
        serde_json::json!({ "generation": self.generation, "files": files }).to_string()
    }

    /// State for a path, `None` when untracked.
    pub fn state_of(&self, rel: &str) -> Option<OverlayState> {
        self.files.get(rel).copied()
    }
}

/// Daemon-side writer (best-effort; called after each sync pass).
/// Keeps the last `max_entries` per-path states (the recent tail matters
/// for icons; the full row table lives in sqlite).
pub fn write_state_file(
    root: &Path,
    files: &[(String, OverlayState)],
    generation: u64,
    max_entries: usize,
) -> std::io::Result<()> {
    let dir = root.join(ROOT_MARKER_DIR);
    std::fs::create_dir_all(&dir)?;
    let mut state = OverlayStateFile {
        files: std::collections::HashMap::new(),
        generation,
    };
    // conflicts/fetching FIRST so a bounded map never drops them
    let mut ordered: Vec<&(String, OverlayState)> = files.iter().collect();
    ordered.sort_by_key(|(_, s)| std::cmp::Reverse(s.priority()));
    for (p, s) in ordered.into_iter().take(max_entries) {
        state.files.insert(p.clone(), *s);
    }
    std::fs::write(OverlayStateFile::path(root), state.to_json())
}

/// A context-menu command the extension can invoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// `cairn lock --project P --path R` — the visible write-authority pen.
    Lock { project: String, rel: String },
    /// `cairn unlock --project P --path R`.
    Unlock { project: String, rel: String },
    /// `cairn snapshot create --project P [--label L]`.
    Snapshot { project: String, label: String },
}

impl MenuAction {
    /// The `cairn` argv this action runs (the COM layer spawns the CLI from
    /// the PATH — the CLI is the audited entry point, not a hidden RPC).
    pub fn argv(&self) -> Vec<String> {
        match self {
            MenuAction::Lock { project, rel } => vec![
                "lock".into(),
                "--project".into(),
                project.clone(),
                "--path".into(),
                rel.clone(),
            ],
            MenuAction::Unlock { project, rel } => vec![
                "unlock".into(),
                "--project".into(),
                project.clone(),
                "--path".into(),
                rel.clone(),
            ],
            MenuAction::Snapshot { project, label } => {
                let mut v = vec![
                    "snapshot".into(),
                    "create".into(),
                    "--project".into(),
                    project.clone(),
                ];
                if !label.is_empty() {
                    v.push("--label".into());
                    v.push(label.clone());
                }
                v
            }
        }
    }

    /// Menu title (Explorer shows these verbatim).
    pub fn title(&self) -> &'static str {
        match self {
            MenuAction::Lock { .. } => "Cairn: Lock this file",
            MenuAction::Unlock { .. } => "Cairn: Unlock this file",
            MenuAction::Snapshot { .. } => "Cairn: Create snapshot",
        }
    }
}

/// Resolve the cairn root for an absolute path: walk up until a
/// `.cairn/root.json` marker exists (bounded depth — the workspace root
/// is the boundary). Returns (root, info).
pub fn resolve_root(abs: &Path) -> Option<(PathBuf, RootInfo)> {
    let mut cur: Option<PathBuf> = Some(abs.to_path_buf());
    for _ in 0..64 {
        let dir = cur?;
        let parent = dir.parent().map(Path::to_path_buf);
        if let Some(info) = RootInfo::read(&dir) {
            return Some((dir, info));
        }
        cur = parent;
    }
    None
}

/// Project-relative path of `abs` under `root` (forward slashes — the
/// journal's canonical separator).
pub fn rel_under(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_marker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(RootInfo::read(dir.path()).is_none(), "not a root yet");
        RootInfo::write(dir.path(), "edit-bay-2").unwrap();
        let info = RootInfo::read(dir.path()).unwrap();
        assert_eq!(info.project_id, "edit-bay-2");
    }

    #[test]
    fn overlay_state_file_roundtrip_and_priority() {
        let mut f = OverlayStateFile {
            files: std::collections::HashMap::new(),
            generation: 7,
        };
        f.files.insert("a.mov".into(), OverlayState::Conflict);
        f.files.insert("b.mov".into(), OverlayState::Synced);
        let text = f.to_json();
        let back = OverlayStateFile::parse(&text).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.state_of("a.mov"), Some(OverlayState::Conflict));
        assert_eq!(back.state_of("zzz"), None);
        // unknown tags are dropped, corrupt json fails quiet
        assert!(OverlayStateFile::parse("{not json").is_none());
        assert!(OverlayStateFile::parse("{\"files\":{\"x\":\"weird\"}}").is_some());
        // priority: conflict outranks everything
        assert!(OverlayState::Conflict.priority() > OverlayState::Fetching.priority());
        assert!(OverlayState::Fetching.priority() > OverlayState::Pinned.priority());
        assert!(OverlayState::Pinned.priority() > OverlayState::Synced.priority());
    }

    #[test]
    fn writer_keeps_conflicts_when_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, OverlayState)> = (0..50)
            .map(|i| (format!("f{i}.mov"), OverlayState::Synced))
            .chain([("conflict.mov".to_string(), OverlayState::Conflict)])
            .collect();
        write_state_file(dir.path(), &files, 3, 10).unwrap();
        let back = OverlayStateFile::read(dir.path()).unwrap();
        assert_eq!(back.files.len(), 10);
        assert_eq!(back.state_of("conflict.mov"), Some(OverlayState::Conflict));
        assert_eq!(back.generation, 3);
    }

    #[test]
    fn menu_actions_build_cairn_argv() {
        let lock = MenuAction::Lock {
            project: "p1".into(),
            rel: "footage/a.mov".into(),
        };
        assert_eq!(
            lock.argv(),
            vec!["lock", "--project", "p1", "--path", "footage/a.mov"]
        );
        let snap = MenuAction::Snapshot {
            project: "p1".into(),
            label: "before-render".into(),
        };
        assert_eq!(
            snap.argv(),
            vec![
                "snapshot",
                "create",
                "--project",
                "p1",
                "--label",
                "before-render"
            ]
        );
        let snap2 = MenuAction::Snapshot {
            project: "p1".into(),
            label: String::new(),
        };
        assert_eq!(snap2.argv().len(), 4);
    }

    #[test]
    fn resolve_root_walks_up_and_computes_rel() {
        let root = tempfile::tempdir().unwrap();
        RootInfo::write(root.path(), "proj-x").unwrap();
        let deep = root.path().join("footage/day1");
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("a.mov");
        std::fs::write(&file, b"x").unwrap();
        let (found, info) = resolve_root(&file).unwrap();
        assert!(found == root.path() || found == root.path().canonicalize().unwrap());
        assert_eq!(info.project_id, "proj-x");
        assert_eq!(rel_under(&found, &file).unwrap(), "footage/day1/a.mov");
        // outside any root → None
        let other = tempfile::tempdir().unwrap();
        assert!(resolve_root(other.path().join("nope.txt").parent().unwrap()).is_none());
    }
}
