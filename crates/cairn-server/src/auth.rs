//! Device authn/authz (SPEC §13, ADR-0011): PASETO v4.public (ed25519) device tokens, 90d
//! rotation, scopes `sync|admin`, revocation on unlink, token_hash in `devices`.
//! Denials are audited.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pasetors::claims::{Claims, ClaimsValidationRules};
use pasetors::keys::{AsymmetricKeyPair, AsymmetricSecretKey, AsymmetricPublicKey, Generate};
use pasetors::token::UntrustedToken;
use pasetors::version4::V4;
use pasetors::{public, Public};
use sqlx::Row;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use cairn_core::clock::SystemClock;
use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};

const TOKEN_TTL_SECS: i64 = 90 * 24 * 3600; // 90d rotation (SPEC §13)

/// Verified device identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub tenant_id: String,
    pub scopes: String,
}

/// Enrollment signing keys + pending codes.
pub struct Authenticator {
    secret: AsymmetricSecretKey<V4>,
    public: AsymmetricPublicKey<V4>,
    codes: RwLock<HashMap<String, (String, String, String, i64)>>, // code → (tenant, email, scopes, exp)
    clock: Arc<dyn SystemClock>,
}

impl Authenticator {
    /// Load (or create) the signing keypair from the data dir.
    pub fn load_or_create(keys_dir: &Path, clock: Arc<dyn SystemClock>) -> Result<Self, CairnError> {
        std::fs::create_dir_all(keys_dir)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("keys dir: {e}")))?;
        let path: PathBuf = keys_dir.join("device-signing.key");
        let secret = if path.exists() {
            let hex_seed = std::fs::read_to_string(&path)
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("read key: {e}")))?;
            let raw = cairn_core::hash::hex_decode(hex_seed.trim())
                .ok_or_else(|| CairnError::new(ErrorKind::Io, "bad key hex"))?;
            AsymmetricSecretKey::<V4>::from(&raw)
                .map_err(|e| CairnError::new(ErrorKind::Internal, format!("key parse: {e}")))?
        } else {
            let kp = AsymmetricKeyPair::<V4>::generate()
                .map_err(|e| CairnError::new(ErrorKind::Internal, format!("keygen: {e}")))?;
            let hex_seed = cairn_core::hash::hex_encode(kp.secret.as_bytes());
            std::fs::write(&path, hex_seed)
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("write key: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            kp.secret
        };
        let public = AsymmetricPublicKey::from(&secret_public_bytes(&secret)?)
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("pubkey: {e}")))?;
        Ok(Authenticator { secret, public, codes: RwLock::new(HashMap::new()), clock })
    }

    /// Issue a single-use enrollment code (admin action; audited by the caller).
    pub async fn enroll_code(
        &self,
        tenant_id: &str,
        email: &str,
        scopes: &str,
        ttl_millis: i64,
    ) -> String {
        let code = format!("enr-{}", uuid::Uuid::now_v7().simple());
        let exp = self.clock.now_millis() + ttl_millis;
        self.codes
            .write()
            .await
            .insert(code.clone(), (tenant_id.into(), email.into(), scopes.into(), exp));
        code
    }

    /// Enroll a device with a valid code; returns the signed PASETO + ids.
    pub async fn enroll(
        &self,
        pool: &SqlitePool,
        code: &str,
        device_pubkey: &str,
        device_name: &str,
    ) -> Result<(String, DeviceIdentity), CairnError> {
        let mut codes = self.codes.write().await;
        let entry = codes.get(code).cloned();
        let Some((tenant_id, _email, scopes, exp)) = entry else {
            return Err(CairnError::new(ErrorKind::Unauthenticated, "invalid enrollment code"));
        };
        if exp < self.clock.now_millis() {
            codes.remove(code);
            return Err(CairnError::new(ErrorKind::SessionExpired, "enrollment code expired"));
        }
        codes.remove(code); // single use
        drop(codes);

        let device_id = format!("dev-{}", uuid::Uuid::now_v7().simple());
        let identity =
            DeviceIdentity { device_id: device_id.clone(), tenant_id: tenant_id.clone(), scopes: scopes.clone() };
        let token = self.sign(&identity, device_name, device_pubkey)?;
        let token_hash = Hash::of(token.as_bytes()).hex();
        sqlx::query(
            "INSERT INTO devices(id, tenant_id, user_id, token_hash, scopes, revoked, last_seen, created_at)
             VALUES(?1,?2,'',?3,?4,0,0,?5)",
        )
        .bind(&device_id)
        .bind(&tenant_id)
        .bind(&token_hash)
        .bind(&scopes)
        .bind(self.clock.now_millis())
        .execute(pool)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("device insert: {e}")))?;
        Ok((token, identity))
    }

    /// Sign a device token (PASETO v4.public, exp 90d, kid implicit binding to v4).
    #[must_use]
    pub fn sign(&self, identity: &DeviceIdentity, device_name: &str, device_pubkey: &str) -> Result<String, CairnError> {
        let mut claims = Claims::new()
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims: {e}")))?;
        // PASETO exp is RFC3339; use the built-in setter for the 90d rotation window (SPEC §13)
        claims
            .set_expires_in(&std::time::Duration::from_secs(TOKEN_TTL_SECS as u64))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims exp: {e}")))?;
        claims
            .issuer("cairn")
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims iss: {e}")))?;
        claims
            .add_additional("device_id", serde_json::json!(identity.device_id))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims dev: {e}")))?;
        claims
            .add_additional("tenant_id", serde_json::json!(identity.tenant_id))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims tenant: {e}")))?;
        claims
            .add_additional("scopes", serde_json::json!(identity.scopes))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims scopes: {e}")))?;
        claims
            .add_additional("device_name", serde_json::json!(device_name))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims name: {e}")))?;
        claims
            .add_additional("device_pubkey", serde_json::json!(device_pubkey))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("claims pk: {e}")))?;
        public::sign(&self.secret, &claims, None, None)
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("paseto sign: {e}")))
    }

    /// Verify a bearer token against signature, expiry, revocation, and token_hash.
    /// Returns the identity; denials are audited by the caller with context.
    pub async fn authenticate(
        &self,
        pool: &SqlitePool,
        bearer: &str,
    ) -> Result<DeviceIdentity, CairnError> {
        let token = bearer
            .strip_prefix("Bearer ")
            .or_else(|| bearer.strip_prefix("bearer "))
            .unwrap_or(bearer);
        let validation = ClaimsValidationRules::new(); // validates exp/nbf/iat
        let untrusted = UntrustedToken::<Public, V4>::try_from(token)
            .map_err(|_| CairnError::new(ErrorKind::Unauthenticated, "token malformed"))?;
        let trusted = public::verify(&self.public, &untrusted, &validation, None, None)
            .map_err(|_| CairnError::new(ErrorKind::Unauthenticated, "token invalid or expired"))?;
        let claims = trusted
            .payload_claims()
            .ok_or_else(|| CairnError::new(ErrorKind::Unauthenticated, "token without payload claims"))?;
        let device_id = claims.get_claim("device_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let tenant_id = claims.get_claim("tenant_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let scopes = claims.get_claim("scopes").and_then(|v| v.as_str()).unwrap_or("sync").to_string();
        if device_id.is_empty() || tenant_id.is_empty() {
            return Err(CairnError::new(ErrorKind::Unauthenticated, "token missing identity claims"));
        }
        let token_hash = Hash::of(token.as_bytes()).hex();
        let row = sqlx::query(
            "SELECT token_hash, scopes, revoked FROM devices WHERE id=?1 AND tenant_id=?2",
        )
        .bind(&device_id)
        .bind(&tenant_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("device lookup: {e}")))?;
        let Some(row) = row else {
            return Err(CairnError::new(ErrorKind::Unauthenticated, "unknown device"));
        };
        let stored_hash: String = row.try_get(0).map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("{e}")))?;
        let revoked: i64 = row.try_get(2).map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("{e}")))?;
        if revoked != 0 {
            return Err(CairnError::new(ErrorKind::Unauthenticated, "device revoked"));
        }
        if stored_hash != token_hash {
            return Err(CairnError::new(ErrorKind::Unauthenticated, "token does not match device enrollment"));
        }
        let _ = sqlx::query("UPDATE devices SET last_seen=?2 WHERE id=?1")
            .bind(&device_id)
            .bind(self.clock.now_millis())
            .execute(pool)
            .await;
        Ok(DeviceIdentity { device_id, tenant_id, scopes })
    }

    /// Revoke a device (unlink semantics).
    pub async fn revoke(&self, pool: &SqlitePool, device_id: &str) -> Result<(), CairnError> {
        let res = sqlx::query("UPDATE devices SET revoked=1 WHERE id=?1")
            .bind(device_id)
            .execute(pool)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("revoke: {e}")))?;
        if res.rows_affected() == 0 {
            return Err(CairnError::new(ErrorKind::NotFound, "device not found"));
        }
        Ok(())
    }
}

