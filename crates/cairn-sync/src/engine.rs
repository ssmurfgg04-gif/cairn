//! Sync engine (SPEC §7.3): one pass = reconcile dirty files → hash+chunk → dedupe/upload →
//! outbox append → synced. Recovery = WAL replay + outbox resend + BatchExists re-check;
//! every step is idempotent and safely re-enterable (I2). Conflict handling per §7.1:
//! rejection → conflict copy on the new path → re-append.

use prost::Message as _;
use rand::SeedableRng;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use cairn_core::clock::SystemClock;
#[allow(unused_imports)]
use cairn_core::clock::WallClock;
use cairn_core::compress::{self, DictRegistry};
use cairn_core::hash::Hash;
use cairn_core::manifest::{Manifest, ManifestEntry};
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::UploadReceipt;
use cairn_store::state::LocalState;
use cairn_store::{Cas, HeaderCache, Outbox, Store};

use crate::aimd::Gate;
use crate::plane::{upsert_op, Plane};
use crate::retry::{backoff_millis, should_retry};
use crate::workspace::workspace_dir;

/// Engine context for one device + project (+ optional root namespace,
/// ADR-0019 §2). `project_id` names the SERVER journal (plane calls);
/// `local_ns` names the LOCAL row tables (files, cursor, outbox, forks);
/// `author_id` is the journal authorship + own-op-suppression identity.
/// For the default (legacy) root both equal the plain ids, so pre-round-15
/// stores and journals are byte-compatible.
pub struct Engine {
    pub tenant_id: String,
    /// Server journal / plane scope.
    pub project_id: String,
    /// Login identity (server auth, leases of record).
    pub device_id: String,
    /// Local store namespace (rows/cursor/outbox); equals `project_id`
    /// for the default root, `<project_id>#<root_id>` for additional roots.
    pub local_ns: String,
    /// Journal authorship: plain `device_id` for the default root,
    /// `<device_id>#<root_id>` for additional roots — own-op suppression
    /// compares THIS, so only same-root entries are skipped.
    pub author_id: String,
    pub store: Store,
    pub cas: Cas,
    pub outbox: Outbox,
    pub headers: HeaderCache,
    pub plane: Arc<dyn Plane>,
    pub dicts: DictRegistry,
    pub gate: Gate,
}

/// Outcome counters for a pass (status/doctor/dashboard).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassStats {
    pub uploaded_chunks: u32,
    pub skipped_chunks: u32,
    pub appended: u32,
    pub conflicts_resolved: u32,
    pub applied_entries: u32,
    /// CONTRACT-DEBT #1: timeline conflicts where a semantic-merge offer row
    /// was created this pass (flag-gated).
    pub merge_offered: u32,
    /// P1 measurement (perf triage #3): per-stage ingest timings accumulated
    /// across `process_file` calls in this pass — read / hash+chunk /
    /// local-CAS / network. Lets the next profiling run attribute the
    /// pipeline instead of guessing (serial-vs-parallel lives here).
    pub read_ms: u64,
    pub hash_ms: u64,
    pub cas_ms: u64,
    pub net_ms: u64,
}

impl Engine {
    /// Full pass: push local dirt, then pull remote entries (cursor replay is the guarantee).
    pub async fn sync_pass(&self) -> Result<PassStats, CairnError> {
        let mut stats = PassStats::default();
        self.push_phase(&mut stats).await?;
        self.pull_phase(&mut stats).await?;
        Ok(stats)
    }

    async fn push_phase(&self, stats: &mut PassStats) -> Result<(), CairnError> {
        // recovery first: resend any acknowledged-but-unsent outbox entries (I2)
        self.flush_outbox(stats).await?;
        // State filter pushed into SQL: busy projects skip decoding every
        // synced row just to discard it (same set as the old in-memory
        // filter — dirty/conflict only; unknown strings never match either).
        for f in self.store.list_files_in_states(
            &self.local_ns,
            &[LocalState::Dirty.as_str(), LocalState::Conflict.as_str()],
        ) {
            let Some(state) = LocalState::parse(&f.local_state) else {
                continue;
            };
            if !matches!(state, LocalState::Dirty | LocalState::Conflict) {
                continue;
            }
            if f.mode != "file" {
                // Metadata rows (dirs, symlinks) carry no content to chunk. A
                // dirty DIR row reaches fs::read(directory) -> EACCES on
                // Windows / EISDIR on Linux and wedges EVERY pass (round 13,
                // caught LIVE by the W1 matrix row on a windows runner: the
                // ReadDirectoryChangesW parent-dir event dirties the dir row
                // the moment children appear). The scan walk re-puts dir rows
                // as metadata; the push side must never touch them.
                continue;
            }
            self.process_file(&f.path, stats).await?;
        }
        Ok(())
    }

