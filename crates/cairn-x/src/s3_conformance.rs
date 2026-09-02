//! S3 wire-conformance check (WO6-4): validates Cairn's SigV4 presigning against a
//! REAL S3-compatible implementation on the wire — not against assumed behavior.
//!
//! ## What this proves, and how it is legal
//! The claim "our presigned URLs work against real buckets" is only proven by talking
//! to a real S3 implementation. That implementation must be one we are AUTHORIZED to
//! touch:
//! - CORRECT: run MinIO (or another S3-compatible server) locally/CI — same SigV4
//!   validation, same canonical-request rules, same response codes as AWS S3.
//! - INCORRECT: pointing this tool at buckets discovered via GrayHatWarfare or any
//!   other public-bucket index. Those belong to someone else; "the bucket is open"
//!   does not make listing/reading/writing it authorized — that is unauthorized
//!   access regardless of intent. This tool therefore REFUSES to run unless the
//!   operator explicitly passes `--i-own-the-target`, and every doc that mentions
//!   bucket testing states the same boundary (docs/BETA_RUNBOOK.md).
//!
//! What real S3 implementations enforce (validated here, inferable for AWS/R2):
//! 1. presigned PUT with host-only SignedHeaders is accepted; UNSIGNED-PAYLOAD works;
//! 2. `x-amz-checksum-sha256` bound into SignedHeaders REJECTS a PUT that omits or
//!    mismatches the header (bucket-rejects-corrupt on the wire, SPEC §9.2);
//! 3. presigned GET is Range-capable (206) — the chunk-hydration resume path;
//! 4. expired or tampered URLs are 403 (never 200, never leaked objects);
//! 5. header-auth server-side calls (manifests/packs/GC) round-trip PUT/GET/HEAD/DELETE;
//! 6. region mismatch is rejected server-side (MinIO answers 400 AuthorizationHeader-
//!    Malformed with the expected region; AWS answers the same shape) — region is
//!    part of the signing scope and MUST match the bucket;
//! 7. path-style addressing works end-to-end (this is how MinIO-on-localhost and most
//!    self-hosted gateways are reached; AWS/R2 accept both styles).
//!
//! Evidence from public S3 documentation backing the inferences (no bucket access
//! involved): AWS SigV4 reference (canonical request, UnsignedPayload exception),
//! S3 API reference (presigned TTL cap 7d for SigV4 — Cairn caps at 1h by policy,
//! SPEC §9), R2 S3-API compatibility notes (vhost `bucket.account.r2.cloudflarestorage.com`
//! supported, checksum headers accepted), MinIO strict-mode SigV4 enforcement.

use cairn_core::clock::SystemClock as _;
use cairn_server::storage::{LocalFsStore, SigV4Presigner};

/// Conformance target config (all from `CAIRN_S3_*` env or CLI flags).
#[derive(Clone, Debug)]
pub struct ConformanceCfg {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub path_style: bool,
}