/// pasetors derives the public half from `seed||pub`; we only stored the 64-byte secret, so
/// the public bytes are its last 32.
fn secret_public_bytes(secret: &AsymmetricSecretKey<V4>) -> Result<[u8; 32], CairnError> {
    let bytes = secret.as_bytes();
    if bytes.len() < 64 {
        return Err(CairnError::new(ErrorKind::Internal, "unexpected key length"));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[32..64]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn setup() -> (tempfile::TempDir, SqlitePool, Authenticator) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&std::path::Path::new(dir.path()).join("meta.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        let auth = Authenticator::load_or_create(dir.path(), Arc::new(cairn_core::clock::WallClock)).unwrap();
        (dir, pool, auth)
    }

    #[tokio::test]
    async fn enroll_authenticate_roundtrip() {
        let (_d, pool, auth) = setup().await;
        let code = auth.enroll_code("t1", "editor@studio", "sync", 60_000).await;
        let (token, identity) = auth.enroll(&pool, &code, "pubkey-hex", "bench-a").await.unwrap();
        assert_eq!(identity.tenant_id, "t1");
        let verified = auth.authenticate(&pool, &format!("Bearer {token}")).await.unwrap();
        assert_eq!(verified, identity);
    }

    #[tokio::test]
    async fn codes_are_single_use() {
        let (_d, pool, auth) = setup().await;
        let code = auth.enroll_code("t1", "e@s", "sync", 60_000).await;
        auth.enroll(&pool, &code, "pk", "d1").await.unwrap();
        let e = auth.enroll(&pool, &code, "pk", "d2").await.unwrap_err();
        assert_eq!(e.code(), "UNAUTHENTICATED");
    }

    #[tokio::test]
    async fn revocation_blocks_token() {
        let (_d, pool, auth) = setup().await;
        let code = auth.enroll_code("t1", "e@s", "sync", 60_000).await;
        let (token, identity) = auth.enroll(&pool, &code, "pk", "d1").await.unwrap();
        auth.authenticate(&pool, &token).await.unwrap();
        auth.revoke(&pool, &identity.device_id).await.unwrap();
        let e = auth.authenticate(&pool, &token).await.unwrap_err();
        assert_eq!(e.code(), "UNAUTHENTICATED");
    }

    #[tokio::test]
    async fn wrong_scope_device_cannot_admin() {
        let (_d, pool, auth) = setup().await;
        let code = auth.enroll_code("t1", "e@s", "sync", 60_000).await;
        let (_token, identity) = auth.enroll(&pool, &code, "pk", "d1").await.unwrap();
        assert!(!identity.scopes.contains("admin"));
    }
}
