//! Role-based access control (ADR-0020 §4): studios have hierarchies — the
//! Lead locks the timeline, the Assistant organizes bins without touching
//! the edit, the Colorist only grades, the Client only comments. The
//! permission matrix is data, enforced at every mutating cairn surface
//! (CLI now; the daemon-side gRPC hooks land with the ctl proto change).
//!
//! Membership lives in `<root>/.cairn/members.json` — a synced project
//! file like review/proxy state, owner-editable, deterministically
//! serialized. Device ids (not names) are the keys: a device's role
//! travels with the machine, and renaming a human never changes access.
//!
//! Fail-open policy for the UNLISTED case: a device absent from the file
//! is `Editor` by default (the two-person-barnstorm default cairn was
//! built for). Adding members makes the studio stricter, not looser —
//! `Owner` adds the people who should have less than full access.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Studio roles, most to least privileged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Full control, including membership + role changes.
    Owner,
    /// The creative authority: everything except membership.
    LeadEditor,
    /// Cut, lock, unlock, publish review versions.
    Editor,
    /// Organize bins/media, prep, snapshot — cannot lock or edit
    /// timelines.
    Assistant,
    /// Color nodes only: read + grade.
    Colorist,
    /// Audio: read + mix.
    SoundDesigner,
    /// Client-facing reviewer: comment + resolve through the portal.
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::LeadEditor => "lead-editor",
            Role::Editor => "editor",
            Role::Assistant => "assistant",
            Role::Colorist => "colorist",
            Role::SoundDesigner => "sound-designer",
            Role::Reviewer => "reviewer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Role::Owner),
            "lead-editor" | "lead" => Some(Role::LeadEditor),
            "editor" => Some(Role::Editor),
            "assistant" | "assist" => Some(Role::Assistant),
            "colorist" | "color" => Some(Role::Colorist),
            "sound-designer" | "sound" => Some(Role::SoundDesigner),
            "reviewer" | "client" => Some(Role::Reviewer),
            _ => None,
        }
    }
}

/// What a role may do. Granular enough for timeline-vs-bin-vs-color
/// separation without an ACL editor (KISS for studios under 50 seats).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    /// Read project files.
    Read,
    /// Write/ingest media + project files.
    WriteFiles,
    /// Organize bins, rename, move (no timeline edits).
    OrganizeBins,
    /// Claim a file/bin lock.
    LockFile,
    /// Claim the timeline lock (picture lock authority).
    LockTimeline,
    /// Edit timeline content (the cut itself).
    EditTimeline,
    /// Color grade.
    ColorGrade,
    /// Audio mix.
    MixAudio,
    /// Frame-accurate review comments + resolve.
    Comment,
    /// Publish review versions, mint guest links.
    ManageReview,
    /// Add/remove members, change roles.
    ManageMembers,
    /// Flip daemon kill switches (set_flag) — global effect on every
    /// attached project.
    ManageFlags,
    /// Attach (bind) a project root on this machine.
    AttachRoot,
    /// Detach (unbind) a project root — stops sync for everyone on this
    /// machine; the machine-level equivalent of leaving the project.
    DetachRoot,
    /// Verify integrity, snapshot, restore.
    Verify,
    Snapshot,
    Restore,
}