pub struct CheckResult {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn brief_body(b: &str) -> String {
    let s: String = b.chars().take(700).collect();
    s.replace('\n', " ")
}

/// Run the full conformance suite. Returns one result per check; a check FAILS on
/// unexpected status or byte mismatch — never panics on a bad server.
pub async fn run(cfg: &ConformanceCfg) -> anyhow::Result<Vec<CheckResult>> {
    let presigner = if cfg.path_style {
        SigV4Presigner::new_path_style(
            &cfg.access_key,
            &cfg.secret_key,
            &cfg.region,
            &cfg.endpoint,
            &cfg.bucket,
        )
    } else {
        let host = cfg
            .endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let scheme = if cfg.endpoint.starts_with("http://") {
            "http"
        } else {
            "https"
        };
        SigV4Presigner::new(
            &cfg.access_key,
            &cfg.secret_key,
            &cfg.region,
            &format!("{scheme}://{bucket}.{host}", bucket = cfg.bucket),
        )
    };
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut out = Vec::new();
    let now = cairn_core::clock::WallClock.now_millis();
    let obj_key = format!("t1/c/ab/cairn-conformance-{}", now % 1_000_000_000);

    // -- (0) bucket exists / create it (header-auth PUT on the bucket; path-style) --
    let bucket_uri = if cfg.path_style {
        format!("{}/{}", cfg.endpoint.trim_end_matches('/'), cfg.bucket)
    } else {
        cfg.endpoint.trim_end_matches('/').to_string()
    };
    let empty_hash = SigV4Presigner::sha256_hex(b"");
    let bucket_path = if cfg.path_style {
        format!("/{}", cfg.bucket)
    } else {
        "/".to_string()
    };
    let (auth, amz_date) =
        presigner.authorization_header_path("PUT", &bucket_path, &empty_hash, now);
    if std::env::var("CAIRN_S3_CONFORMANCE_DEBUG").is_ok() {
        println!(
            "DEBUG bucket PUT: auth={auth} x-amz-date={amz_date} url={bucket_uri} payload_hash={empty_hash}"
        );
    }
    let resp = http
        .put(&bucket_uri)
        .header("Authorization", auth)
        .header("x-amz-date", amz_date)
        .header("x-amz-content-sha256", &empty_hash)
        .send()
        .await?;
    let ok = resp.status().is_success() || resp.status().as_u16() == 409;
    let detail = format!(
        "PUT bucket -> {} {}",
        resp.status(),
        brief_body(&resp.text().await.unwrap_or_default())
    );
    out.push(CheckResult {
        name: "bucket_create_or_exists",
        ok,
        detail,
    });

    // -- (1) presigned PUT, host-only SignedHeaders incl. x-amz-content-sha256 --
    // R2 wire rule (2026-09-02): every x-amz-* header must be IN SignedHeaders, and
    // the presigner binds x-amz-content-sha256:UNSIGNED-PAYLOAD — so the client sends
    // exactly that header. Without it MinIO 400s (missing signed header) too.
    let payload: Vec<u8> = (0..64_000u32).map(|i| (i % 251) as u8).collect();
    let url = presigner.presign_put_host_only(&obj_key, 3600, now);
    let resp = http
        .put(&url)
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .body(payload.clone())
        .send()
        .await?;
    let ok = resp.status().is_success();
    let detail = format!(
        "PUT {} bytes -> {} {}",
        payload.len(),
        resp.status(),
        brief_body(&resp.text().await.unwrap_or_default())
    );
    out.push(CheckResult {
        name: "presign_put_host_only",
        ok,
        detail,
    });

    // -- (2) presigned GET, full object, byte-identity --
    let url = presigner.presign_get(&obj_key, 3600, now);
    let resp = http.get(&url).send().await?;
    let got = resp.bytes().await?;
    out.push(CheckResult {
        name: "presign_get_byte_identity",
        ok: got.as_ref() == payload.as_slice(),
        detail: format!(
            "GET -> {} bytes, identity={}",
            got.len(),
            got.as_ref() == payload.as_slice()
        ),
    });

    // -- (3) Range GET (chunk-hydration resume path): bytes=1000-1099 -> 206 --
    let resp = http
        .get(&url)
        .header("Range", "bytes=1000-1099")
        .send()
        .await?;
    let slice = resp.bytes().await?;
    out.push(CheckResult {
        name: "presign_get_range_206",
        ok: slice.as_ref() == &payload[1000..1100],
        detail: format!("Range GET -> {} bytes", slice.len()),
    });

    // -- (4) checksum-bound presign REJECTS a mismatched header (bucket-rejects-corrupt) --
    let checksum = SigV4Presigner::sha256_hex(&payload);
    let url = presigner.presign_put(&obj_key, 3600, now, &checksum);
    let bad: Vec<u8> = payload.iter().map(|b| b.wrapping_add(1)).collect();
    let resp = http.put(&url).body(bad).send().await?;
    out.push(CheckResult {
        name: "checksum_bound_put_rejects_corrupt",
        ok: resp.status().as_u16() == 400 || resp.status().as_u16() == 403,
        detail: format!("corrupt PUT -> {} (want 400/403)", resp.status()),
    });

    // -- (4b) HOST-ONLY presign + base64 checksum header (the DAEMON path, quirk S1) --
    // Sessions presign host-only; the client attaches `x-amz-checksum-sha256`
    // (base64 per RFC 4648 — hex is rejected 400 "Invalid checksum provided").
    // The bucket then verifies the value against the payload, so corrupt
    // uploads are rejected at the bucket WITHOUT checksum-bound presigning.
    let ck_hex = SigV4Presigner::sha256_hex(&payload);
    let ck_b64 =
        cairn_core::hash::b64_encode(&cairn_core::hash::hex_decode(&ck_hex).expect("valid hex"));
    let url = presigner.presign_put_host_only(&obj_key, 3600, now);
    let resp = http
        .put(&url)
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-amz-checksum-sha256", &ck_b64)
        .body(payload.clone())
        .send()
        .await?;
    out.push(CheckResult {
        name: "host_only_put_base64_checksum_ok",
        ok: resp.status().is_success(),
        detail: format!("host-only PUT + base64 checksum -> {}", resp.status()),
    });
    let url = presigner.presign_put_host_only(&obj_key, 3600, now);
    let bad_b64 = cairn_core::hash::b64_encode(&[0u8; 32]);
    let resp = http
        .put(&url)
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-amz-checksum-sha256", bad_b64)
        .body(payload.clone())
        .send()
        .await?;
    out.push(CheckResult {
        name: "host_only_put_rejects_wrong_checksum",
        ok: resp.status().as_u16() == 400 || resp.status().as_u16() == 403,
        detail: format!(
            "host-only PUT + wrong base64 -> {} (want 400/403)",
            resp.status()
        ),
    });

    // -- (5) expired URL -> 403 --
    let url = presigner.presign_get(&obj_key, 3600, now - 3_600_000 - 60_000);
    let resp = http.get(&url).send().await?;
    out.push(CheckResult {
        name: "expired_url_403",
        ok: resp.status().as_u16() == 403,
        detail: format!("expired GET -> {} (want 403)", resp.status()),
    });

    // -- (6) tampered signature -> 403 --
    let url = presigner.presign_get(&obj_key, 3600, now);
    let tampered = match url.find("X-Amz-Signature=") {
        Some(pos) => {
            let sig_at = pos + "X-Amz-Signature=".len();
            let mut bytes = url.clone().into_bytes();
            // flip the first hex digit; URLs + signatures are ASCII so byte surgery is safe
            if bytes.get(sig_at) == Some(&b'0') {
                bytes[sig_at] = b'1';
            } else {
                bytes[sig_at] = b'0';
            }
            String::from_utf8(bytes).unwrap_or(url.clone())
        }
        None => url.clone(),
    };
    let resp = http.get(&tampered).send().await?;
    out.push(CheckResult {
        name: "tampered_signature_403",
        ok: resp.status().as_u16() == 403,
        detail: format!("tampered GET -> {} (want 403)", resp.status()),
    });

    // -- (7) header-auth server-side paths: PUT/GET/HEAD/DELETE (manifests, packs, GC) --
    let server_key = format!("t1/o/ab/manifest-{}", now % 100_000);
    let body = b"cairn-header-auth-conformance".to_vec();
    let payload_hash = SigV4Presigner::sha256_hex(&body);
    let (auth, amz_date) = presigner.authorization_header("PUT", &server_key, &payload_hash, now);
    let resp = http
        .put(presigner.url_for(&server_key))
        .header("Authorization", auth)
        .header("x-amz-date", amz_date)
        .header("x-amz-content-sha256", &payload_hash)
        .body(body.clone())
        .send()
        .await?;
    let put_ok = resp.status().is_success();
    let (auth, amz_date) = presigner.authorization_header("HEAD", &server_key, &empty_hash, now);
    let resp = http
        .head(presigner.url_for(&server_key))
        .header("Authorization", auth)
        .header("x-amz-date", amz_date)
        .header("x-amz-content-sha256", &empty_hash)
        .send()
        .await?;
    let head_ok = resp.status().is_success()
        && resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            == Some(body.len() as u64);
    let (auth, amz_date) = presigner.authorization_header("GET", &server_key, &empty_hash, now);
    let resp = http
        .get(presigner.url_for(&server_key))
        .header("Authorization", auth)
        .header("x-amz-date", amz_date)
        .header("x-amz-content-sha256", &empty_hash)
        .send()
        .await?;
    let get_ok = resp.status().is_success() && resp.bytes().await?.as_ref() == body.as_slice();
    let (auth, amz_date) = presigner.authorization_header("DELETE", &server_key, &empty_hash, now);
    let resp = http
        .delete(presigner.url_for(&server_key))
        .header("Authorization", auth)
        .header("x-amz-date", amz_date)
        .header("x-amz-content-sha256", &empty_hash)
        .send()
        .await?;
    let del_ok = resp.status().is_success();
    out.push(CheckResult {
        name: "header_auth_put_get_head_delete",
        ok: put_ok && head_ok && get_ok && del_ok,
        detail: format!("put={put_ok} head={head_ok} get={get_ok} delete={del_ok}"),
    });

    // -- (8) wrong region is rejected (region is part of the signing scope) --
    let wrong_region = if cfg.region == "us-east-7" {
        "us-east-6".to_string()
    } else {
        "us-east-7".to_string()
    };
    let wrong = if cfg.path_style {
        SigV4Presigner::new_path_style(
            &cfg.access_key,
            &cfg.secret_key,
            &wrong_region,
            &cfg.endpoint,
            &cfg.bucket,
        )
    } else {
        SigV4Presigner::new(
            &cfg.access_key,
            &cfg.secret_key,
            &wrong_region,
            &presigner.url_for("x"),
        )
    };
    let url = wrong.presign_get(&obj_key, 3600, now);
    let resp = http.get(&url).send().await?;
    let region_note = |status: u16| {
        if status == 200 || status == 206 {
            "server ACCEPTED a wrong-region signature — MinIO enforces region only when \
             MINIO_SITE_REGION is set (AWS S3 always enforces; R2 ignores region by design); \
             set the site region on production-like targets"
                .to_string()
        } else {
            "region is enforced server-side (matches AWS S3 behavior)".to_string()
        }
    };
    out.push(CheckResult {
        name: "region_mismatch_rejected",
        ok: resp.status().as_u16() == 400 || resp.status().as_u16() == 403,
        detail: format!(
            "wrong-region GET -> {} (want 400/403); {}",
            resp.status(),
            region_note(resp.status().as_u16())
        ),
    });

    // -- (9) cleanup the conformance object (idempotent DELETE) --
    let url = presigner.presign_get(&obj_key, 3600, now);
    let (auth, amz_date) = presigner.authorization_header("DELETE", &obj_key, &empty_hash, now);
    let resp = http
        .delete(presigner.url_for(&obj_key))
        .header("Authorization", auth)
        .header("x-amz-date", amz_date)
        .header("x-amz-content-sha256", &empty_hash)
        .send()
        .await?;
    let _ = resp.status(); // cleanup best-effort
    let _ = url;

    Ok(out)
}

/// LocalFsStore stays referenced so the module documents the DEV fallback parity
/// (dev endpoint implements the same presign semantics in-process).
#[allow(dead_code)]
fn _dev_parity_note(_s: &LocalFsStore) {}
