//! Hydration (WO1): materialize files that exist in the journal/local table but are absent
//! on disk — the second-device attach path and the pull side of convergence. Chunk hashes
//! are verified on EVERY chunk before assembly (I2: never materialize a corrupt file).

use std::collections::HashMap;

use cairn_core::compress::decompress_chunk;
use cairn_core::hash::Hash;
use cairn_core::manifest::{Manifest, ManifestEntry};
use cairn_core::{CairnError, ErrorKind};
use cairn_store::state::LocalState;
use cairn_store::{Cas, HeaderCache, Store};

use crate::plane::Plane;
use crate::workspace::workspace_dir;
use cairn_core::normalize::{recompress, Transform};

/// Hydration counters (doctor/status surface).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HydrateStats {
    pub materialized: u64,
    pub bytes: u64,
    pub already_local: u64,
    pub paths: Vec<String>,
}

/// Materialize every journal-known file that is missing on disk for `project_id`.
/// Deterministic (rows iterate in path order). Local CAS is consulted first; misses go to
/// the plane (signed object GET) and are mirrored into the local CAS on arrival.
pub async fn materialize_missing(
    plane: &dyn Plane,
    store: &Store,
    cas: &Cas,
    headers: &HeaderCache,
    tenant: &str,
    project_id: &str,
) -> Result<HydrateStats, CairnError> {
    let mut stats = HydrateStats::default();
    let root = workspace_dir(store, project_id);
    let mut manifest_cache: HashMap<String, Manifest> = HashMap::new();

    for row in store.list_files(project_id) {
        if row.mode != "file" {
            continue;
        }
        let Some(hash_hex) = row.manifest_hash.clone() else {
            continue;
        };
        let state = LocalState::parse(&row.local_state);
        if !matches!(
            state,
            Some(LocalState::Synced) | Some(LocalState::Clean) | Some(LocalState::Placeholder)
        ) {
            continue; // dirty/conflict rows are the PUSH side's business
        }
        let target = root.join(&row.path);
        let stale_placeholder = state == Some(LocalState::Placeholder);
        if target.exists() && !stale_placeholder {
            stats.already_local += 1;
            continue;
        }
        // Placeholder rows materialize (or OVERWRITE a stale local copy left by a remote
        // update to a locally-clean file — the pull side of convergence).
        let bytes = hydrate_one(
            plane,
            cas,
            tenant,
            &hash_hex,
            &row.path,
            &mut manifest_cache,
        )
        .await?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CairnError::new(ErrorKind::Io, format!("mkdir {}: {e}", parent.display()))
            })?;
        }
        std::fs::write(&target, &bytes).map_err(|e| {
            CairnError::new(ErrorKind::Io, format!("write {}: {e}", target.display()))
        })?;
        // Restore the journaled mtime (punch #5): the echo check suppresses watcher
        // events only while on-disk size AND mtime match the row — hydration must
        // produce a file whose stat matches the journaled row, or every hydration
        // would classify as a local edit and re-push forever. Restoring mtimes is
        // also what editors expect (NLE re-link decisions read mtime).
        if row.mtime > 0 {
            let f = std::fs::File::options()
                .append(true)
                .open(&target)
                .map_err(|e| {
                    CairnError::new(ErrorKind::Io, format!("reopen {}: {e}", target.display()))
                })?;
            f.set_modified(millis_to_systemtime(row.mtime))
                .map_err(|e| {
                    CairnError::new(
                        ErrorKind::Io,
                        format!("set mtime {}: {e}", target.display()),
                    )
                })?;
        }
        // header cache fill so the I1 open path works immediately after hydration
        let head: Vec<u8> = bytes
            .iter()
            .take(cairn_core::HEADER_HEAD_BYTES)
            .copied()
            .collect();
        let tail: Vec<u8> = if bytes.len() > cairn_core::HEADER_HEAD_BYTES {
            bytes[bytes.len().saturating_sub(cairn_core::HEADER_TAIL_BYTES)..].to_vec()
        } else {
            Vec::new()
        };
        let _ = headers.put(
            &hash_hex,
            &head,
            if tail.is_empty() { None } else { Some(&tail) },
        );
        store.set_file_state(project_id, &row.path, LocalState::Synced.as_str())?;
        stats.materialized += 1;
        stats.bytes = stats.bytes.saturating_add(bytes.len() as u64);
        stats.paths.push(row.path.clone());
    }
    Ok(stats)
}

