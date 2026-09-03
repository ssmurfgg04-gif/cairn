//! Windows Explorer badge layer (P1 #2): sync-status visibility.
//!
//! The badge contract, mapped to the real CfAPI surface (windows-rs 0.58):
//! - **Root-level provider status** — `CfUpdateSyncProviderStatus`
//!   (connected / syncing / disconnected / error): the sync root shows
//!   activity and health in Explorer's navigation and status areas.
//! - **Root-level sync error + CLEAR** — `CfReportSyncStatus(root, None)`
//!   clears a reported error; `CfReportSyncStatus(root, CF_SYNC_STATUS{code,
//!   description})` reports one (the mechanism behind the shell's
//!   LastSyncError view).
//! - **Per-file in-sync state** — `CfSetInSyncState` (already wired:
//!   [`crate::cfapi::mark_in_sync`] / [`mark_not_in_sync`]): the green-check
//!   vs. syncing indicator on each file.
//! - **Hydration/pin state** (cloud-only vs. locally-available icons) is the
//!   placeholder state itself — set at placeholder creation and pinning
//!   (cfapi.rs).
//!
//! Architecture: the DECISION layer is the portable state machine
//! ([`BadgeMachine`]) — pure, cross-platform, unit-tested right here on
//! Linux; the windows FFI ([`apply`]) is a thin, policy-free adapter. The
//! daemon feeds engine facts (connectivity, outbox depth, transfers in
//! flight, last error) and applies the derived directive — engine policy
//! stays in cairn-sync, badge rendering stays here.

// Same exception as cfapi.rs: the Windows-only FFI adapter necessarily
// contains `unsafe` (raw C ABI). lib.rs's `#![deny(unsafe_code)]` stays the
// rule for everything portable; this module is the audited exception.
// (Round 13: Round 12 shipped badge.rs WITHOUT this attribute -- it never
// compiled on the real windows target; the linux build never sees the
// cfg(windows) FFI, so only windows CI caught it.)
#![allow(unsafe_code)]

use std::fmt;

/// Root-level provider status for `CfUpdateSyncProviderStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderStatus {
    /// The provider is running but not currently syncing.
    Idle,
    /// An incremental sync pass is in flight.
    SyncingIncremental,
    /// A full sync (initial population / bulk recall) is in flight.
    SyncingFull,
    /// The provider lost server connectivity (transient).
    ConnectivityLost,
    /// The provider is terminated / disconnected.
    Disconnected,
}

/// What the badge layer should drive right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadgeDirective {
    pub status: ProviderStatus,
    /// Report (or clear) a root sync error.
    pub root_error: Option<RootError>,
}

/// A root-level error to report via `CfReportSyncStatus`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootError {
    pub code: u32,
    /// Human text Explorer can surface (kept ASCII — the FFI encodes UTF-16).
    pub description: String,
}

/// The engine facts the badge decision runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineFacts {
    /// Server ctl/gRPC reachable at the last probe.
    pub server_reachable: bool,
    /// Pending outbox entries (unsaved local work).
    pub outbox_pending: usize,
    /// Uploads/recalls currently in flight.
    pub transfers_in_flight: usize,
    /// The last blocking error, if any (conflict requiring a human, auth
    /// failure, quota). None = healthy.
    pub last_error: Option<RootError>,
}

/// The decision table. Pure — no I/O, no clocks: the daemon polls facts and
/// applies the directive when it CHANGES.
///
/// Priority (highest first):
/// 1. `last_error` → Error status + report the error (sticky until cleared
///    by the engine — never auto-hidden: honest state, I2).
/// 2. `!server_reachable` → ConnectivityLost (transient; no error report).
/// 3. `transfers_in_flight > 0` → SyncingIncremental/Full by bulk flag.
/// 4. `outbox_pending > 0` → Idle (work queued, not yet picked up).
/// 5. else → Idle, error cleared.
#[derive(Clone, Debug, Default)]
pub struct BadgeMachine {
    last: Option<BadgeDirective>,
}

pub enum Bulk {
    No,
    Yes,
}

impl BadgeMachine {
    pub fn new() -> BadgeMachine {
        BadgeMachine::default()
    }

    /// Derive the directive from facts (pure).
    pub fn derive(facts: &EngineFacts, bulk: Bulk) -> BadgeDirective {
        if let Some(err) = &facts.last_error {
            return BadgeDirective {
                // ERROR is provider-status DISCONNECTED-class on the root
                // plus the explicit report; Explorer distinguishes via the
                // reported code.
                status: ProviderStatus::Disconnected,
                root_error: Some(err.clone()),
            };
        }
        if !facts.server_reachable {
            return BadgeDirective {
                status: ProviderStatus::ConnectivityLost,
                root_error: None,
            };
        }
        if facts.transfers_in_flight > 0 {
            return BadgeDirective {
                status: match bulk {
                    Bulk::No => ProviderStatus::SyncingIncremental,
                    Bulk::Yes => ProviderStatus::SyncingFull,
                },
                root_error: None,
            };
        }
        BadgeDirective {
            status: ProviderStatus::Idle,
            root_error: None,
        }
    }

