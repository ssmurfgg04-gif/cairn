//! Local file-state helpers (SPEC §7.3 state machine + §5.3 `local_state`).
//!
//! Explicit, exhaustive transitions. Any transition may be interrupted; recovery is always
//! safe re-entry (WAL replay + outbox resend + BatchExists re-check).

/// Per-file local states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalState {
    /// In sync with server snapshot/journal.
    Clean,
    /// Locally modified, not yet hashed.
    Dirty,
    /// Being hashed+chunked (stable-state gate passed).
    Hashing,
    /// Chunks uploading.
    UploadPending,
    /// Journal append queued (outbox).
    OutboxPending,
    /// Fully synced.
    Synced,
    /// Hydration placeholder (not yet materialized).
    Placeholder,
    /// Pinned locally (fully materialized, eviction-exempt).
    Pinned,
    /// Conflict copy created; needs re-append for the new path.
    Conflict,
}

impl LocalState {
    /// String form used in the `files.local_state` column (SPEC §5.3).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LocalState::Clean => "clean",
            LocalState::Dirty => "dirty",
            LocalState::Hashing => "hashing",
            LocalState::UploadPending => "upload_pending",
            LocalState::OutboxPending => "outbox_pending",
            LocalState::Synced => "synced",
            LocalState::Placeholder => "placeholder",
            LocalState::Pinned => "pinned",
            LocalState::Conflict => "conflict",
        }
    }

    /// Parse from column.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "clean" => Some(LocalState::Clean),
            "dirty" => Some(LocalState::Dirty),
            "hashing" => Some(LocalState::Hashing),
            "upload_pending" => Some(LocalState::UploadPending),
            "outbox_pending" => Some(LocalState::OutboxPending),
            "synced" => Some(LocalState::Synced),
            "placeholder" => Some(LocalState::Placeholder),
            "pinned" => Some(LocalState::Pinned),
            "conflict" => Some(LocalState::Conflict),
            _ => None,
        }
    }

    /// Allowed transition table (SPEC §7.3). Exhaustive match — compiler-enforced.
    #[must_use]
    pub const fn can_transition_to(self, next: LocalState) -> bool {
        use LocalState::*;
        match (self, next) {
            (Clean, Dirty) | (Clean, Placeholder) | (Clean, Pinned) => true,
            (Dirty, Hashing) => true,
            (Hashing, UploadPending) | (Hashing, Dirty) => true, // modified during chunking → re-run
            (UploadPending, OutboxPending) | (UploadPending, Dirty) => true,
            (OutboxPending, Synced) | (OutboxPending, Dirty) => true,
            (Synced, Dirty) | (Synced, Conflict) | (Synced, Pinned) | (Synced, Placeholder) => true,
            (Placeholder, Pinned) | (Placeholder, Synced) | (Placeholder, Dirty) => true,
            (Pinned, Clean) | (Pinned, Synced) => true, // unpin
            (Conflict, Dirty) => true,                  // conflict copy re-appends as new path
            // self-transitions: idempotent re-entry (recovery paths)
            (Clean, Clean)
            | (Dirty, Dirty)
            | (Hashing, Hashing)
            | (UploadPending, UploadPending)
            | (OutboxPending, OutboxPending)
            | (Synced, Synced)
            | (Placeholder, Placeholder)
            | (Pinned, Pinned)
            | (Conflict, Conflict) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §7.3 line: clean → dirty → (hash+chunk) → (upload pending) → (outbox append) → synced
    #[test]
    fn happy_path_transitions() {
        let path = [
            (LocalState::Clean, LocalState::Dirty),
            (LocalState::Dirty, LocalState::Hashing),
            (LocalState::Hashing, LocalState::UploadPending),
            (LocalState::UploadPending, LocalState::OutboxPending),
            (LocalState::OutboxPending, LocalState::Synced),
        ];
        for (from, to) in path {
            assert!(from.can_transition_to(to), "{from:?} → {to:?}");
        }
    }

    #[test]
    fn recovery_reentry_is_always_allowed() {
        // any state may re-enter itself (interrupted transitions, WAL replay)
        for s in [
            LocalState::Clean,
            LocalState::Dirty,
            LocalState::Hashing,
            LocalState::UploadPending,
            LocalState::OutboxPending,
            LocalState::Synced,
        ] {
            assert!(s.can_transition_to(s));
        }
    }

    #[test]
    fn interruption_paths_allow_dirty_fallback() {
        assert!(LocalState::Hashing.can_transition_to(LocalState::Dirty));
        assert!(LocalState::UploadPending.can_transition_to(LocalState::Dirty));
        assert!(LocalState::OutboxPending.can_transition_to(LocalState::Dirty));
    }

    #[test]
    fn forbidden_transitions_rejected() {
        assert!(!LocalState::Clean.can_transition_to(LocalState::Synced)); // must go through pipeline
        assert!(!LocalState::Synced.can_transition_to(LocalState::Hashing));
        assert!(!LocalState::UploadPending.can_transition_to(LocalState::Synced)); // no append → no sync
    }

    #[test]
    fn string_roundtrip() {
        for s in [
            LocalState::Clean,
            LocalState::Dirty,
            LocalState::Hashing,
            LocalState::UploadPending,
            LocalState::OutboxPending,
            LocalState::Synced,
            LocalState::Placeholder,
            LocalState::Pinned,
            LocalState::Conflict,
        ] {
            assert_eq!(LocalState::from_str(s.as_str()), Some(s));
        }
        assert_eq!(LocalState::from_str("bogus"), None);
    }
}
