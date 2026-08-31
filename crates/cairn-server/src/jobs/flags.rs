//! Kill switches (config_flags, SPEC §16): read PER RUN so flips take effect without
//! restart; admin actions audited.

use crate::ServerState;
use cairn_core::{CairnError, ErrorKind};

/// Canonical flag names + defaults (SPEC §16).
pub const FLAGS: &[(&str, &str)] = &[
    ("packing_enabled", "true"),
    ("tiering_enabled", "true"),
    ("delta_fold_enabled", "true"),
    ("compression_enabled", "true"),
    ("placeholder_driver", "native"), // native | winfsp (Windows fallback flag)
];

/// Read one flag (default when unset).
pub async fn get(state: &ServerState, name: &str) -> Result<String, CairnError> {
    let v: Option<String> = sqlx::query_scalar("SELECT value FROM config_flags WHERE name=?1")
        .bind(name)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("flag: {e}")))?;
    Ok(v.or_else(|| {
        FLAGS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| d.to_string())
    })
    .unwrap_or_default())
}

/// Set one flag (audited; next run picks it up — no restart).
pub async fn set(
    state: &ServerState,
    actor: &str,
    name: &str,
    value: &str,
) -> Result<(), CairnError> {
    if !FLAGS.iter().any(|(n, _)| *n == name) && name != "gc_epoch" {
        return Err(CairnError::new(
            ErrorKind::NotFound,
            format!("unknown flag {name}"),
        ));
    }
    sqlx::query(
        "INSERT INTO config_flags(name, value, updated_at) VALUES(?1,?2,?3)
         ON CONFLICT(name) DO UPDATE SET value=?2, updated_at=?3",
    )
    .bind(name)
    .bind(value)
    .bind(state.clock.now_millis())
    .execute(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("flag set: {e}")))?;
    crate::db::audit(&state.db, &state.clock, "", actor, "flag.set", name, value).await;
    Ok(())
}

/// Readiness helper: is a job's kill switch on?
pub async fn enabled(state: &ServerState, name: &str) -> Result<bool, CairnError> {
    Ok(get(state, name).await?.to_lowercase() != "false")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn flags_flip_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        assert!(enabled(&state, "packing_enabled").await.unwrap());
        set(&state, "ops", "packing_enabled", "false")
            .await
            .unwrap();
        // a NEW read (as every job run does) sees the flip immediately
        assert!(!enabled(&state, "packing_enabled").await.unwrap());
        let e = set(&state, "ops", "bogus_flag", "1").await;
        assert!(e.is_err());
    }
}