    /// Compute the directive AND whether it differs from the last applied
    /// one (callers skip no-op FFI round-trips). First call always applies.
    pub fn next(&mut self, facts: &EngineFacts, bulk: Bulk) -> Option<BadgeDirective> {
        let d = Self::derive(facts, bulk);
        let changed = self.last.as_ref() != Some(&d);
        if changed {
            self.last = Some(d.clone());
            Some(d)
        } else {
            None
        }
    }
}

impl fmt::Display for ProviderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderStatus::Idle => "idle",
            ProviderStatus::SyncingIncremental => "syncing",
            ProviderStatus::SyncingFull => "syncing (full)",
            ProviderStatus::ConnectivityLost => "offline",
            ProviderStatus::Disconnected => "disconnected",
        };
        f.write_str(s)
    }
}

/// The per-file badge decision (documented mapping; the calls are
/// `cfapi::mark_in_sync` / `mark_not_in_sync`):
///
/// - synced row (engine mark_synced) → IN-SYNC (green check)
/// - dirty row (write-back predicate) → NOT-in-sync (pending arrows)
///
/// The FFI for those already exists; this keeps the decision honest in one
/// place for tests and the tray app to share.
pub fn file_badge(is_synced: bool) -> FileBadge {
    if is_synced {
        FileBadge::InSync
    } else {
        FileBadge::Pending
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileBadge {
    InSync,
    Pending,
}

// ---------------------------------------------------------------------------
// windows FFI — thin, policy-free. Everything above is cross-platform.
// ---------------------------------------------------------------------------
#[cfg(windows)]
pub mod ffi {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::CloudFilters::{
        CfReportSyncStatus, CfUpdateSyncProviderStatus, CF_SYNC_PROVIDER_STATUS, CF_SYNC_STATUS,
    };

    use super::{BadgeDirective, ProviderStatus, RootError};

    pub struct BadgeConnection<'a> {
        /// The CfAPI connection key from `connect*()` — badge calls ride the
        /// SAME connection the callbacks use.
        pub connection: &'a crate::cfapi::Connection,
        /// Registered sync root path (UTF-16 buffer for report calls).
        pub root_utf16: Vec<u16>,
    }

    impl ProviderStatus {
        /// The CF_SYNC_PROVIDER_STATUS code (windows-rs 0.58 constants).
        pub fn cf_code(self) -> CF_SYNC_PROVIDER_STATUS {
            use windows::Win32::Storage::CloudFilters as cf;
            match self {
                ProviderStatus::Idle => cf::CF_PROVIDER_STATUS_IDLE,
                ProviderStatus::SyncingIncremental => cf::CF_PROVIDER_STATUS_SYNC_INCREMENTAL,
                ProviderStatus::SyncingFull => cf::CF_PROVIDER_STATUS_SYNC_FULL,
                ProviderStatus::ConnectivityLost => cf::CF_PROVIDER_STATUS_CONNECTIVITY_LOST,
                ProviderStatus::Disconnected => cf::CF_PROVIDER_STATUS_DISCONNECTED,
            }
        }
    }

    /// Apply the full directive (status + error report/clear).
    pub fn apply(conn: &BadgeConnection<'_>, directive: &BadgeDirective) -> Result<(), i32> {
        update_provider_status(conn, directive.status)?;
        match &directive.root_error {
            Some(err) => report_root_error(conn, err),
            None => clear_root_error(conn),
        }
    }

    /// `CfUpdateSyncProviderStatus` on the shared connection.
    pub fn update_provider_status(
        conn: &BadgeConnection<'_>,
        status: ProviderStatus,
    ) -> Result<(), i32> {
        // SAFETY: connection key is a plain handle value (Copy); the call is
        // synchronous and takes no pointers we own.
        unsafe {
            CfUpdateSyncProviderStatus(conn.connection.key(), status.cf_code())
                .map_err(|e| e.code().0 as i32)
        }
    }

    /// `CfReportSyncStatus(root, Some(status))` — report a named error.
    pub fn report_root_error(conn: &BadgeConnection<'_>, err: &RootError) -> Result<(), i32> {
        let mut desc: Vec<u16> = err.description.encode_utf16().collect();
        desc.push(0);
        // CF_SYNC_STATUS uses byte offsets into a trailing blob; we lay out
        // [struct][description bytes][device id bytes] in one buffer.
        let struct_size = std::mem::size_of::<CF_SYNC_STATUS>() as u32;
        let desc_off = struct_size;
        let desc_len = (desc.len() - 1) as u32 * 2; // without the NUL, in bytes
        let device_off = desc_off + desc_len;
        let device_len = 0u32;
        let status = CF_SYNC_STATUS {
            StructSize: struct_size,
            Code: err.code,
            DescriptionOffset: desc_off,
            DescriptionLength: desc_len,
            DeviceIdOffset: device_off,
            DeviceIdLength: device_len,
        };
        // SAFETY: root_utf16 is NUL-terminated by construction; the status
        // blob is fully owned by this frame for the duration of the call.
        unsafe {
            CfReportSyncStatus(
                PCWSTR(conn.root_utf16.as_ptr()),
                Some(&status as *const CF_SYNC_STATUS),
            )
            .map_err(|e| e.code().0 as i32)
        }
    }

    /// `CfReportSyncStatus(root, None)` — clear the reported error.
    pub fn clear_root_error(conn: &BadgeConnection<'_>) -> Result<(), i32> {
        // SAFETY: same as report_root_error with the null status.
        unsafe {
            CfReportSyncStatus(PCWSTR(conn.root_utf16.as_ptr()), None)
                .map_err(|e| e.code().0 as i32)
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn facts(reachable: bool, outbox: usize, inflight: usize) -> EngineFacts {
        EngineFacts {
            server_reachable: reachable,
            outbox_pending: outbox,
            transfers_in_flight: inflight,
            last_error: None,
        }
    }

    #[test]
    fn decision_table() {
        // healthy idle
        let d = BadgeMachine::derive(&facts(true, 0, 0), Bulk::No);
        assert_eq!(d.status, ProviderStatus::Idle);
        assert!(d.root_error.is_none());
        // incremental sync in flight
        let d = BadgeMachine::derive(&facts(true, 0, 2), Bulk::No);
        assert_eq!(d.status, ProviderStatus::SyncingIncremental);
        // full/bulk sync
        let d = BadgeMachine::derive(&facts(true, 0, 900), Bulk::Yes);
        assert_eq!(d.status, ProviderStatus::SyncingFull);
        // queued work but nothing in flight → idle (honest: not syncing yet)
        let d = BadgeMachine::derive(&facts(true, 7, 0), Bulk::No);
        assert_eq!(d.status, ProviderStatus::Idle);
        // offline
        let d = BadgeMachine::derive(&facts(false, 0, 0), Bulk::No);
        assert_eq!(d.status, ProviderStatus::ConnectivityLost);
        assert!(
            d.root_error.is_none(),
            "offline is transient, not an error report"
        );
        // sticky error wins over everything
        let mut ef = facts(true, 5, 5);
        ef.last_error = Some(RootError {
            code: 0x8007_0070,
            description: "disk quota".into(),
        });
        let d = BadgeMachine::derive(&ef, Bulk::Yes);
        assert_eq!(d.status, ProviderStatus::Disconnected);
        assert_eq!(d.root_error.as_ref().unwrap().code, 0x8007_0070);
    }

    #[test]
    fn change_detection_skips_noop_ffi() {
        let mut m = BadgeMachine::new();
        // first call always applies
        assert!(m.next(&facts(true, 0, 0), Bulk::No).is_some());
        // same facts → no-op
        assert!(m.next(&facts(true, 0, 0), Bulk::No).is_none());
        // status change applies
        assert!(m.next(&facts(true, 0, 1), Bulk::No).is_some());
        assert!(m.next(&facts(true, 0, 1), Bulk::No).is_none());
        // error appearing applies; error STICKS while facts say so
        let mut ef = facts(true, 0, 0);
        ef.last_error = Some(RootError {
            code: 1,
            description: "x".into(),
        });
        assert!(m.next(&ef, Bulk::No).is_some());
        assert!(m.next(&ef, Bulk::No).is_none(), "sticky");
        // error cleared by the ENGINE (not auto-hide) applies
        assert!(m.next(&facts(true, 0, 0), Bulk::No).is_some());
    }

    #[test]
    fn file_badge_mapping() {
        assert_eq!(file_badge(true), FileBadge::InSync);
        assert_eq!(file_badge(false), FileBadge::Pending);
    }

    #[test]
    fn display_is_human() {
        assert_eq!(ProviderStatus::SyncingIncremental.to_string(), "syncing");
        assert_eq!(ProviderStatus::ConnectivityLost.to_string(), "offline");
    }
}
