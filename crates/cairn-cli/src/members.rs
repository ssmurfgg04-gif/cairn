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

pub fn load(root: &Path) -> anyhow::Result<MemberFile> {
    match std::fs::read(members_path(root)) {
        Ok(b) => MemberFile::from_json(&b).map_err(anyhow::Error::msg),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemberFile::default()),
        Err(e) => Err(anyhow::anyhow!("read members: {e}")),
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
}