/// The matrix. `true` = allowed. Column order mirrors `Permission`.
/// Missing cell = `false`.
const MATRIX: &[(Role, &[Permission])] = &[
    (
        Role::Owner,
        &[
            Permission::Read,
            Permission::WriteFiles,
            Permission::OrganizeBins,
            Permission::LockFile,
            Permission::LockTimeline,
            Permission::EditTimeline,
            Permission::ColorGrade,
            Permission::MixAudio,
            Permission::Comment,
            Permission::ManageReview,
            Permission::ManageMembers,
            Permission::ManageFlags,
            Permission::AttachRoot,
            Permission::DetachRoot,
            Permission::Verify,
            Permission::Snapshot,
            Permission::Restore,
        ],
    ),
    (
        Role::LeadEditor,
        &[
            Permission::Read,
            Permission::WriteFiles,
            Permission::OrganizeBins,
            Permission::LockFile,
            Permission::LockTimeline,
            Permission::EditTimeline,
            Permission::ColorGrade,
            Permission::MixAudio,
            Permission::Comment,
            Permission::ManageReview,
            Permission::ManageFlags,
            Permission::AttachRoot,
            Permission::DetachRoot,
            Permission::Verify,
            Permission::Snapshot,
        ],
    ),
    (
        Role::Editor,
        &[
            Permission::Read,
            Permission::WriteFiles,
            Permission::OrganizeBins,
            Permission::LockFile,
            Permission::EditTimeline,
            Permission::Comment,
            Permission::ManageReview,
            Permission::AttachRoot,
            Permission::DetachRoot,
            Permission::Verify,
            Permission::Snapshot,
        ],
    ),
    (
        Role::Assistant,
        &[
            Permission::Read,
            Permission::WriteFiles,
            Permission::OrganizeBins,
            Permission::Comment,
            Permission::AttachRoot,
            Permission::Verify,
            Permission::Snapshot,
        ],
    ),
    (
        Role::Colorist,
        &[
            Permission::Read,
            Permission::WriteFiles,
            Permission::LockFile,
            Permission::ColorGrade,
            Permission::Comment,
            Permission::AttachRoot,
            Permission::Verify,
        ],
    ),
    (
        Role::SoundDesigner,
        &[
            Permission::Read,
            Permission::WriteFiles,
            Permission::LockFile,
            Permission::MixAudio,
            Permission::Comment,
            Permission::AttachRoot,
            Permission::Verify,
        ],
    ),
    (Role::Reviewer, &[Permission::Read, Permission::Comment]),
];

/// May `role` do `perm`?
pub fn allows(role: Role, perm: Permission) -> bool {
    MATRIX
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, perms)| perms.contains(&perm))
        .unwrap_or(false)
}

/// One member: a device with a role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub device_id: String,
    pub name: String,
    pub role: Role,
    pub added_at_ms: i64,
    pub added_by: String,
}

/// `<root>/.cairn/members.json` — BTreeMap for deterministic sync.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberFile {
    pub members: BTreeMap<String, Member>,
}

pub const SCHEMA: &str = "cairn-members/v1";

