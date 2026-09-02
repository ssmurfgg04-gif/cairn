//! Domain decomposition config (ADR-0014 Phase 2): per-subproject lease scoping.
//!
//! A project MAY ship a `.cairn-domains` file in its synced project root — one
//! subproject root per line (e.g. `sequences/A001`, `gfx`). It is an ORDINARY synced
//! file: config propagates to every device through the normal sync engine, no wire
//! or server change (clients resolve scopes deterministically from identical state).
//!
//! Semantics: a file under a declared domain shares ONE lease row with everything
//! else in that domain (the domain is the state boundary — a whole sequence's state
//! moves together, so one pen per sequence is the honest granularity); files outside
//! all domains lease per-file (Phase 3 behavior, unchanged). Two editors in disjoint
//! domains never see each other's pens — the >90% collision reduction the ADR
//! projects, now enforced by config instead of team discipline.
//!
//! Parsing is LENIENT by design: unknown/bad lines are skipped, and a missing file
//! means "no domains" (per-file). A teammate pushing a half-baked config must never
//! wedge a mount. Accepted lines: non-empty, not `#`-comment, relative, no `..`,
//! no drive letters, trailing slashes tolerated, `\\` normalized to `/`.

use std::path::Path;

/// Declared subproject roots, longest-first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Domains {
    roots: Vec<String>,
}

/// Normalize one candidate root; None = invalid line (skip with a warning upstream).
fn normalize_root(line: &str) -> Option<String> {
    let mut s = line.trim().replace('\\', "/");
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    if s.is_empty() || s.starts_with('#') || s.starts_with('/') || s.contains("//") {
        return None;
    }
    // drive letters (Windows-authoring mistake — a relative root never has one)
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return None;
    }
    let comps: Vec<&str> = s.split('/').collect();
    if comps
        .iter()
        .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return None;
    }
    Some(s)
}

impl Domains {
    /// Parse the `.cairn-domains` content. Lenient: invalid lines are skipped.
    pub fn parse(content: &str) -> Domains {
        let mut roots: Vec<String> = Vec::new();
        for line in content.lines() {
            if let Some(root) = normalize_root(line) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        // longest-first so `sequences/A/sub` wins over `sequences/A`
        roots.sort_by_key(|r| std::cmp::Reverse(r.split('/').count()));
        Domains { roots }
    }

    /// Load from a synced project dir (missing file → no domains → per-file leases).
    pub fn from_dir(project_dir: &Path) -> Domains {
        match std::fs::read_to_string(project_dir.join(".cairn-domains")) {
            Ok(content) => Domains::parse(&content),
            Err(_) => Domains::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Declared roots (longest-first). Test/inspection surface.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// The lease scope for `path`: the LONGEST declared root that contains it
    /// (component-boundary match — `sequences/A` does not swallow
    /// `sequences/A111/x`), or the path itself when no domain applies.
    pub fn scope_for(&self, path: &str) -> String {
        let p = path.trim().replace('\\', "/");
        let p = p.trim_start_matches('/');
        for root in &self.roots {
            if p == root || p.starts_with(&format!("{root}/")) {
                return root.clone();
            }
        }
        p.to_string()
    }
}

/// One-shot convenience: parse `content` and resolve `path` (used by call sites
/// that re-read the synced config per decision — opens are rare vs I/O, so the
/// re-parse costs nothing and eliminates cache-invalidation bugs).
pub fn resolve(content: &str, path: &str) -> String {
    Domains::parse(content).scope_for(path)
}

/// Load-and-resolve from a synced project dir.
pub fn resolve_from_dir(project_dir: &Path, path: &str) -> String {
    Domains::from_dir(project_dir).scope_for(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_lenient_and_dedupes() {
        let d = Domains::parse(
            "# comment\n\nsequences/A001\n gfx \nsequences/A001/SHOTS\n..\\escape\n/etc/passwd\nC:\\temp\nsequences//x\nsequences/A001/SHOTS\n",
        );
        assert_eq!(
            d.roots(),
            &[
                "sequences/A001/SHOTS".to_string(),
                "sequences/A001".to_string(),
                "gfx".to_string()
            ]
        );
    }

    #[test]
    fn longest_root_wins_and_component_boundaries_hold() {
        let d = Domains::parse("sequences/A\nsequences/A/sub\n");
        assert_eq!(
            d.scope_for("sequences/A/sub/shot.prproj"),
            "sequences/A/sub"
        );
        assert_eq!(d.scope_for("sequences/A/scene.prproj"), "sequences/A");
        // component boundary: A111 is NOT under A
        assert_eq!(
            d.scope_for("sequences/A111/other.prproj"),
            "sequences/A111/other.prproj"
        );
    }

    #[test]
    fn unmatched_paths_lease_per_file() {
        let d = Domains::parse("sequences/A\n");
        assert_eq!(d.scope_for("audio/vo/take3.wav"), "audio/vo/take3.wav");
        assert_eq!(d.scope_for("sequences/A"), "sequences/A"); // the root file itself
    }

    #[test]
    fn empty_or_missing_config_means_per_file() {
        assert!(Domains::default().is_empty());
        assert_eq!(Domains::parse("").scope_for("a/b.prproj"), "a/b.prproj");
        assert_eq!(resolve("", "x/y.prproj"), "x/y.prproj");
    }

    #[test]
    fn resolve_end_to_end_and_windows_authored_roots() {
        // authored on Windows, mounted on Linux: backslashes normalize
        let content = "sequences\\A001\r\n";
        assert_eq!(
            resolve(content, "sequences/A001/shot/scene.prproj"),
            "sequences/A001"
        );
        assert_eq!(resolve(content, "other/file.bin"), "other/file.bin");
    }

    #[test]
    fn from_dir_missing_file_is_per_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Domains::from_dir(dir.path()).is_empty());
        std::fs::write(dir.path().join(".cairn-domains"), "gfx\n").unwrap();
        assert_eq!(
            Domains::from_dir(dir.path()).scope_for("gfx/tex.png"),
            "gfx"
        );
    }
}
