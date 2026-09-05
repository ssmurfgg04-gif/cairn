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
        for f in self.store.list_files(&self.local_ns) {
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

    async fn process_file(&self, path: &str, stats: &mut PassStats) -> Result<(), CairnError> {
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
        let bytes = tokio::fs::read(&full)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("read {path}: {e}")))?;
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
        let (sh, content) = crate::offload::hash_stream_owned(content, fine).await?;

        // local CAS insert (verified) — content-addressed, idempotent
        for (span, h) in sh.spans.iter().zip(sh.chunk_hashes.iter()) {
            let raw = &content[span.offset as usize..(span.offset + u64::from(span.len)) as usize];
            if self.cas.contains(h) {
                stats.skipped_chunks += 1;
            } else {
                self.cas.put(h, raw)?;
                stats.uploaded_chunks += 1;
            }
        }

        // upload missing chunks via session + AIMD
        let hash_hexes: Vec<String> = sh.chunk_hashes.iter().map(Hash::hex).collect();
        let missing = self
            .plane
            .batch_exists(&self.tenant_id, &hash_hexes)
            .await?;
        if !missing.is_empty() {
            let session = self
                .plane
                .create_session(&self.tenant_id, &self.author_id, &self.project_id, &missing)
                .await?;
            // receipts must report the size the BUCKET holds — the compressed/stored bytes —
            // because CompleteUpload sample-verifies via HEAD against the object key.
            // (Raw span sizes are only correct for Compression::None; reporting them for
            // zstd-stored chunks rejects every upload.)
            let mut stored_sizes: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            for (hash_hex, url) in &session.puts {
                let h = Hash::from_hex(hash_hex)
                    .ok_or_else(|| CairnError::new(ErrorKind::Internal, "bad hash in session"))?;
                let Some(span) = sh
                    .spans
                    .iter()
                    .zip(sh.chunk_hashes.iter())
                    .find(|(_, ch)| **ch == h)
                    .map(|(s, _)| s)
                else {
                    continue;
                };
                let raw =
                    &content[span.offset as usize..(span.offset + u64::from(span.len)) as usize];
                let stored = compress::compress_chunk(raw, policy, dict.as_ref())?;
                let checksum = cairn_core::hash::hex_encode(&Sha256::digest(&stored));
                self.upload_with_aimd(url, &stored, &checksum).await?;
                stored_sizes.insert(hash_hex.clone(), stored.len() as u64);
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
            let out = self.plane.complete(&session.id, &receipts).await?;
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
        for (child_hash, child_bytes) in &built.child_objects {
            self.cas.put(child_hash, child_bytes)?;
            self.plane
                .put_manifest(&self.tenant_id, &child_hash.hex(), child_bytes)
                .await?;
        }
        let manifest = built.manifest;
        let (manifest_hash, manifest_bytes) = manifest.serialize();
        // mirror the manifest object into the local CAS (hydration path reads it offline)
        self.cas.put(&manifest_hash, &manifest_bytes)?;
        self.plane
            .put_manifest(&self.tenant_id, &manifest_hash.hex(), &manifest_bytes)
            .await?;

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
        while !self.gate.try_acquire() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
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
                        // re-acquire for the next attempt
                        while !self.gate.try_acquire() {
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn flush_outbox(&self, stats: &mut PassStats) -> Result<(), CairnError> {
        for e in self.outbox.pending(&self.local_ns, 256) {
            if let Ok(op) = cairn_proto::pb::JournalOp::decode(e.op.as_slice()) {
                let path = op_path(&op);
                let lease_token = self.store.get_lease(&path).map_or(0, |(t, _)| t);
                self.send_outbox_entry(&e.request_id, op, lease_token, &path, stats)
                    .await?;
            }
        }
        Ok(())
    }

    async fn send_outbox_entry(
        &self,
        request_id: &str,
        op: cairn_proto::pb::JournalOp,
        lease_token: u64,
        path: &str,
        stats: &mut PassStats,
    ) -> Result<(), CairnError> {
        let _base_seq = self.store.get_cursor(&self.author_id, &self.local_ns);
        // manifest identity extracted up front (op is consumed by the append)
        let upsert_manifest: Option<String> = match op.op.as_ref() {
            Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) => Some(u.manifest_hash.clone()),
            _ => None,
        };
        match self
            .plane
            .append(
                &self.tenant_id,
                &self.project_id,
                &self.author_id,
                request_id,
                op,
                lease_token,
            )
            .await
        {
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
            Err(e) if e.code() == "STALE_LEASE" => {
                // surface to user per §14: keep the outbox entry, mark state, stop this path
                self.store
                    .set_file_state(&self.local_ns, path, LocalState::Dirty.as_str())?;
                Err(e)
            }
            Err(e) if e.code() == "CONFLICT" => {
                // conflict copy per §7.1: rename on the new path and re-append
                self.conflict_copy(path, stats).await?;
                self.outbox.ack(request_id)?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn conflict_copy(&self, path: &str, stats: &mut PassStats) -> Result<(), CairnError> {
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
        Ok(())
    }

    async fn pull_phase(&self, stats: &mut PassStats) -> Result<(), CairnError> {
        let cursor = self.store.get_cursor(&self.author_id, &self.local_ns);
        let entries = self
            .plane
            .fetch_batch(&self.tenant_id, &self.project_id, cursor, 512)
            .await?;
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

    fn rooted(&self, path: &str) -> std::path::PathBuf {
        workspace_dir(&self.store, &self.local_ns).join(path)
    }
}

fn op_path(op: &cairn_proto::pb::JournalOp) -> String {
    match op.op.as_ref() {
        Some(cairn_proto::pb::journal_op::Op::FileUpsert(o)) => o.path.clone(),
        Some(cairn_proto::pb::journal_op::Op::FileDelete(o)) => o.path.clone(),
        Some(cairn_proto::pb::journal_op::Op::Rename(r)) => r.old_path.clone(),
        Some(cairn_proto::pb::journal_op::Op::LeaseEvent(l)) => l.path.clone(),
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