impl MemberFile {
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|e| format!("serialize members: {e}"))
    }

    pub fn from_json(bytes: &[u8]) -> Result<MemberFile, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("parse members: {e}"))
    }

    /// The role a device plays in this project. Unlisted devices are
    /// `Editor` (the fail-open barnstorm default; see module doc).
    pub fn role_of(&self, device_id: &str) -> Role {
        self.members
            .get(device_id)
            .map(|m| m.role)
            .unwrap_or(Role::Editor)
    }

    /// Enforcement helper: is `device_id` allowed to `perm`? (Unlisted
    /// devices get the default Editor role.)
    pub fn permits(&self, device_id: &str, perm: Permission) -> bool {
        allows(self.role_of(device_id), perm)
    }

    /// Add/replace a member (Owner action).
    pub fn upsert(&mut self, device_id: &str, name: &str, role: Role, by: &str, now_ms: i64) {
        let m = Member {
            device_id: device_id.to_string(),
            name: name.to_string(),
            role,
            added_at_ms: now_ms,
            added_by: by.to_string(),
        };
        self.members.insert(device_id.to_string(), m);
    }

    pub fn remove(&mut self, device_id: &str) -> bool {
        self.members.remove(device_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_matches_the_studio_hierarchy() {
        // Owner does everything
        for p in [
            Permission::ManageMembers,
            Permission::Restore,
            Permission::LockTimeline,
        ] {
            assert!(allows(Role::Owner, p), "owner must allow {p:?}");
        }
        // Lead: everything but membership
        assert!(allows(Role::LeadEditor, Permission::LockTimeline));
        assert!(!allows(Role::LeadEditor, Permission::ManageMembers));
        assert!(!allows(Role::LeadEditor, Permission::Restore));
        // Assistant: bins, not the edit
        assert!(allows(Role::Assistant, Permission::OrganizeBins));
        assert!(allows(Role::Assistant, Permission::Snapshot));
        assert!(!allows(Role::Assistant, Permission::LockFile));
        assert!(!allows(Role::Assistant, Permission::EditTimeline));
        // Colorist: color, not the cut
        assert!(allows(Role::Colorist, Permission::ColorGrade));
        assert!(!allows(Role::Colorist, Permission::EditTimeline));
        assert!(!allows(Role::Colorist, Permission::MixAudio));
        // Sound: audio, not color
        assert!(allows(Role::SoundDesigner, Permission::MixAudio));
        assert!(!allows(Role::SoundDesigner, Permission::ColorGrade));
        // Reviewer: read + comment only — no filesystem attach (the
        // portal is their surface)
        assert!(allows(Role::Reviewer, Permission::Comment));
        assert!(!allows(Role::Reviewer, Permission::WriteFiles));
        assert!(!allows(Role::Reviewer, Permission::ManageReview));
        assert!(!allows(Role::Reviewer, Permission::AttachRoot));
        // Editor: the cut + locks, no timeline-wide lock authority
        assert!(allows(Role::Editor, Permission::LockFile));
        assert!(allows(Role::Editor, Permission::EditTimeline));
        assert!(!allows(Role::Editor, Permission::LockTimeline));
    }

    #[test]
    fn ctl_boundary_permissions_follow_the_machine_role() {
        // attach: every creative role may bind their machine; reviewers
        // live in the portal
        for r in [
            Role::Owner,
            Role::LeadEditor,
            Role::Editor,
            Role::Assistant,
            Role::Colorist,
            Role::SoundDesigner,
        ] {
            assert!(allows(r, Permission::AttachRoot), "{r:?} must attach");
        }
        assert!(!allows(Role::Reviewer, Permission::AttachRoot));
        // detach: editor and above (an assistant cannot unbind the lead's
        // machine from the project — the daemon-guard story)
        for r in [Role::Owner, Role::LeadEditor, Role::Editor] {
            assert!(allows(r, Permission::DetachRoot), "{r:?} must detach");
        }
        for r in [
            Role::Assistant,
            Role::Colorist,
            Role::SoundDesigner,
            Role::Reviewer,
        ] {
            assert!(!allows(r, Permission::DetachRoot), "{r:?} must NOT detach");
        }
        // kill switches: owner + lead only
        assert!(allows(Role::Owner, Permission::ManageFlags));
        assert!(allows(Role::LeadEditor, Permission::ManageFlags));
        assert!(!allows(Role::Editor, Permission::ManageFlags));
        assert!(!allows(Role::Assistant, Permission::ManageFlags));
    }

    #[test]
    fn member_file_roundtrip_and_enforcement() {
        let mut f = MemberFile::default();
        f.upsert("dev-b", "Bob", Role::Assistant, "dev-a", 100);
        f.upsert("dev-c", "Carol", Role::Colorist, "dev-a", 200);
        let bytes = f.to_json().unwrap();
        let back = MemberFile::from_json(&bytes).unwrap();
        assert_eq!(back, f);

        assert_eq!(f.role_of("dev-b"), Role::Assistant);
        assert!(!f.permits("dev-b", Permission::LockFile));
        assert!(f.permits("dev-c", Permission::ColorGrade));
        // unlisted device: default Editor (fail-open barnstorm)
        assert_eq!(f.role_of("dev-zz"), Role::Editor);
        assert!(f.permits("dev-zz", Permission::EditTimeline));
        assert!(!f.permits("dev-zz", Permission::ManageMembers));
        // removal
        assert!(f.remove("dev-b"));
        assert!(!f.remove("dev-b"));
        assert_eq!(f.role_of("dev-b"), Role::Editor);
    }

    #[test]
    fn roles_parse_friendly_spellings() {
        assert_eq!(Role::parse("Lead"), Some(Role::LeadEditor));
        assert_eq!(Role::parse("client"), Some(Role::Reviewer));
        assert_eq!(Role::parse("sound"), Some(Role::SoundDesigner));
        assert_eq!(Role::parse("boss"), None);
        assert_eq!(Role::as_str(Role::SoundDesigner), "sound-designer");
    }

    #[test]
    fn corrupt_members_fail_closed() {
        assert!(MemberFile::from_json(b"{ nope").is_err());
    }
}