/// Journaled millis → SystemTime (exact at millisecond granularity — the encoding
/// rows use, so stat round-trips match bit-for-bit).
fn millis_to_systemtime(ms: i64) -> std::time::SystemTime {
    if ms <= 0 {
        std::time::UNIX_EPOCH
    } else {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0))
    }
}

async fn hydrate_one(
    plane: &dyn Plane,
    cas: &Cas,
    tenant: &str,
    manifest_hash_hex: &str,
    rel_path: &str,
    manifest_cache: &mut HashMap<String, Manifest>,
) -> Result<Vec<u8>, CairnError> {
    let manifest = if let Some(m) = manifest_cache.get(manifest_hash_hex) {
        m.clone()
    } else {
        let hash = Hash::from_hex(manifest_hash_hex)
            .ok_or_else(|| CairnError::new(ErrorKind::ManifestFormat, "bad manifest hash hex"))?;
        let bytes = match cas.get(&hash) {
            Ok(b) => b,
            Err(_) => {
                let fetched = plane.get_manifest(tenant, manifest_hash_hex).await?;
                // cache locally (hash-verified put): the reconcile sweep and offline
                // re-materialization need manifests in the local CAS — a hydrated device
                // whose CAS lacks the manifest silently skips rehash reconciliation.
                cas.put(&hash, &fetched)?;
                fetched
            }
        };
        let m = Manifest::parse(&bytes)?;
        manifest_cache.insert(manifest_hash_hex.to_string(), m.clone());
        m
    };

    // Compression policy is uniform per file (ADR-0004). ZstdDict additionally needs the
    // trained dictionary, which is NOT yet synced across devices (documented gap in
    // STATUS.md) — hydration of dict-compressed files fails loudly rather than silently.
    let (policy, transform) = match &manifest {
        Manifest::Leaf {
            compression,
            transform,
            ..
        }
        | Manifest::Node {
            compression,
            transform,
            ..
        } => (*compression, *transform),
    };

    // Collect every leaf entry across fanout children (depth ≥ 2); child manifests are
    // fetched CAS-first, plane second, and cached for the resolve closure.
    let entries: Vec<ManifestEntry> = match &manifest {
        Manifest::Leaf { entries, .. } => entries.clone(),
        Manifest::Node { children, .. } => {
            let mut all = Vec::new();
            for c in children {
                let bytes = match cas.get(&c.hash) {
                    Ok(b) => b,
                    Err(_) => plane.get_manifest(tenant, &c.hash.hex()).await?,
                };
                let child = Manifest::parse(&bytes)?;
                if let Manifest::Leaf { entries, .. } = &child {
                    all.extend(entries.iter().cloned());
                }
                manifest_cache.insert(c.hash.hex(), child);
            }
            all
        }
    };

    // Pre-fetch every missing leaf chunk: stored (compressed) bytes come off the wire,
    // are decompressed to RAW form, and land in the local CAS hash-verified (the local
    // CAS stores raw chunk content exactly like the push path does).
    let mut local_raw: HashMap<String, Vec<u8>> = HashMap::new();
    for e in &entries {
        let h = e.chunk_hash;
        if cas.contains(&h) {
            continue;
        }
        let hex = h.hex();
        if local_raw.contains_key(&hex) {
            continue;
        }
        let stored = plane.fetch_object(tenant, &hex).await?;
        let raw = decompress_chunk(&stored, policy, None)?;
        cas.put(&h, &raw)?; // BLAKE3-verified before landing (I2)
        local_raw.insert(hex, raw);
    }

    // Resolve + assemble. assemble_file re-verifies every chunk hash before assembly.
    let mut resolve =
        |child: &Hash| -> Option<Manifest> { manifest_cache.get(&child.hex()).cloned() };
    let mut get_chunk = |h: &Hash| -> Option<Vec<u8>> {
        if let Ok(raw) = cas.get(h) {
            return Some(raw);
        }
        local_raw.get(&h.hex()).cloned()
    };
    let inner = cairn_core::manifest::assemble_file(&manifest, &mut resolve, &mut get_chunk)?;
    // container transform (normalization): the stored chunks cover the INNER payload —
    // rebuild the wrapper so editors see a real gzip/zip file (wrapper byte-identity is
    // irrelevant; the payload is what was hash-verified)
    if transform == Transform::None {
        Ok(inner)
    } else {
        recompress(&inner, transform, rel_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plane::{CompleteOut, Entry};
    use cairn_core::chunker::FastCdc;
    use cairn_core::clock::WallClock;
    use cairn_core::compress;
    use cairn_core::compress::compress_chunk;
    use cairn_core::hash::Hash;
    use cairn_core::manifest::Manifest;
    use cairn_store::{Outbox, Store as LocalStore};
    use std::sync::Arc;

    /// In-memory plane over a HashMap — hydration's missing-chunk path without a server.
    struct MemPlane {
        objects: std::collections::HashMap<String, Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl Plane for MemPlane {
        async fn batch_exists(&self, _t: &str, _h: &[String]) -> Result<Vec<String>, CairnError> {
            Ok(vec![])
        }
        async fn create_session(
            &self,
            _t: &str,
            _d: &str,
            _p: &str,
            _m: &[String],
        ) -> Result<crate::plane::Session, CairnError> {
            Err(CairnError::new(ErrorKind::Internal, "unused"))
        }
        async fn complete(
            &self,
            _s: &str,
            _r: &[cairn_proto::pb::UploadReceipt],
        ) -> Result<CompleteOut, CairnError> {
            Err(CairnError::new(ErrorKind::Internal, "unused"))
        }
        async fn put_presigned(&self, _u: &str, _b: &[u8], _c: &str) -> Result<(), CairnError> {
            Ok(())
        }
        async fn put_manifest(&self, _t: &str, _h: &str, _b: &[u8]) -> Result<(), CairnError> {
            Ok(())
        }
        async fn get_manifest(&self, _t: &str, h: &str) -> Result<Vec<u8>, CairnError> {
            self.objects
                .get(h)
                .cloned()
                .ok_or_else(|| CairnError::new(ErrorKind::NotFound, h.to_string()))
        }
        async fn fetch_object(&self, _t: &str, h: &str) -> Result<Vec<u8>, CairnError> {
            self.objects
                .get(h)
                .cloned()
                .ok_or_else(|| CairnError::new(ErrorKind::NotFound, h.to_string()))
        }
        async fn append(
            &self,
            _t: &str,
            _p: &str,
            _d: &str,
            _r: &str,
            _o: cairn_proto::pb::JournalOp,
            _l: u64,
        ) -> Result<(u64, bool), CairnError> {
            Ok((1, false))
        }
        async fn fetch_batch(
            &self,
            _t: &str,
            _p: &str,
            _a: u64,
            _l: u32,
        ) -> Result<Vec<Entry>, CairnError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn hydrates_missing_file_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path(), Arc::new(WallClock)).unwrap();
        let conn = store.conn_handle();
        let cas = Cas::open(&dir.path().join("blobs"), conn.clone()).unwrap();
        let headers = HeaderCache::new(conn.clone());
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        store
            .meta_set("workspace:p1", ws.to_str().unwrap())
            .unwrap();

        // build a file > 1 chunk through the REAL chunker + compression policy
        let content: Vec<u8> = (0..5 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let spans = FastCdc::cut(&content);
        let entries: Vec<ManifestEntry> = spans
            .iter()
            .map(|s| ManifestEntry {
                offset: s.offset,
                len: s.len,
                chunk_hash: Hash::of(
                    &content[s.offset as usize..(s.offset + u64::from(s.len)) as usize],
                ),
            })
            .collect();
        let manifest = Manifest::build(entries, compress::Compression::Zstd3, None);
        let (mh, mbytes) = manifest.serialize();

        // plane has ONLY compressed stored chunks + manifest (device B's view)
        let mut objects = std::collections::HashMap::new();
        objects.insert(mh.hex(), mbytes.clone());
        for e in manifest.flatten() {
            let raw = &content[e.offset as usize..(e.offset + u64::from(e.len)) as usize];
            let stored = compress_chunk(raw, compress::Compression::Zstd3, None).unwrap();
            objects.insert(e.chunk_hash.hex(), stored);
        }
        // register the file row (as pull would)
        store
            .put_file(&cairn_store::FileRow {
                path: "media/clip.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh.hex()),
                size: content.len() as u64,
                mode: "file".into(),
                mtime: 1,
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        // journal row exists but the device is offline: local CAS has nothing
        let plane = MemPlane { objects };
        let stats = materialize_missing(&plane, &store, &cas, &headers, "t1", "p1")
            .await
            .unwrap();
        assert_eq!(stats.materialized, 1);
        let got = std::fs::read(ws.join("media/clip.mov")).unwrap();
        assert_eq!(got, content, "byte-identical materialization");
        // second run: already local → no-op
        let stats2 = materialize_missing(&plane, &store, &cas, &headers, "t1", "p1")
            .await
            .unwrap();
        assert_eq!(stats2.materialized, 0);
        assert_eq!(stats2.already_local, 1);
        let _ = Outbox::new(conn); // touch to keep imports honest
    }

    #[tokio::test]
    async fn hydrates_transformed_container_by_rebuilding_the_wrapper() {
        // normalization round-trip: chunks cover the INNER payload; the hydrated file is
        // a REAL gzip wrapper again (payload hash-verified, wrapper bytes may differ)
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path(), Arc::new(WallClock)).unwrap();
        let conn = store.conn_handle();
        let cas = Cas::open(&dir.path().join("blobs"), conn.clone()).unwrap();
        let headers = HeaderCache::new(conn.clone());
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        store
            .meta_set("workspace:p1", ws.to_str().unwrap())
            .unwrap();

        let inner: Vec<u8> = b"<project><clip/></project>".repeat(20_000);
        let wrapper = cairn_core::normalize::recompress(
            &inner,
            cairn_core::normalize::Transform::Gzip,
            "s.prproj",
        )
        .unwrap();
        let spans = FastCdc::cut(&inner);
        let entries: Vec<ManifestEntry> = spans
            .iter()
            .map(|s| ManifestEntry {
                offset: s.offset,
                len: s.len,
                chunk_hash: Hash::of(
                    &inner[s.offset as usize..(s.offset + u64::from(s.len)) as usize],
                ),
            })
            .collect();
        let manifest = Manifest::build_with_transform(
            entries,
            compress::Compression::Zstd3,
            None,
            cairn_core::normalize::Transform::Gzip,
        );
        let (mh, mbytes) = manifest.serialize();

        let mut objects = std::collections::HashMap::new();
        objects.insert(mh.hex(), mbytes.clone());
        for e in manifest.flatten() {
            let raw = &inner[e.offset as usize..(e.offset + u64::from(e.len)) as usize];
            let stored = compress_chunk(raw, compress::Compression::Zstd3, None).unwrap();
            objects.insert(e.chunk_hash.hex(), stored);
        }
        store
            .put_file(&cairn_store::FileRow {
                path: "scene.prproj".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh.hex()),
                size: wrapper.len() as u64,
                mode: "file".into(),
                mtime: 1,
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        let plane = MemPlane { objects };
        let stats = materialize_missing(&plane, &store, &cas, &headers, "t1", "p1")
            .await
            .unwrap();
        assert_eq!(stats.materialized, 1);
        let got = std::fs::read(ws.join("scene.prproj")).unwrap();
        assert_eq!(
            cairn_core::normalize::sniff(&got),
            cairn_core::normalize::Transform::Gzip,
            "hydrated file must be a gzip wrapper again"
        );
        let back =
            cairn_core::normalize::decompress_inner(&got, cairn_core::normalize::Transform::Gzip)
                .unwrap();
        assert_eq!(back, inner, "payload must round-trip exactly");
    }
}
