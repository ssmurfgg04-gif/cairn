//! Hydration (WO1): materialize files that exist in the journal/local table but are absent
//! on disk — the second-device attach path and the pull side of convergence. Chunk hashes
//! are verified on EVERY chunk before assembly (I2: never materialize a corrupt file).

use std::collections::HashMap;

use cairn_core::compress::decompress_chunk;
use cairn_core::hash::Hash;
use cairn_core::manifest::{assemble_file_into, Manifest};
use cairn_core::{CairnError, ErrorKind};
use cairn_store::state::LocalState;
use cairn_store::{Cas, HeaderCache, Store};

use crate::peer::PeerSource;
use crate::plane::Plane;
use crate::workspace::workspace_dir;
use cairn_core::normalize::Transform;

/// Hydration counters (doctor/status surface). Peer-sourced block counts are
/// surfaced by the swarm stats (blocks_fetched) — the daemon reports both.
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
/// When a [`PeerSource`] is supplied, swarm peers are consulted FIRST (ADR-0017 §7:
/// LAN-speed blocks, zero cloud egress).
pub async fn materialize_missing(
    plane: &dyn Plane,
    peer: Option<&dyn PeerSource>,
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
            peer,
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
        // the disk now descends from the remote head: consume any
        // content-lineage fork marker for this path (round 13)
        let _ = crate::apply::clear_fork(store, project_id, &row.path);
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

/// Hydrate ONE file's full bytes by manifest hash (compat wrapper over
/// [`hydrate_one_into`]). ⚠️ Buffers the WHOLE wrapper in RAM — fine for
/// project-class files, wrong for 50GB media. Streaming callers (restore to file,
/// recall) MUST use [`hydrate_one_into`].
#[allow(clippy::implicit_hasher)] // cache is caller-local; hasher choice is not a contract
pub async fn hydrate_one(
    plane: &dyn Plane,
    peer: Option<&dyn PeerSource>,
    cas: &Cas,
    tenant: &str,
    manifest_hash_hex: &str,
    rel_path: &str,
    manifest_cache: &mut HashMap<String, Manifest>,
) -> Result<Vec<u8>, CairnError> {
    let mut out = Vec::new();
    hydrate_one_into(
        plane,
        peer,
        cas,
        tenant,
        manifest_hash_hex,
        rel_path,
        manifest_cache,
        &mut out,
    )
    .await?;
    Ok(out)
}

/// STREAMING hydrate (review round: `hydrate_one` assembled the whole file in RAM —
/// an instant OOM for 50GB-class media on a 16GB machine). Chunks are written
/// sequentially to `out` as they arrive; peak RAM is one chunk (≤16MB) plus the
/// manifest cache. Every chunk is hash-verified before its bytes touch `out` (I2 —
/// a corrupt chunk aborts mid-stream; callers writing to a real file must use
/// temp-file + rename for atomicity).
///
/// gzip containers stream through a `GzEncoder` directly into `out` — no inner
/// payload buffering. zip stays scoped-out and rejects BEFORE any I/O.
#[allow(clippy::implicit_hasher)] // cache is caller-local; hasher choice is not a contract
pub async fn hydrate_one_into<W: std::io::Write>(
    plane: &dyn Plane,
    peer: Option<&dyn PeerSource>,
    cas: &Cas,
    tenant: &str,
    manifest_hash_hex: &str,
    _rel_path: &str,
    manifest_cache: &mut HashMap<String, Manifest>,
    out: &mut W,
) -> Result<u64, CairnError> {
    let manifest = if let Some(m) = manifest_cache.get(manifest_hash_hex) {
        m.clone()
    } else {
        let hash = Hash::from_hex(manifest_hash_hex)
            .ok_or_else(|| CairnError::new(ErrorKind::ManifestFormat, "bad manifest hash hex"))?;
        let bytes = match cas.get_async(&hash).await {
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

    // zip normalization is scoped OUT (multi-entry archives have no single inner
    // payload — normalize.rs). Reject BEFORE any network/disk work, never mid-stream.
    if transform == Transform::Zip {
        return Err(CairnError::new(
            ErrorKind::Compression,
            "normalize: zip normalization is scoped OUT (see normalize.rs); the file syncs \
             as opaque bytes instead",
        ));
    }

    // Recursively collect every leaf entry across fanout children (any depth ≥ 2);
    // child manifests are fetched CAS-first, plane second, and cached for the resolver.
    collect_entries_recursive(&manifest, cas, plane, tenant, manifest_cache).await?;
    let mut resolve = |child: &Hash| -> Option<Vec<u8>> {
        manifest_cache.get(&child.hex()).map(|m| m.serialize().1)
    };

    // Pre-fetch every missing leaf chunk: stored (compressed) bytes come off the wire,
    // are decompressed to RAW form, and land in the local CAS hash-verified (the local
    // CAS stores raw chunk content exactly like the push path does). The verified CAS
    // put IS the read-back path — no in-RAM mirror of the whole file (the old
    // `local_raw` HashMap duplicated every fetched byte and defeated the point).
    //
    // PEER-FIRST (ADR-0017 §7): the swarm warms ALL missing hashes up front (the
    // pre-walk — parallel scheduling starts immediately), then each chunk tries
    // peers before the cloud plane. `peer_may_have` is a fast local check; a
    // no-holder chunk falls straight to the plane with zero added latency.
    let entries =
        manifest.flatten_with(&mut |child: &Hash| manifest_cache.get(&child.hex()).cloned());
    if let Some(peer) = peer {
        let missing: Vec<Hash> = entries
            .iter()
            .map(|e| e.chunk_hash)
            .filter(|h| !cas.contains(h))
            .collect();
        if !missing.is_empty() {
            peer.warm_blocks(&missing).await;
        }
    }
    for e in &entries {
        let h = e.chunk_hash;
        if cas.contains(&h) {
            continue;
        }
        let mut from_peer = false;
        if let Some(peer) = peer {
            if peer.peer_may_have(&h) {
                if let Some(raw) = peer.fetch_peer_block(&h).await {
                    // hash-verified by the swarm AND by the CAS put (I2 twice —
                    // the peer is outside every trust boundary)
                    cas.put(&h, &raw)?;
                    from_peer = true;
                }
            }
        }
        if !from_peer {
            let stored = plane.fetch_object(tenant, &h.hex()).await?;
            let raw = decompress_chunk(&stored, policy, None)?;
            cas.put(&h, &raw)?; // BLAKE3-verified before landing (I2)
        }
    }

    // Stream assembly. The resolver hands serialized manifest bytes to
    // assemble_file_into (it parses internally); get_chunk reads the verified CAS.
    let mut get_chunk = |h: &Hash| -> Option<Vec<u8>> { cas.get(h).ok() };
    match transform {
        Transform::None => assemble_file_into(&manifest, &mut resolve, &mut get_chunk, out),
        Transform::Gzip => {
            // stream the inner payload through the gzip encoder — wrapper bytes are
            // rebuilt on the fly, wrapper byte-identity is irrelevant (normalize.rs)
            let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
            let written = assemble_file_into(&manifest, &mut resolve, &mut get_chunk, &mut enc)?;
            enc.finish().map_err(|e| {
                CairnError::new(ErrorKind::Compression, format!("gzip finish: {e}"))
            })?;
            Ok(written)
        }
        Transform::Zip => unreachable!("zip rejected before any I/O"),
    }
}

/// Recursively fetch + parse every fanout child manifest (CAS-first, plane second),
/// caching by hash so the resolver closures never touch the network. Replaces the
/// old ONE-LEVEL walk: depth-3 trees (impossible in practice — each level covers
/// ≥8,192× more chunks — but cheap to be correct) no longer drop grandchildren.
async fn collect_entries_recursive(
    manifest: &Manifest,
    cas: &Cas,
    plane: &dyn Plane,
    tenant: &str,
    cache: &mut HashMap<String, Manifest>,
) -> Result<(), CairnError> {
    let Manifest::Node { children, .. } = manifest else {
        return Ok(()); // leaf: entries are already in hand
    };
    for c in children {
        if cache.contains_key(&c.hash.hex()) {
            continue;
        }
        let bytes = match cas.get_async(&c.hash).await {
            Ok(b) => b,
            Err(_) => plane.get_manifest(tenant, &c.hash.hex()).await?,
        };
        let child = Manifest::parse(&bytes)?;
        Box::pin(collect_entries_recursive(&child, cas, plane, tenant, cache)).await?;
        cache.insert(c.hash.hex(), child);
    }
    Ok(())
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
    use cairn_core::manifest::{Manifest, ManifestEntry};
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
        let stats = materialize_missing(&plane, None, &store, &cas, &headers, "t1", "p1")
            .await
            .unwrap();
        assert_eq!(stats.materialized, 1);
        let got = std::fs::read(ws.join("media/clip.mov")).unwrap();
        assert_eq!(got, content, "byte-identical materialization");
        // second run: already local → no-op
        let stats2 = materialize_missing(&plane, None, &store, &cas, &headers, "t1", "p1")
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
        let stats = materialize_missing(&plane, None, &store, &cas, &headers, "t1", "p1")
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

    /// In-memory PeerSource: answers from a block map — the swarm's test twin.
    struct MemPeer {
        blocks: std::collections::HashMap<cairn_core::hash::Hash, Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl crate::peer::PeerSource for MemPeer {
        fn peer_may_have(&self, h: &cairn_core::hash::Hash) -> bool {
            self.blocks.contains_key(h)
        }
        async fn fetch_peer_block(&self, h: &cairn_core::hash::Hash) -> Option<Vec<u8>> {
            self.blocks.get(h).cloned()
        }
        async fn warm_blocks(&self, _hashes: &[cairn_core::hash::Hash]) {}
    }

    #[tokio::test]
    async fn peer_first_hydration_pulls_blocks_from_the_swarm() {
        // ADR-0017 §7: chunks a peer holds come from the peer, chunks it
        // doesn't come from the plane — one file, both paths exercised.
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

        // varied content (FastCDC needs entropy to find cut points — uniform
        // test data produced ONE 3MB chunk and defeated the split-transport
        // assertion)
        let mut seed = 0x2545F491u64;
        let content: Vec<u8> = (0..12 * 1024 * 1024u32)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed & 0xFF) as u8
            })
            .collect();
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

        let flat = manifest.flatten();
        assert!(flat.len() >= 2, "test needs a multi-chunk file");
        // peer holds the FIRST half of the chunks; the plane holds everything
        let peer_holds = flat.len() / 2;
        let mut plane_objects = std::collections::HashMap::new();
        plane_objects.insert(mh.hex(), mbytes.clone());
        let mut peer_blocks = std::collections::HashMap::new();
        for (i, e) in flat.iter().enumerate() {
            let raw = &content[e.offset as usize..(e.offset + u64::from(e.len)) as usize];
            let stored = compress_chunk(raw, compress::Compression::Zstd3, None).unwrap();
            plane_objects.insert(e.chunk_hash.hex(), stored);
            if i < peer_holds {
                peer_blocks.insert(e.chunk_hash, raw.to_vec());
            }
        }
        store
            .put_file(&cairn_store::FileRow {
                path: "media/broll.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh.hex()),
                size: content.len() as u64,
                mode: "file".into(),
                mtime: 1,
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        let plane = MemPlane {
            objects: plane_objects,
        };
        let peer = MemPeer {
            blocks: peer_blocks,
        };
        let stats = materialize_missing(&plane, Some(&peer), &store, &cas, &headers, "t1", "p1")
            .await
            .unwrap();
        assert_eq!(stats.materialized, 1);
        let got = std::fs::read(ws.join("media/broll.mov")).unwrap();
        assert_eq!(got, content, "byte-identical through BOTH transports");
    }

    #[tokio::test]
    async fn plane_fallback_when_peer_lacks_every_block() {
        // the cloud-fallback contract: a swarm with nothing relevant adds
        // zero latency (may_have = false short-circuits) and the plane serves
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

        let mut seed = 0x9E3779B97F4A7C15u64;
        let content: Vec<u8> = (0..1024 * 1024u32)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed & 0xFF) as u8
            })
            .collect();
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
        let mut objects = std::collections::HashMap::new();
        objects.insert(mh.hex(), mbytes.clone());
        for e in manifest.flatten() {
            let raw = &content[e.offset as usize..(e.offset + u64::from(e.len)) as usize];
            let stored = compress_chunk(raw, compress::Compression::Zstd3, None).unwrap();
            objects.insert(e.chunk_hash.hex(), stored);
        }
        store
            .put_file(&cairn_store::FileRow {
                path: "media/plain.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh.hex()),
                size: content.len() as u64,
                mode: "file".into(),
                mtime: 1,
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        let plane = MemPlane { objects };
        let peer = MemPeer {
            blocks: std::collections::HashMap::new(), // empty swarm
        };
        let stats = materialize_missing(&plane, Some(&peer), &store, &cas, &headers, "t1", "p1")
            .await
            .unwrap();
        assert_eq!(stats.materialized, 1);
        let got = std::fs::read(ws.join("media/plain.mov")).unwrap();
        assert_eq!(got, content);
    }
}