    /// Ingest one dirty file end-to-end. `pub(crate)`: the merge-offer
    /// accept path re-drives the normal pipeline for the merged timeline
    /// (ONE journal entry, same request-id discipline) instead of cloning it.
    pub(crate) async fn process_file(
        &self,
        path: &str,
        stats: &mut PassStats,
    ) -> Result<(), CairnError> {
        let full = self.rooted(path);
        // stat BEFORE reading: these are the values the sweep/rescan will compare
        // against after the push — if a write lands mid-push, the watcher re-dirties
        // and the next pass re-pushes with fresh stat (I2: last-writer wins is fine,
        // silent drift is not).
        let pushed_meta = std::fs::metadata(&full)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("stat {path}: {e}")))?;
        let pushed_size = pushed_meta.len();
        let pushed_mtime = crate::scan::mtime_millis(&pushed_meta);
        // async file lane (ADR-0025): `tokio::fs::read` rides the runtime's async
        // file machinery — on Linux with the io_uring driver armed (tokio
        // `io-uring` feature, runtime-probed with automatic fallback) big reads
        // land on the ring instead of parking an I/O worker on `std::fs::read`.
        let t_read = std::time::Instant::now();
        let bytes = tokio::fs::read(&full)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("read {path}: {e}")))?;
        stats.read_ms += t_read.elapsed().as_millis() as u64;
        // raw (pre-normalization) size feeds the content-derived idempotency key
        // and the upsert op below — captured before `bytes` moves into the lane
        let raw_len = bytes.len();
        // header cache fill (I1 path, SPEC §5.1): head 2MB + tail 1MB OF THE RAW
        // FILE — carved before `bytes` moves into the offload lane
        let head: Vec<u8> = bytes
            .iter()
            .take(cairn_core::HEADER_HEAD_BYTES)
            .copied()
            .collect();
        let tail: Vec<u8> = if raw_len > cairn_core::HEADER_HEAD_BYTES {
            bytes[raw_len.saturating_sub(cairn_core::HEADER_TAIL_BYTES)..].to_vec()
        } else {
            Vec::new()
        };
        // chunk-input normalization (flag-gated): compressed project containers are
        // decompressed so CDC runs on the canonical INNER payload — a 5KB XML edit inside a
        // gzip'd .prproj then reuses ~all chunks instead of avalanching the wrapper
        let normalize_on = self
            .store
            .meta_get("flag:normalize_containers")
            .is_some_and(|v| v == "true");
        let transform = if normalize_on {
            cairn_core::normalize::sniff(&bytes)
        } else {
            cairn_core::normalize::Transform::None
        };
        // transformed containers chunk with plain zstd-3 (the inner payload has no ext to
        // sniff; dict training does not apply to canonical payloads)
        let policy = if transform == cairn_core::normalize::Transform::None {
            compress::policy_for(path)
        } else {
            compress::Compression::Zstd3
        };
        let dict = if policy == cairn_core::manifest::Compression::ZstdDict {
            self.dicts
                .get(&self.local_ns)
                .or_else(|| compress::train_project_dict(&self.local_ns, &bytes))
        } else {
            None
        };
        if let Some(d) = &dict {
            self.dicts.put(d.clone());
        }
        // transformed containers chunk FINE (project-class granularity — a 512-byte edit
        // in a 6MB .blend must not re-upload a 4MB chunk); media keeps the coarse profile.
        // Hash+chunk is ~1 GiB/s of CPU: it moves to the offload lane (ADR-0025,
        // PostHog pattern) instead of parking this I/O worker for the whole pass;
        // small files stay inline, big ones round-trip through rayon + a oneshot.
        let fine = transform != cairn_core::normalize::Transform::None;
        let content: Vec<u8> = if fine {
            cairn_core::normalize::decompress_inner(&bytes, transform)?
        } else {
            bytes
        };
        let t_hash = std::time::Instant::now();
        let (sh, content) = crate::offload::hash_stream_owned(content, fine).await?;
        stats.hash_ms += t_hash.elapsed().as_millis() as u64;

        // local CAS insert (verified) — content-addressed, idempotent
        let t_cas = std::time::Instant::now();
        for (span, h) in sh.spans.iter().zip(sh.chunk_hashes.iter()) {
            let raw = &content[span.offset as usize..(span.offset + u64::from(span.len)) as usize];
            if self.cas.contains(h) {
                stats.skipped_chunks += 1;
            } else {
                self.cas.put(h, raw)?;
                stats.uploaded_chunks += 1;
            }
        }
        stats.cas_ms += t_cas.elapsed().as_millis() as u64;

        // upload missing chunks via session + AIMD
        let hash_hexes: Vec<String> = sh.chunk_hashes.iter().map(Hash::hex).collect();
        let t_net = std::time::Instant::now();
        // Per-call retry like the PUT path: a lost batch_exists fails the
        // whole file push otherwise. Read-only → trivially idempotent.
        let mut attempts = 0u32;
        let mut rng = rand::rngs::StdRng::seed_from_u64(0xbe7c4e55u64);
        let missing = loop {
            match self.plane.batch_exists(&self.tenant_id, &hash_hexes).await {
                Ok(m) => break m,
                Err(e) => {
                    attempts += 1;
                    if !should_retry(e.retry_class(), attempts - 1) {
                        return Err(e);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                        attempts, &mut rng,
                    )))
                    .await;
                }
            }
        };
        if !missing.is_empty() {
            // Same retry; a duplicate session on retry is a harmless orphan
            // (sessions expire; PUTs only ever target the winning session).
            let mut attempts = 0u32;
            let session = loop {
                match self
                    .plane
                    .create_session(&self.tenant_id, &self.author_id, &self.project_id, &missing)
                    .await
                {
                    Ok(s) => break s,
                    Err(e) => {
                        attempts += 1;
                        if !should_retry(e.retry_class(), attempts - 1) {
                            return Err(e);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                            attempts, &mut rng,
                        )))
                        .await;
                    }
                }
            };
            // receipts must report the size the BUCKET holds — the compressed/stored bytes —
            // because CompleteUpload sample-verifies via HEAD against the object key.
            // (Raw span sizes are only correct for Compression::None; reporting them for
            // zstd-stored chunks rejects every upload.)
            // O(1) span lookup per chunk (was an O(n) .find() per chunk →
            // O(n²) per file with many chunks).
            let span_by_hash: std::collections::HashMap<Hash, &cairn_core::chunker::ChunkSpan> = sh
                .spans
                .iter()
                .zip(sh.chunk_hashes.iter())
                .map(|(s, h)| (*h, s))
                .collect();
            // Bounded-parallel upload (was strictly serial: N chunks = N
            // sequential compress + round trips). Each PUT is independent and
            // idempotent (content-addressed); receipts are still built from
            // `session.puts` order, so completion semantics are unchanged.
            // A failed chunk aborts the pass like before — chunks already in
            // flight are crash-equivalent orphans (sessions expire; the
            // server sweeps), exactly what a kill -9 mid-upload produces.
            // Bound tracks the AIMD window so the gate still throttles us.
            use futures::StreamExt as _;
            let bound = self.gate.limit().clamp(4, 32);
            let puts: Vec<(String, String)> = session
                .puts
                .iter()
                .map(|(h, u)| (h.clone(), u.clone()))
                .collect();
            // Shared borrows bound once so each `async move` future copies
            // the reference, not the owned value (FnMut closure runs per item).
            let spans = &span_by_hash;
            let body: &[u8] = &content;
            let d = dict.as_ref();
            let uploaded: Vec<Result<(String, u64), CairnError>> = futures::stream::iter(puts)
                .map(|(hash_hex, url)| async move {
                    let h = Hash::from_hex(&hash_hex).ok_or_else(|| {
                        CairnError::new(ErrorKind::Internal, "bad hash in session")
                    })?;
                    let span = spans.get(&h).ok_or_else(|| {
                        CairnError::new(
                            ErrorKind::Internal,
                            format!("session hash {hash_hex} not in local chunk set"),
                        )
                    })?;
                    let raw =
                        &body[span.offset as usize..(span.offset + u64::from(span.len)) as usize];
                    let stored = compress::compress_chunk(raw, policy, d)?;
                    let checksum = cairn_core::hash::hex_encode(&Sha256::digest(&stored));
                    self.upload_with_aimd(&url, &stored, &checksum).await?;
                    Ok((hash_hex, stored.len() as u64))
                })
                .buffered(bound)
                .collect()
                .await;
            let mut stored_sizes: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            for r in uploaded {
                let (hash_hex, len) = r?;
                stored_sizes.insert(hash_hex, len);
            }
            let receipts: Vec<UploadReceipt> = session
                .puts
                .iter()
                .map(|(hash_hex, _)| UploadReceipt {
                    chunk_hash: hash_hex.clone(),
                    size: stored_sizes.get(hash_hex).copied().unwrap_or(0),
                    etag: String::new(),
                })
                .collect();
            // Per-call retry (was single-shot AFTER all uploads succeeded:
            // losing `complete` discarded a fully-uploaded session's worth of
            // work back to the next pass). Same Auto-only discipline; the
            // (session, receipts) pair is idempotent server-side.
            let mut seed = 0xcbf29ce484222325u64;
            for b in session.id.bytes() {
                seed ^= u64::from(b);
                seed = seed.wrapping_mul(0x100000001b3);
            }
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut attempts = 0u32;
            let out = loop {
                match self.plane.complete(&session.id, &receipts).await {
                    Ok(out) => break out,
                    Err(e) => {
                        attempts += 1;
                        if !should_retry(e.retry_class(), attempts - 1) {
                            return Err(e);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                            attempts, &mut rng,
                        )))
                        .await;
                    }
                }
            };
            stats.net_ms += t_net.elapsed().as_millis() as u64;
            if !out.rejected.is_empty() {
                return Err(CairnError::new(
                    ErrorKind::ChecksumMismatch,
                    format!("{} chunks rejected at complete", out.rejected.len()),
                ));
            }
        }

        // manifest (ADR-0004 + normalization: chunk hashes cover the INNER payload when a
        // container transform is active; the transform travels in the manifest v2 header)
        let entries: Vec<ManifestEntry> = sh
            .spans
            .iter()
            .zip(sh.chunk_hashes.iter())
            .map(|(s, h)| ManifestEntry {
                offset: s.offset,
                len: s.len,
                chunk_hash: *h,
            })
            .collect();
        // manifest (ADR-0004 + normalization: chunk hashes cover the INNER payload when a
        // container transform is active; the transform travels in the manifest v2 header).
        // build_tree (not plain build): files fanning out past MANIFEST_MAX_ENTRIES
        // (>8,192 chunks) reference CHILD manifest objects — those bytes MUST be stored
        // or the tree is unresolvable at hydrate and invisible to GC (review round).
        let built = Manifest::build_tree_with_transform(
            entries,
            policy,
            dict.as_ref().map(|d| d.dict_hash),
            transform,
        );
        // children first (leaf-first order), parent last — crash between the two leaves
        // unreferenced children that GC reclaims, never a dangling parent
        let mut seed = 0xcbf29ce484222325u64;
        for b in path.bytes() {
            seed ^= u64::from(b);
            seed = seed.wrapping_mul(0x100000001b3);
        }
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        for (child_hash, child_bytes) in &built.child_objects {
            let t = std::time::Instant::now();
            self.cas.put(child_hash, child_bytes)?;
            stats.cas_ms += t.elapsed().as_millis() as u64;
            // Content-addressed PUT → idempotent; retried like chunk PUTs.
            let mut attempts = 0u32;
            loop {
                let t = std::time::Instant::now();
                match self
                    .plane
                    .put_manifest(&self.tenant_id, &child_hash.hex(), child_bytes)
                    .await
                {
                    Ok(()) => {
                        stats.net_ms += t.elapsed().as_millis() as u64;
                        break;
                    }
                    Err(e) => {
                        stats.net_ms += t.elapsed().as_millis() as u64;
                        attempts += 1;
                        if !should_retry(e.retry_class(), attempts - 1) {
                            return Err(e);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                            attempts, &mut rng,
                        )))
                        .await;
                    }
                }
            }
        }
        let manifest = built.manifest;
        let (manifest_hash, manifest_bytes) = manifest.serialize();
        // mirror the manifest object into the local CAS (hydration path reads it offline)
        let t = std::time::Instant::now();
        self.cas.put(&manifest_hash, &manifest_bytes)?;
        stats.cas_ms += t.elapsed().as_millis() as u64;
        let mut attempts = 0u32;
        loop {
            let t = std::time::Instant::now();
            match self
                .plane
                .put_manifest(&self.tenant_id, &manifest_hash.hex(), &manifest_bytes)
                .await
            {
                Ok(()) => {
                    stats.net_ms += t.elapsed().as_millis() as u64;
                    break;
                }
                Err(e) => {
                    stats.net_ms += t.elapsed().as_millis() as u64;
                    attempts += 1;
                    if !should_retry(e.retry_class(), attempts - 1) {
                        return Err(e);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                        attempts, &mut rng,
                    )))
                    .await;
                }
            }
        }
        tracing::debug!(
            path = %path,
            chunks = sh.spans.len(),
            read_ms = stats.read_ms,
            hash_ms = stats.hash_ms,
            cas_ms = stats.cas_ms,
            net_ms = stats.net_ms,
            "ingest stages"
        );

        // Stat-only drift short-circuit (round 18, the W4 catch): a fork
        // marker on this path means apply REFUSED a remote upsert (§7.1 guard
        // or dirty-keep). If the freshly hashed content is IDENTICAL to the
        // row's recorded manifest, there was never a local edit -- nothing to
        // preserve. Falling through would re-assert bytes the server already
        // has, clear the fork, and leave the refused remote permanently past
        // the cursor: silent divergence, no conflict copy, no warning (the
        // Windows-matrix W4 red: A held v1 forever while B held v2). Instead:
        // refresh the row's stat from disk (the touch -- so the guard's exact
        // comparison cannot re-fire), keep the row synced at this manifest,
        // and re-pin replay to the fork point -- the conflict_copy
        // re-delivery, minus the copy (content never changed). The next pull
        // re-delivers the refused upsert onto a clean, stat-fresh row and
        // converges normally. A REAL edit re-chunks to a different manifest
        // and takes the fork-claim append below, W5 contract untouched.
        if let Some(fork) = crate::apply::fork_seq(&self.store, &self.local_ns, path) {
            let identical_to_row = self
                .store
                .get_file(&self.local_ns, path)
                .and_then(|row| row.manifest_hash)
                .is_some_and(|row_manifest| row_manifest == manifest_hash.hex());
            if fork > 0 && identical_to_row {
                self.store.mark_synced_with_stat(
                    &self.local_ns,
                    path,
                    &manifest_hash.hex(),
                    pushed_size,
                    pushed_mtime,
                )?;
                crate::apply::clear_fork(&self.store, &self.local_ns, path)?;
                let _ = self
                    .store
                    .set_cursor(&self.author_id, &self.local_ns, fork - 1);
                tracing::info!(
                    path = %path,
                    fork,
                    "stat-only drift resolved: content identical, replay re-pinned to \
                     the fork point so the refused remote re-delivers"
                );
                return Ok(());
            }
        }

        // outbox → append (fencing token included when leased)
        // Content-lineage fork (round 13, the W5 catch): base_seq must declare
        // what the local BYTES descend from, not what this device has READ.
        // When apply refused a remote upsert for this path (undiscovered-local-
        // edit guard or the dirty-keep arm), the local content forks at the
        // pre-refusal head -- claiming the cursor would let the server accept
        // the append linearly and silently supersede the other device's
        // version with NO conflict copy. Claim min(cursor, fork-1) so the
        // server's seq>base rule fires (SPEC 7.1) and the conflict copy
        // preserves BOTH versions.
        let mut base_seq = self.store.get_cursor(&self.author_id, &self.local_ns);
        if let Some(fork) = crate::apply::fork_seq(&self.store, &self.local_ns, path) {
            if fork > 0 && fork - 1 < base_seq {
                tracing::info!(
                    path = %path,
                    fork,
                    cursor = base_seq,
                    "append claims the content-lineage fork, not the read cursor"
                );
                base_seq = fork - 1;
            }
        }
        let lease_token = self.store.get_lease(path).map_or(0, |(t, _)| t);
        // Content-derived idempotency key (WO6-4): the watcher and the scan can both
        // enqueue the same fresh file before either append lands; a random id made
        // the server accept BOTH (two journal entries for one edit — caught by the
        // soak's zero-dup gate). Same edit ⇒ same id ⇒ server dedups. A re-save
        // changes mtime/manifest ⇒ new id ⇒ legitimate re-append.
        let mtime_ms = std::fs::metadata(self.rooted(path))
            .map(|m| crate::scan::mtime_millis(&m))
            .unwrap_or(0);
        let request_id = cairn_core::ids::request_id_for(
            &self.tenant_id,
            &self.project_id,
            path,
            &manifest_hash.hex(),
            raw_len as u64,
            mtime_ms,
        );
        let op = upsert_op(path, &manifest_hash.hex(), raw_len as u64, base_seq);
        let entry = cairn_store::OutboxEntry {
            request_id: request_id.clone(),
            project_id: self.local_ns.clone(),
            op: {
                let mut buf = Vec::new();
                prost::Message::encode(&op, &mut buf)
                    .map_err(|e| CairnError::new(ErrorKind::Internal, format!("op encode: {e}")))?;
                buf
            },
            state: "pending".into(),
            attempts: 0,
            created_at: self.store.clock().now_millis(),
        };
        self.outbox.enqueue(entry)?;
        // durable before the send: a crash between enqueue and append leaves the row
        // outbox_pending (NOT dirty), so recovery resends the SAME request_id (server
        // dedup) instead of re-chunking and double-appending (I2, §9.1)
        self.store
            .set_file_state(&self.local_ns, path, LocalState::OutboxPending.as_str())?;
        self.send_outbox_entry(&request_id, op, lease_token, path, stats)
            .await?;

        self.headers.put(
            &manifest_hash.hex(),
            &head,
            if tail.is_empty() { None } else { Some(&tail) },
        )?;

        self.store.mark_synced_with_stat(
            &self.local_ns,
            path,
            &manifest_hash.hex(),
            pushed_size,
            pushed_mtime,
        )?;
        // the append resolved the fork (accepted: the head now descends from
        // these bytes; conflicted: conflict_copy handles the original path)
        let _ = crate::apply::clear_fork(&self.store, &self.local_ns, path);
        Ok(())
    }

    async fn upload_with_aimd(
        &self,
        url: &str,
        stored: &[u8],
        checksum: &str,
    ) -> Result<(), CairnError> {
        let mut attempts = 0u32;
        self.gate.acquire_async().await;
        let mut rng = rand::rngs::StdRng::seed_from_u64(
            cairn_core::clock::WallClock
                .now_millis()
                .min(i64::from(u32::MAX)) as u64,
        );
        loop {
            let r = self.plane.put_presigned(url, stored, checksum).await;
            match r {
                Ok(()) => {
                    self.gate.finish(true);
                    return Ok(());
                }
                Err(e) => {
                    self.gate.finish(false);
                    attempts += 1;
                    if should_retry(e.retry_class(), attempts - 1) {
                        let delay = backoff_millis(attempts, &mut rng);
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        // re-acquire for the next attempt (parks, no polling)
                        self.gate.acquire_async().await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Parallel outbox drain (priority #1): entries are grouped by path and
    /// each path-group appends sequentially (FIFO per path preserved —
    /// renames/deletes on one path never reorder), while distinct paths
    /// append concurrently under a 4-wide semaphore. The common Ok path
    /// (ack + mark_synced) runs inline in the task; anything else
    /// (STALE_LEASE / CONFLICT / transport error) is deferred to the
    /// existing sequential `send_outbox_entry`, which owns all tricky
    /// logic (conflict copies, cursor re-pins). Re-append is safe:
    /// request_id dedup makes the second append a no-op fetch of the
    /// same verdict. Error semantics match the old loop: first hard
    /// error aborts the pass.
    async fn flush_outbox(&self, stats: &mut PassStats) -> Result<(), CairnError> {
        use std::collections::BTreeMap;

        struct Job {
            request_id: String,
            op: cairn_proto::pb::JournalOp,
            lease_token: u64,
            path: String,
            manifest: Option<String>,
        }

        // 1. Collect + decode FIFO (pending() is created_at ASC).
        let mut groups: BTreeMap<String, Vec<Job>> = BTreeMap::new();
        for e in self.outbox.pending(&self.local_ns, 256) {
            if let Ok(op) = cairn_proto::pb::JournalOp::decode(e.op.as_slice()) {
                let path = op_path(&op);
                let lease_token = self.store.get_lease(&path).map_or(0, |(t, _)| t);
                // Manifest fast-path extraction: ONLY FileUpsert has a row to
                // complete. StateRecord ops (ADR-0031) carry no manifest and no
                // file row — they append+ack on this fast path with
                // manifest=None (nothing to mark), and any Err falls through
                // to the sequential `send_outbox_entry` below, whose
                // StateRecord arm leaves the entry pending without touching
                // files (no dirty marking, no conflict copy).
                let manifest = match op.op.as_ref() {
                    Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) => {
                        Some(u.manifest_hash.clone())
                    }
                    _ => None,
                };
                groups.entry(path.clone()).or_default().push(Job {
                    request_id: e.request_id,
                    op,
                    lease_token,
                    path,
                    manifest,
                });
            }
        }
        if groups.is_empty() {
            return Ok(());
        }

        // 2. One task per path-group, bounded concurrency.
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        let mut set = tokio::task::JoinSet::new();
        for (_path, group) in groups {
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("flush semaphore: {e}")))?;
            let tenant = self.tenant_id.clone();
            let project = self.project_id.clone();
            let author = self.author_id.clone();
            let local_ns = self.local_ns.clone();
            let store = self.store.clone();
            let outbox = self.outbox.clone();
            let plane = Arc::clone(&self.plane);
            set.spawn(async move {
                let _permit = permit;
                let mut appended: u64 = 0;
                // Entries that did NOT clean-append: re-driven sequentially
                // by the caller through the full handler below.
                let mut retry: Vec<Job> = Vec::new();
                let mut first_err: Option<CairnError> = None;
                for job in group {
                    match plane
                        .append(
                            &tenant,
                            &project,
                            &author,
                            &job.request_id,
                            job.op.clone(),
                            job.lease_token,
                        )
                        .await
                    {
                        Ok((_seq, _dedup)) => {
                            if let Err(e) = outbox.ack(&job.request_id) {
                                first_err = Some(CairnError::new(
                                    ErrorKind::Io,
                                    format!("ack {}: {e}", job.request_id),
                                ));
                                retry.push(job);
                                break;
                            }
                            // Same post-ack pipeline as the sequential path:
                            // the row's content identity lands with synced
                            // state so self-pull never mistakes our own
                            // entry for a remote update.
                            if let Some(mh) = job.manifest.clone() {
                                let full = workspace_dir(&store, &local_ns).join(&job.path);
                                match std::fs::metadata(&full) {
                                    Ok(m) => {
                                        if let Err(e) = store.mark_synced_with_stat(
                                            &local_ns,
                                            &job.path,
                                            &mh,
                                            m.len(),
                                            crate::scan::mtime_millis(&m),
                                        ) {
                                            first_err = Some(e);
                                            retry.push(job);
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        if let Err(e) = store.mark_synced(&local_ns, &job.path, &mh)
                                        {
                                            first_err = Some(e);
                                            retry.push(job);
                                            break;
                                        }
                                    }
                                }
                            }
                            appended += 1;
                        }
                        Err(_) => {
                            retry.push(job);
                            break; // stop group on first non-Ok: FIFO per path
                        }
                    }
                }
                (appended, retry, first_err)
            });
        }

        // 3. Join; sum fast-path appends; collect sequential retries.
        let mut retry_jobs: Vec<Job> = Vec::new();
        while let Some(res) = set.join_next().await {
            let (appended, mut retry, first_err) =
                res.map_err(|e| CairnError::new(ErrorKind::Internal, format!("flush task: {e}")))?;
            stats.appended += appended as u32;
            if let Some(e) = first_err {
                return Err(e);
            }
            retry_jobs.append(&mut retry);
        }

        // 4. Sequential fallback through the full handler (conflict copies,
        // cursor re-pins, lease surfacing — byte-identical to the old loop).
        // Re-append hits request_id dedup; verdicts replay deterministically.
        retry_jobs.sort_by(|a, b| a.request_id.cmp(&b.request_id));
        for job in retry_jobs {
            // Re-resolve the lease fresh (it may have changed while racing).
            let lease_token = self
                .store
                .get_lease(&job.path)
                .map_or(job.lease_token, |(t, _)| t);
            self.send_outbox_entry(&job.request_id, job.op, lease_token, &job.path, stats)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn send_outbox_entry(
        &self,
        request_id: &str,
        op: cairn_proto::pb::JournalOp,
        lease_token: u64,
        path: &str,
        stats: &mut PassStats,
    ) -> Result<(), CairnError> {
        let _base_seq = self.store.get_cursor(&self.author_id, &self.local_ns);
        // State records (ADR-0031 Phase 1) carry NO file row: a failed send
        // must leave the outbox entry pending and touch nothing else — no
        // dirty marking (there is no row), no conflict copy (there is no file
        // to rename; and a spec-correct server never CONFLICTs this family —
        // only a legacy pre-ADR server could, and that is surfaced honestly).
        let is_state_record = matches!(
            op.op.as_ref(),
            Some(cairn_proto::pb::journal_op::Op::StateRecord(_))
        );
        // manifest identity extracted up front (op is consumed by the append)
        let upsert_manifest: Option<String> = match op.op.as_ref() {
            Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) => Some(u.manifest_hash.clone()),
            _ => None,
        };
        // Per-call retry (was single-shot: one lost packet failed the pass
        // even though the outbox row survives for the next pass). Auto-class
        // only, capped, full-jitter; the append is idempotent by request_id
        // (server dedups), so retrying is safe. Deterministic rng per
        // request keeps sim schedules reproducible.
        let mut seed = 0xcbf29ce484222325u64;
        for b in request_id.bytes() {
            seed ^= u64::from(b);
            seed = seed.wrapping_mul(0x100000001b3);
        }
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut attempts = 0u32;
        let append_out = loop {
            match self
                .plane
                .append(
                    &self.tenant_id,
                    &self.project_id,
                    &self.author_id,
                    request_id,
                    op.clone(),
                    lease_token,
                )
                .await
            {
                Ok(out) => break Ok(out),
                Err(e) => {
                    attempts += 1;
                    if !should_retry(e.retry_class(), attempts - 1) {
                        break Err(e);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                        attempts, &mut rng,
                    )))
                    .await;
                }
            }
        };
        match append_out {
            Ok((_seq, _dedup)) => {
                self.outbox.ack(request_id)?;
                // complete the row's pipeline for FileUpserts: content identity lands with
                // the synced state so the self-pull never mistakes our own entry for a
                // remote update (fresh or deduplicated — both mean the server has it)
                if let Some(mh) = upsert_manifest {
                    // crash-resume resend: refresh the row's stat from disk if the file
                    // still exists (post-push invariant row.stat == file.stat); a missing
                    // file keeps its row — the next sweep's stat walk classifies it.
                    match std::fs::metadata(self.rooted(path)) {
                        Ok(m) => {
                            self.store.mark_synced_with_stat(
                                &self.local_ns,
                                path,
                                &mh,
                                m.len(),
                                crate::scan::mtime_millis(&m),
                            )?;
                        }
                        Err(_) => {
                            self.store.mark_synced(&self.local_ns, path, &mh)?;
                        }
                    }
                }
                stats.appended += 1;
                Ok(())
            }
            Err(e) if is_state_record => {
                // ADR-0031: nothing to dirty, nothing to copy — the entry stays
                // pending (acked only on Ok) and the next pass re-sends it
                // (request_id dedup). Surfaced so the pass reports the truth.
                Err(e)
            }
            Err(e) if e.code() == "STALE_LEASE" => {
                // surface to user per §14: keep the outbox entry, mark state, stop this path
                self.store
                    .set_file_state(&self.local_ns, path, LocalState::Dirty.as_str())?;
                Err(e)
            }
            Err(e) if e.code() == "CONFLICT" => {
                // conflict copy per §7.1: rename on the new path and re-append
                let copy_path = self.conflict_copy(path, stats).await?;
                // CONTRACT-DEBT #1: offer a semantic merge of the conflict when
                // the flag is on and the file is a timeline. Fire-and-forget by
                // contract: a failed offer NEVER fails the conflict resolution
                // (the copy already preserved the local edit); the merge is the
                // passive affordance on top.
                let op_base_seq = match op.op.as_ref() {
                    Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) => u.base_seq,
                    _ => 0,
                };
                match self
                    .create_offer_if_mergeable(op_base_seq, path, &copy_path)
                    .await
                {
                    Ok(true) => stats.merge_offered += 1,
                    Ok(false) => {}
                    Err(err) => {
                        tracing::warn!(
                            path = %path,
                            "merge offer failed (conflict resolution unaffected): {err}"
                        );
                    }
                }
                self.outbox.ack(request_id)?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Returns the conflict copy's project-relative path (CONTRACT-DEBT #1:
    /// the merge-offer step needs it to read this device's side of the merge
    /// without recomputing the name).
    async fn conflict_copy(&self, path: &str, stats: &mut PassStats) -> Result<String, CairnError> {
        let date = date_of(self.store.clock().now_millis());
        let name = path.rsplit('/').next().unwrap_or(path);
        let copy_name = cairn_core::pathutil::conflict_copy_name(name, &self.author_id, &date);
        let copy_path = match path.rfind('/') {
            Some(idx) => format!("{}/{}", &path[..idx], copy_name),
            None => copy_name,
        };
        let full = self.rooted(path);
        let copy_full = self.rooted(&copy_path);
        std::fs::rename(&full, &copy_full)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("conflict copy: {e}")))?;
        // case-collision guard (§10): if the destination exists case-insensitively, suffix it
        let rows: Vec<String> = self
            .store
            .list_files(&self.local_ns)
            .into_iter()
            .map(|f| f.path)
            .collect();
        if !cairn_core::pathutil::find_case_collisions(&rows).is_empty() {
            tracing::warn!(path = %copy_path, "case-insensitive collision detected");
        }
        // The copy MUST get a real row NOW: process_file tracks an existing row through
        // the pipeline (mark_synced_with_stat updates by path) — without a row the
        // device syncs the copy's content but keeps NO local record of the file, and
        // only ever learns it back via journal replay (a sim-green state that hid a
        // real divergence; caught when the sweep + byte budgets exposed it).
        let meta = std::fs::metadata(&copy_full)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("stat copy: {e}")))?;
        self.store.put_file(&cairn_store::FileRow {
            path: copy_path.clone(),
            project_id: self.local_ns.clone(),
            manifest_hash: None,
            size: meta.len(),
            mode: "file".into(),
            mtime: crate::scan::mtime_millis(&meta),
            local_state: LocalState::Dirty.as_str().into(),
        })?;
        // The ORIGINAL path's local content now lives at the copy path; this device has
        // no local claim on the original anymore. Leaving the row `Conflict` would keep
        // push_phase re-processing a file that no longer exists (fs::read → error loop,
        // blocking pull forever — the divergence the sim caught). `Clean` hands the path
        // back to the journal: the next pull of the winner's upsert flips it to
        // placeholder and hydration materializes the winner's content (§7.1 end state:
        // original = winner, copy = ours, both preserved).
        self.store
            .set_file_state(&self.local_ns, path, LocalState::Clean.as_str())?;
        // Re-delivery (round 13, the W5 lag case): when the fork marker exists,
        // the refused remote entries are already PAST this device's cursor
        // (the pull that triggered the guard consumed them) -- the original
        // path would never re-receive the winner's head. Re-pin the journal
        // replay to the fork point: the next pull re-delivers everything the
        // local bytes refused, now that the local claim lives at the copy path
        // (apply is idempotent; own-device entries rewrite nothing). The
        // CLASSIC offline case has no marker: the winner's entry is still
        // ahead of the cursor and the next pull delivers it naturally.
        if let Some(fork) = crate::apply::fork_seq(&self.store, &self.local_ns, path) {
            if fork > 0 {
                let _ = self
                    .store
                    .set_cursor(&self.author_id, &self.local_ns, fork - 1);
                tracing::info!(
                    path = %path,
                    fork,
                    "conflict resolved: journal replay re-pinned to the fork point"
                );
            }
        }
        // the original path's local claim is over: any fork is resolved
        let _ = crate::apply::clear_fork(&self.store, &self.local_ns, path);
        // re-append for the new path (content already chunked + uploaded); boxed because the
        // conflict path is strictly one level deep per file
        let mut inner = PassStats::default();
        Box::pin(self.process_file(&copy_path, &mut inner)).await?;
        stats.conflicts_resolved += 1;
        stats.appended += inner.appended;
        Ok(copy_path)
    }

    async fn pull_phase(&self, stats: &mut PassStats) -> Result<(), CairnError> {
        let cursor = self.store.get_cursor(&self.author_id, &self.local_ns);
        // Read-only → retried like everything else on the flaky path.
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x9114E55u64);
        let mut attempts = 0u32;
        let entries = loop {
            match self
                .plane
                .fetch_batch(&self.tenant_id, &self.project_id, cursor, 512)
                .await
            {
                Ok(e) => break e,
                Err(e) => {
                    attempts += 1;
                    if !should_retry(e.retry_class(), attempts - 1) {
                        return Err(e);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_millis(
                        attempts, &mut rng,
                    )))
                    .await;
                }
            }
        };
        for e in &entries {
            // Own-device ops are already folded locally: the push path marked the row
            // synced (mark_synced) when the append was acked. Replaying them here would
            // overwrite the row's LOCAL stat fields (mtime from the scan, size from the
            // file) with journal-level values (server_ts) — and any stat-based
            // reconciliation (rescan, reconcile sweep) then sees a phantom size/mtime
            // drift on an unchanged file, re-dirties it, and re-pushes: a push↔pull
            // livelock that generates journal entries forever (caught by the WO1
            // acceptance byte/journal budgets at gate 1: 1302 journal ops for 10 files).
            // A device that loses its local table rebuilds via reset_to_snapshot, not
            // by replaying its own ops.
            if e.device_id == self.author_id {
                continue;
            }
            crate::apply::apply_entry(&self.store, &self.local_ns, &self.author_id, e)?;
            stats.applied_entries += 1;
        }
        if let Some(last) = entries.last() {
            self.store
                .set_cursor(&self.author_id, &self.local_ns, last.seq)?;
        }
        Ok(())
    }

    /// Cursor replay on demand (CONTRACT-DEBT #1): `merge_offer::accept_offer`
    /// pulls BEFORE re-pushing the merged timeline so its append claims a base
    /// the server's seq>base rule accepts instead of re-conflicting. Idempotent.
    pub(crate) async fn pull_now(&self) -> Result<(), CairnError> {
        let mut stats = PassStats::default();
        self.pull_phase(&mut stats).await
    }

    /// Project-relative -> absolute workspace path. Shared with the
    /// merge-offer accept path (same crate, different module).
    pub(crate) fn rooted(&self, path: &str) -> std::path::PathBuf {
        workspace_dir(&self.store, &self.local_ns).join(path)
    }
}

