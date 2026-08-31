//! Path handling per SPEC §10 "filesystem landmines" and §18: UTF-8 NFC stored paths,
//! case-collision detection, ignore list, Windows reserved-name sanitization.

use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;

/// Paths are NFC-normalized for storage; original bytes are preserved for display.
#[must_use]
pub fn nfc_normalize(path: &str) -> String {
    path.nfc().collect()
}

/// AppleDouble + metadata junk never synced (default policy, SPEC §10).
pub const DEFAULT_IGNORE: &[&str] = &[
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    "._*",
    ".cairn*",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Case-folded collision key (case-insensitive FS collision detection → conflict copy).
#[must_use]
pub fn casefold_key(path: &str) -> String {
    path.to_lowercase()
}

/// Decide if a path is ignored (`.DS_Store`, `._*` AppleDouble, etc.).
#[must_use]
pub fn is_ignored(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == ".DS_Store" || name == "Thumbs.db" || name == "desktop.ini" {
        return true;
    }
    if name.starts_with("._") {
        return true;
    }
    if name.starts_with(".cairn") {
        return true;
    }
    false
}

/// Windows reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9).
pub const WIN_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Windows reserved-name sanitization (SPEC §10). Returns a display-safe name.
#[must_use]
pub fn sanitize_windows_name(name: &str) -> String {
    let stem = name.split('.').next().unwrap_or("");
    if WIN_RESERVED.contains(&stem.to_uppercase().as_str()) {
        return format!("#{name}");
    }
    name.to_string()
}

/// Long-path prefixing for Windows (`\\?\`) — applied only on Windows targets.
#[must_use]
pub fn win_long_path(path: &str) -> String {
    if cfg!(windows) && !path.starts_with("\\\\?\\") && path.len() > 240 {
        format!("\\\\?\\{path}")
    } else {
        path.to_string()
    }
}

/// Characters invalid on Windows filesystems (used for display-safe conflict-copy naming).
const WIN_INVALID: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Validate a conflict-copy name is FS-safe on all platforms (SPEC §7.1 naming rule:
/// `"name (conflict — {device} — {date}).ext"` — extension moves to the end).
#[must_use]
pub fn conflict_copy_name(original: &str, device: &str, date: &str) -> String {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if WIN_INVALID.contains(&c) || c == '/' || c == '\\' {
                    '_'
                } else {
                    c
                }
            })
            .collect()
    };
    let (stem, ext) = match original.rsplit_once('.') {
        // plausible extension: short, no separators
        Some((s, e)) if !e.is_empty() && e.len() <= 12 && !e.contains('/') => (s, Some(e)),
        _ => (original, None),
    };
    let base = format!(
        "{} (conflict — {} — {})",
        safe(stem),
        safe(device),
        safe(date)
    );
    match ext {
        Some(e) => format!("{base}.{}", safe(e)),
        None => base,
    }
}

/// Set of paths that collide case-insensitively within one directory.
#[must_use]
pub fn find_case_collisions(paths: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for p in paths {
        if !seen.insert(casefold_key(p)) {
            out.push(p.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_is_idempotent_and_stable() {
        let decomposed = "caf\u{e9}".to_string(); // é as single codepoint is already NFC
        assert_eq!(
            nfc_normalize(&decomposed),
            nfc_normalize(&nfc_normalize(&decomposed))
        );
    }

    #[test]
    fn ignore_list() {
        assert!(is_ignored(".DS_Store"));
        assert!(is_ignored("sub/._foo.mp4"));
        assert!(is_ignored(".cairn-cache"));
        assert!(!is_ignored("render.mov"));
        assert!(!is_ignored(
            "._real-work.braw".replace("._", "final-").as_str()
        ));
    }

    #[test]
    fn windows_sanitization() {
        assert_eq!(sanitize_windows_name("CON.prproj"), "#CON.prproj");
        assert_eq!(sanitize_windows_name("nul"), "#nul");
        assert_eq!(sanitize_windows_name("timeline.prproj"), "timeline.prproj");
    }

    #[test]
    fn conflict_copy_naming_per_spec() {
        let n = conflict_copy_name("scene", "dev-abc", "2026-08-31");
        assert!(n.starts_with("scene (conflict — dev-abc — 2026-08-31)"));
        let with_ext = conflict_copy_name("shot.braw", "d1", "2026-01-01");
        assert_eq!(with_ext, "shot (conflict — d1 — 2026-01-01).braw");
        let risky = conflict_copy_name("a:b.c", "d", "e");
        assert!(!risky.contains(':'));
    }

    #[test]
    fn case_collision_detection() {
        let paths = vec![
            "A/Shot.braw".to_string(),
            "a/shot.braw".to_string(),
            "b/other.mov".to_string(),
        ];
        let c = find_case_collisions(&paths);
        assert_eq!(c, vec!["a/shot.braw".to_string()]);
    }
}
