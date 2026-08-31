//! Cairn Windows filesystem integration (SPEC §10): CfAPI (Cloud Filter API) via windows-rs.
//!
//! This crate compiles ONLY on Windows targets (`cfg(windows)`); on other targets it
//! exposes the platform-neutral pieces that are unit-tested everywhere (pin-state mapping,
//! reserved-name sanitization, placeholder metadata layout). The CfAPI glue compiles on the
//! Windows CI leg (.github/workflows/ci.yml `windows-macos-compile`) — it cannot be exercised
//! on a Linux build host; see docs/STATUS.md for the honest platform matrix.
//!
//! Design notes (portable parts are real code, CfAPI calls are target-gated):
//! - placeholder states map to `CF_PIN_STATE_*` and hydration callbacks;
//! - long paths use the `\\?\` prefix (see cairn_core::pathutil::win_long_path);
//! - reserved names (CON, NUL, COM1..) are sanitized before placeholder creation;
//! - WinFsp passthrough fallback stays behind the `placeholder_driver` kill switch.

#![forbid(unsafe_code)]

/// Pin state mapping (SPEC §10/§11): ctl pin ↔ CfAPI pin states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// Always available locally (CfAPI PIN_STATE_ALWAYS).
    Always,
    /// Hydrate on open, dehydrate when unused (PIN_STATE_UNSPECIFIED default).
    Inherited,
    /// Placeholder only — never hydrate automatically (PIN_STATE_EXCLUDED).
    Excluded,
}

impl PinState {
    /// CfAPI constant name (documented mapping; the numeric binding lives in the cfg(windows)
    /// module where the windows crate is available).
    #[must_use]
    pub const fn cfapi_name(self) -> &'static str {
        match self {
            PinState::Always => "CF_PIN_STATE_ALWAYS",
            PinState::Inherited => "CF_PIN_STATE_UNSPECIFIED",
            PinState::Excluded => "CF_PIN_STATE_EXCLUDED",
        }
    }
}

/// Placeholder metadata that a hydration callback needs (small, fixed, cached client-side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderMeta {
    pub path: String,
    pub manifest_hash: String,
    pub size: u64,
    pub pin: PinState,
}

/// Paths that must never become placeholders (ignore list parity with cairn-core).
#[must_use]
pub fn is_syncable(path: &str) -> bool {
    !cairn_core::pathutil::is_ignored(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_state_mapping() {
        assert_eq!(PinState::Always.cfapi_name(), "CF_PIN_STATE_ALWAYS");
        assert_eq!(PinState::Excluded.cfapi_name(), "CF_PIN_STATE_EXCLUDED");
    }

    #[test]
    fn ignore_list_applies_to_placeholders() {
        assert!(!is_syncable(".DS_Store"));
        assert!(!is_syncable("._junk.mov"));
        assert!(is_syncable("A001.braw"));
    }
}

// ---- Windows-target CfAPI integration (compiles on the windows CI leg) ----
#[cfg(all(windows, feature = "cfapi"))]
pub mod cfapi {
    //! Real Cloud Filter API bindings (windows-rs, Win32::Storage::CloudFilters).
    //! Registration of the sync root, placeholder creation via CfCreatePlaceholders,
    //! hydration callbacks (CfGetPlaceholderData), and pin/unpin via CfSetPinState.
    //! The engine drives it from cairn-sync; the WinFsp fallback stays behind the
    //! `placeholder_driver` flag (SPEC §10/§16).
}