fn op_path(op: &cairn_proto::pb::JournalOp) -> String {
    match op.op.as_ref() {
        Some(cairn_proto::pb::journal_op::Op::FileUpsert(o)) => o.path.clone(),
        Some(cairn_proto::pb::journal_op::Op::FileDelete(o)) => o.path.clone(),
        Some(cairn_proto::pb::journal_op::Op::Rename(r)) => r.old_path.clone(),
        Some(cairn_proto::pb::journal_op::Op::LeaseEvent(l)) => l.path.clone(),
        Some(cairn_proto::pb::journal_op::Op::StateRecord(sr)) => {
            // synthetic grouping key (ADR-0031): never a real file, never leased
            cairn_core::pathutil::state_record_path(&sr.family, &sr.key)
        }
        None => String::new(),
    }
}

fn date_of(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_of_computes_civil_calendar_correctly() {
        // Unix epoch: 1970-01-01
        assert_eq!(date_of(0), "1970-01-01");
        // 2000-02-29 (leap year 400-year rule)
        assert_eq!(date_of(951_782_400_000), "2000-02-29");
        // 2024-02-29 (regular leap year)
        assert_eq!(date_of(1_709_164_800_000), "2024-02-29");
        // 2024-03-01 (day after leap day)
        assert_eq!(date_of(1_709_251_200_000), "2024-03-01");
        // 2100-02-28 (century non-leap year: 47540 days from epoch)
        assert_eq!(date_of(4_107_456_000_000), "2100-02-28");
        // 2100-03-01 (day 47541 from epoch)
        assert_eq!(date_of(4_107_542_400_000), "2100-03-01");
    }
}
