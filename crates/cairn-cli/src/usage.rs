//! Local-only usage counts (review #95). See the policy on
//! `dashboard::usage_count`: action names and counts only, stored in
//! `<home>/usage.json`, never transmitted.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// feature -> count, e.g. "file/open" -> 14
    pub counts: BTreeMap<String, u64>,
    /// UTC day (YYYY-MM-DD) of the first and the latest counted action
    pub first_day: Option<String>,
    pub last_day: Option<String>,
}

/// `/api/v1/file/open` -> `file/open`; `/api/v1/review/abc123/revoke`
/// -> `review/:id/revoke`. Any segment with a digit or longer than 24
/// chars is an id and is replaced, so no identifier is ever stored.
pub fn feature_of(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/v1/")?;
    if rest.is_empty() {
        return None;
    }
    let parts: Vec<&str> = rest
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| {
            if p.len() > 24 || p.bytes().any(|b| b.is_ascii_digit()) {
                ":id"
            } else {
                p
            }
        })
        .collect();
    Some(parts.join("/"))
}

fn path_of(home: &Path) -> std::path::PathBuf {
    home.join("usage.json")
}

pub fn load(home: &Path) -> Usage {
    std::fs::read(path_of(home))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    // civil-from-days (Howard Hinnant), UTC
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Count one use of `feature`. Best effort: a failed write never affects
/// the action that was counted.
pub fn bump(home: &Path, feature: &str) {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut u = load(home);
    *u.counts.entry(feature.to_owned()).or_insert(0) += 1;
    let day = today();
    if u.first_day.is_none() {
        u.first_day = Some(day.clone());
    }
    u.last_day = Some(day);
    if let Ok(bytes) = serde_json::to_vec_pretty(&u) {
        let tmp = path_of(home).with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path_of(home));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_never_carry_identifiers() {
        assert_eq!(
            feature_of("/api/v1/file/open").as_deref(),
            Some("file/open")
        );
        assert_eq!(
            feature_of("/api/v1/review/7f3a9c2e/revoke").as_deref(),
            Some("review/:id/revoke")
        );
        assert_eq!(
            feature_of("/api/v1/recall/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").as_deref(),
            Some("recall/:id")
        );
        assert_eq!(feature_of("/").as_deref(), None);
        assert_eq!(feature_of("/api/v1/").as_deref(), None);
    }

    #[test]
    fn counts_accumulate_locally() {
        let d = tempfile::tempdir().unwrap();
        bump(d.path(), "file/open");
        bump(d.path(), "file/open");
        bump(d.path(), "snapshots");
        let u = load(d.path());
        assert_eq!(u.counts["file/open"], 2);
        assert_eq!(u.counts["snapshots"], 1);
        assert_eq!(u.first_day.as_deref().map(str::len), Some(10));
        assert!(today().starts_with("20"));
    }
}
