//! Membership CLI (ADR-0020 §4): manage `.cairn/members.json` and check
//! permissions against the RBAC matrix. The file syncs with the project;
//! enforcement lives at every root-based mutating command (review
//! publish/link check ManageReview; member edits check ManageMembers).
//! Daemon-side gRPC enforcement lands with the ctl proto change (the
//! ledger records it).

use std::path::{Path, PathBuf};

use cairn_core::clock::SystemClock as _;
use cairn_core::rbac::{MemberFile, Permission, Role};

/// `<root>/.cairn/members.json`
pub fn members_path(root: &Path) -> PathBuf {
    root.join(".cairn").join("members.json")
}

/// How a members-file read failed — the guard policy differs per shape
/// (round 27, the stale-CfAPI-reparse lesson):
///
/// * `Missing` — no file (or no root at all): the barnstorm default
///   (every device an Editor) applies. Fail-OPEN.
/// * `Unreadable` — the bytes could not even be READ: a broken CfAPI
///   reparse point, a flaky network drive, a transient filter-manager
///   HRESULT (`0x801F0005`). This is NOT corruption — the file may be
///   perfectly fine behind a wedged placeholder. Fail-open for
///   read-shaped commands; the audit ledger records the bypass.
/// * `Corrupt` — the bytes read but the JSON does not parse: a genuinely
///   corrupt members file. Fail-CLOSED (parse error propagates).
#[derive(Debug)]
pub enum LoadError {
    /// File or root absent — default Editor applies.
    Missing,
    /// IO read failure (reparse/filter/transient) — not corruption.
    Unreadable(String),
    /// Parse failure — corruption. Fail closed.
    Corrupt(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Missing => write!(f, "no members file"),
            LoadError::Unreadable(e) => write!(f, "members unreadable (transient/reparse): {e}"),
            LoadError::Corrupt(e) => write!(f, "members corrupt: {e}"),
        }
    }
}

/// Classify a read failure into the three policy shapes. A missing ROOT
/// (detach after the folder was deleted, a broken reparse whose
/// `is_dir()` is false) reads as `Missing` — there is no authority to
/// enforce, so the default Editor applies.
fn classify_read_err(root: &Path, e: &std::io::Error) -> LoadError {
    if e.kind() == std::io::ErrorKind::NotFound {
        return LoadError::Missing;
    }
    // The root itself gone/unreadable: no authority exists on disk.
    if !root.is_dir() {
        return LoadError::Missing;
    }
    // Windows: a wedged CfAPI placeholder surfaces as raw OS errors
    // (HRESULT -2145452027 / 0x801F0005 "invalid name request",
    // ERROR_CLOUD_FILE_INVALID_REQUEST, or ERROR_ACCESS_DENIED while the
    // filter reconnects). Anything we cannot READ is unproven — treat as
    // transient, never as corruption: only a PARSE failure proves the
    // bytes are wrong.
    LoadError::Unreadable(format!("{e}"))
}

/// Load with policy classification: `Ok(file)` means the file READ and
/// parsed. `Err(Missing)` (absent file/root — the barnstorm default),
/// `Err(Unreadable)` (transient/reparse IO — fail-open with warning)
/// and `Err(Corrupt)` (fail-closed) are the three policy shapes
/// callers translate per-action.
pub fn load_classified(root: &Path) -> Result<MemberFile, LoadError> {
    match std::fs::read(members_path(root)) {
        Ok(b) => MemberFile::from_json(&b).map_err(LoadError::Corrupt),
        Err(e) => Err(classify_read_err(root, &e)),
    }
}

/// Strict load (fail-closed on corrupt, fail-open on absent):
/// the CLI-side guard. An UNREADABLE file is an error here — the CLI
/// runs on the user's box where a wedged root should be surfaced, not
/// silently bypassed (the daemon-side guard has the richer policy).
pub fn load(root: &Path) -> anyhow::Result<MemberFile> {
    match load_classified(root) {
        Ok(f) => Ok(f),
        Err(LoadError::Missing) => Ok(MemberFile::default()),
        Err(LoadError::Corrupt(e)) => Err(anyhow::anyhow!("{e}")),
        Err(LoadError::Unreadable(e)) => Err(anyhow::anyhow!(
            "{e} — the folder may be a broken sync root; try `cairn detach` then re-attach"
        )),
    }
}

fn save(root: &Path, f: &MemberFile) -> anyhow::Result<()> {
    let json = f.to_json().map_err(anyhow::Error::msg)?;
    cairn_proxy::pipeline::atomic_write(&members_path(root), &json).map_err(anyhow::Error::msg)
}

/// The acting device: `--as <device-id>` flag, else CAIRN_DEVICE env,
/// else "local" (which, when unlisted, is the default Editor).
pub fn acting_device(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("CAIRN_DEVICE").ok())
        .unwrap_or_else(|| "local".into())
}

/// Guard a mutating command: load members, check `perm` for `device`,
/// refuse with a clear message otherwise.
pub fn guard(root: &Path, device: &str, perm: Permission) -> anyhow::Result<MemberFile> {
    let f = load(root)?;
    if !f.permits(device, perm) {
        let role = f.role_of(device);
        anyhow::bail!(
            "role '{}' may not {:?} — ask the owner to change your role in \
             .cairn/members.json",
            role.as_str(),
            perm
        );
    }
    Ok(f)
}

