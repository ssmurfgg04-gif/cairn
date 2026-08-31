//! Retry policy (SPEC §14, ADR-0010): full jitter, max 5, idempotent ops only for Auto-class.

use cairn_core::RetryClass;

/// Max automatic retries for Auto-class errors.
pub const MAX_AUTO_RETRIES: u32 = 5;

/// Full-jitter backoff (rclone-style; see THIRD_PARTY.md): sleep = rand(0, min(cap, base*2^n)).
#[must_use]
pub fn backoff_millis(attempt: u32, rng: &mut impl rand::Rng) -> u64 {
    let base = 100u64;
    let cap = 30_000u64;
    let exp = base.saturating_mul(1u64 << attempt.min(9));
    let bound = exp.min(cap);
    rng.gen_range(0..=bound)
}

/// Decide whether to retry: Auto class only, up to MAX_AUTO_RETRIES attempts (idempotent ops
/// only — the caller is responsible for passing idempotent closures, per §14).
#[must_use]
pub const fn should_retry(class: RetryClass, attempts_so_far: u32) -> bool {
    matches!(class, RetryClass::Auto) && attempts_so_far < MAX_AUTO_RETRIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::RetryClass as RC;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn auto_class_retries_up_to_five() {
        assert!(should_retry(RC::Auto, 0));
        assert!(should_retry(RC::Auto, 4));
        assert!(!should_retry(RC::Auto, 5));
    }

    #[test]
    fn other_classes_never_auto_retry() {
        for c in [RC::Never, RC::Conflict, RC::Server] {
            assert!(!should_retry(c, 0));
        }
    }

    #[test]
    fn backoff_is_bounded_and_jittered() {
        let mut rng = StdRng::seed_from_u64(1);
        for attempt in 0..8 {
            let b = backoff_millis(attempt, &mut rng);
            assert!(b <= 30_000);
        }
    }
}