/// `cairn member add` (Owner/Lead action — matrix: ManageMembers is
/// Owner-only; leads get a clear refusal).
pub fn cmd_add(
    root: &Path,
    device: &str,
    name: &str,
    role: Role,
    as_device: Option<&str>,
) -> anyhow::Result<()> {
    let actor = acting_device(as_device);
    let mut f = guard(root, &actor, Permission::ManageMembers)?;
    f.upsert(
        device,
        name,
        role,
        &actor,
        cairn_core::clock::WallClock.now_millis(),
    );
    save(root, &f)?;
    println!("{device} ({name}) -> {}", role.as_str());
    Ok(())
}

/// `cairn member remove`.
pub fn cmd_remove(root: &Path, device: &str, as_device: Option<&str>) -> anyhow::Result<()> {
    let actor = acting_device(as_device);
    let mut f = guard(root, &actor, Permission::ManageMembers)?;
    if !f.remove(device) {
        anyhow::bail!("{device} is not a member");
    }
    save(root, &f)?;
    println!("removed {device}");
    Ok(())
}

/// `cairn member list` — includes the implicit default row.
pub fn cmd_list(root: &Path) -> anyhow::Result<()> {
    let f = load(root)?;
    if f.members.is_empty() {
        println!("no members file — every device is 'editor' by default (fail-open barnstorm)");
        return Ok(());
    }
    for m in f.members.values() {
        println!(
            "{:<14} {:<14} {}",
            m.device_id.chars().take(14).collect::<String>(),
            m.role.as_str(),
            m.name
        );
    }
    println!("(unlisted devices default to 'editor')");
    Ok(())
}

/// `cairn member check --device X --perm <kebab>` — exit-1 refusal answer
/// for scripts and future daemon hooks.
pub fn cmd_check(root: &Path, device: &str, perm: &str) -> anyhow::Result<bool> {
    let f = load(root)?;
    let p = parse_perm(perm).ok_or_else(|| anyhow::anyhow!("unknown permission: {perm}"))?;
    let ok = f.permits(device, p);
    println!(
        "{device} ({}) {:<14} -> {}",
        f.role_of(device).as_str(),
        perm,
        if ok { "ALLOW" } else { "DENY" }
    );
    Ok(ok)
}

fn parse_perm(s: &str) -> Option<Permission> {
    #[allow(clippy::enum_glob_use)] // terse mapping table
    use Permission::*;
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "read" => Read,
        "write-files" => WriteFiles,
        "organize-bins" => OrganizeBins,
        "lock-file" => LockFile,
        "lock-timeline" => LockTimeline,
        "edit-timeline" => EditTimeline,
        "color-grade" => ColorGrade,
        "mix-audio" => MixAudio,
        "comment" => Comment,
        "manage-review" => ManageReview,
        "manage-members" => ManageMembers,
        "verify" => Verify,
        "snapshot" => Snapshot,
        "restore" => Restore,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    #[test]
    fn member_crud_enforces_owner_gate() {
        let root = tmp();
        // unlisted actor = editor: may NOT manage members
        assert!(cmd_add(&root, "dev-b", "Bob", Role::Assistant, None).is_err());

        // seed an owner via the file itself (bootstrap: first owner is
        // whoever creates the file)
        let mut f = MemberFile::default();
        f.upsert("dev-a", "Alice", Role::Owner, "bootstrap", 1);
        save(&root, &f).unwrap();

        std::env::remove_var("CAIRN_DEVICE");
        cmd_add(&root, "dev-b", "Bob", Role::Assistant, Some("dev-a")).unwrap();
        cmd_add(&root, "dev-c", "Carol", Role::Colorist, Some("dev-a")).unwrap();
        let listed = load(&root).unwrap();
        assert_eq!(listed.members.len(), 3);

        // lead cannot manage members either
        cmd_add(&root, "dev-l", "Lead", Role::LeadEditor, Some("dev-a")).unwrap();
        assert!(cmd_add(&root, "dev-d", "D", Role::Editor, Some("dev-l")).is_err());

        // check helper
        assert!(cmd_check(&root, "dev-c", "color-grade").unwrap());
        assert!(!cmd_check(&root, "dev-c", "edit-timeline").unwrap());
        assert!(cmd_check(&root, "dev-c", "nope").is_err());

        // removal
        cmd_remove(&root, "dev-b", Some("dev-a")).unwrap();
        assert!(cmd_remove(&root, "dev-b", Some("dev-a")).is_err());
    }

    #[test]
    fn load_classifies_missing_corrupt_and_unreadable() {
        let root = tmp();
        // 1) absent file + absent root: Missing (fail-open default) —
        // an Err VARIANT, not Ok: the caller decides the policy
        assert!(matches!(load_classified(&root), Err(LoadError::Missing)));
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        assert!(matches!(load_classified(&root), Err(LoadError::Missing)));

        // 2) garbage bytes: Corrupt (fail-closed) — the only hard failure
        std::fs::write(members_path(&root), b"{not json").unwrap();
        match load_classified(&root) {
            Err(LoadError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }

        // 3) strict load surfaces corrupt as an error, absent as default
        std::fs::remove_file(members_path(&root)).unwrap();
        assert!(load(&root).unwrap().members.is_empty());

        // 4) an unreadable file on a live root classifies Unreadable
        //    (simulate with a directory in the file's place: EISDIR)
        std::fs::create_dir_all(members_path(&root)).unwrap();
        match load_classified(&root) {
            Err(LoadError::Unreadable(_)) => {}
            other => panic!("expected Unreadable, got {other:?}"),
        }
        // ...and the CLI-shaped load turns that into the detach hint
        let msg = format!("{}", load(&root).unwrap_err());
        assert!(
            msg.contains("cairn detach"),
            "hint should name detach: {msg}"
        );
    }
}
