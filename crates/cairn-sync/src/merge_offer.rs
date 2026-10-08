//! Conflict auto-offer (CONTRACT-DEBT #1): when the sync engine's append is
//! refused with CONFLICT on a TIMELINE file and the `semantic_merge` flag is
//! ON, the engine OFFERS a three-way semantic merge instead of leaving the
//! editor to reconcile two files by hand.
//!
//! The offer is strictly PASSIVE: creating it writes one `merge_offers` row
//! (manifest hashes + report JSON, never merged bytes — the merge is
//! byte-deterministic, so accepting RECOMPUTES it from the same inputs) and
//! nothing else. The classic conflict-copy behavior is untouched.
//!
//! OFFER-WORTHINESS DECISION (module contract): a merge whose report still
//! ends in `Outcome::Conflicts` is NOT an offer — an offer whose result
//! still needs a human is a second conflict wearing a badge. Those cases
//! (and C10 refusals) keep the conflict copy alone; the classic tl-merge
//! UI remains the path for them. Only Clean / Notes outcomes are offered.
//!
//! Accept semantics: ONE journal entry for the merged timeline (the normal
//! process_file pipeline — content-derived request id), the conflict copy's
//! row + file removed, both devices converge on the merged head. Decline:
//! the offer row is removed, the conflict copy stays (§7.1 contract).

use std::collections::HashMap;

use cairn_core::{CairnError, ErrorKind};
use cairn_tl::merge::{merge_with, MergeOptions, Outcome};
use cairn_tl::model::Timeline;

use crate::engine::Engine;

/// True when `path` names a timeline the tl bridge can parse (SPEC: the
/// same ext predicate search uses — case-insensitive otio/fcpxml).
#[must_use]
pub fn is_timeline_path(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    if ext.len() == path.len() {
        return false; // no dot in the path at all
    }
    ext.eq_ignore_ascii_case("otio") || ext.eq_ignore_ascii_case("fcpxml")
}

/// Parse timeline bytes by the path's extension (the CLI's dispatch: fcpxml
/// goes through the bridge, everything else is OTIO JSON).
fn parse_timeline(path: &str, bytes: &[u8]) -> Option<Timeline> {
    let text = std::str::from_utf8(bytes).ok()?;
    if path.to_ascii_lowercase().ends_with(".fcpxml") {
        cairn_tl::fcpxml::parse_fcpxml(text).ok()
    } else {
        cairn_tl::parse::parse_otio(text).ok()
    }
}

impl Engine {
    /// Called from the CONFLICT arm AFTER the conflict copy exists and BEFORE
    /// the failed append is acked. Fire-and-forget by contract: every
    /// "cannot honestly offer" shape returns `Ok(false)` (flag off, not a
    /// timeline, no provable ancestor, parse failure, refusal, still-conflicted);
    /// only store/IO failures surface as `Err` — the caller logs and moves on,
    /// never failing the conflict resolution because the offer failed.
    ///
    /// `op_base_seq` is the failed FileUpsert's `base_seq` — the journal seq
    /// the local content claimed as its ancestor. The server's conflict rule
    /// guarantees a DIFFERENT-device entry with seq > base_seq exists for this
    /// path (that IS the conflict), so one fetch at `base_seq - 1` serves both
    /// lookups: the ancestor (seq == base_seq) and the winner (latest seq >
    /// base_seq from another device).
    pub async fn create_offer_if_mergeable(
        &self,
        op_base_seq: u64,
        path: &str,
        copy_path: &str,
    ) -> Result<bool, CairnError> {
        // 1. The flag is read per-invocation like normalize_containers — the
        //    ctl flip lands on the next conflict, no restart.
        if self.store.meta_get("flag:semantic_merge").as_deref() != Some("true") {
            return Ok(false);
        }
        if !is_timeline_path(path) {
            return Ok(false);
        }
        // 2. No ancestor seq -> nothing to prove the common base with. An
        //    offer without a provable ancestor would guess, and guessing in a
        //    merge is how edits get silently rewritten.
        if op_base_seq == 0 {
            return Ok(false);
        }
        let entries = match self
            .plane
            .fetch_batch(
                &self.tenant_id,
                &self.project_id,
                op_base_seq.saturating_sub(1),
                512,
            )
            .await
        {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(path = %path, "merge offer: journal lookup failed: {e}");
                return Ok(false);
            }
        };
        // base = the entry our content claimed (seq == base_seq, same path,
        // an upsert — a delete/rename ancestor is not a mergeable base).
        let base_manifest = entries.iter().rev().find_map(|e| {
            if e.seq != op_base_seq {
                return None;
            }
            match e.op.op.as_ref() {
                Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) if u.path == path => {
                    Some(u.manifest_hash.clone())
                }
                _ => None,
            }
        });
        // theirs = the LAST different-device upsert for the path beyond the
        // base (the server's rule keys on any such entry; the latest IS the
        // journal head those bytes descend from).
        let theirs_manifest = entries.iter().rev().find_map(|e| {
            if e.seq <= op_base_seq || e.device_id == self.author_id {
                return None;
            }
            match e.op.op.as_ref() {
                Some(cairn_proto::pb::journal_op::Op::FileUpsert(u)) if u.path == path => {
                    Some(u.manifest_hash.clone())
                }
                _ => None,
            }
        });
        let (Some(base_manifest), Some(theirs_manifest)) = (base_manifest, theirs_manifest) else {
            return Ok(false); // no provable ancestor or winner: no offer
        };

        // 3. Bytes: base + theirs through the hydrate assembly (local CAS
        //    first, plane second, chunks hash-verified); ours is the conflict
        //    copy this device just renamed its local edit into.
        let mut manifest_cache: HashMap<String, cairn_core::manifest::Manifest> = HashMap::new();
        let base_bytes = crate::hydrate::hydrate_one(
            self.plane.as_ref(),
            None,
            &self.cas,
            &self.tenant_id,
            &base_manifest,
            path,
            &mut manifest_cache,
        )
        .await?;
        let theirs_bytes = crate::hydrate::hydrate_one(
            self.plane.as_ref(),
            None,
            &self.cas,
            &self.tenant_id,
            &theirs_manifest,
            path,
            &mut manifest_cache,
        )
        .await?;
        let ours_bytes = tokio::fs::read(self.rooted(copy_path))
            .await
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("read conflict copy: {e}")))?;

        // 4-6. Parse, merge, serialize. Parse errors and refusals are honest
        //     "no offer" (Ok(false)) — the copy still preserves the edit.
        self.compute_and_store_offer(
            path,
            copy_path,
            &base_manifest,
            &theirs_manifest,
            &base_bytes,
            &theirs_bytes,
            &ours_bytes,
        )
    }

    /// The shared merge+store core for create and accept (the SAME inputs
    /// must produce the SAME merged bytes both times — that is the whole
    /// reason the offer stores manifests instead of merged content).
    fn compute_and_store_offer(
        &self,
        path: &str,
        copy_path: &str,
        base_manifest: &str,
        theirs_manifest: &str,
        base_bytes: &[u8],
        theirs_bytes: &[u8],
        ours_bytes: &[u8],
    ) -> Result<bool, CairnError> {
        let (Some(base), Some(ours), Some(theirs)) = (
            parse_timeline(path, base_bytes),
            parse_timeline(path, ours_bytes),
            parse_timeline(path, theirs_bytes),
        ) else {
            tracing::debug!(path = %path, "merge offer: source not parseable as a timeline");
            return Ok(false);
        };
        let (merged, report) = match merge_with(
            &base,
            &ours,
            &theirs,
            &MergeOptions {
                semantic: true, // ADR-0023: the opt-in policy this flag IS
            },
        ) {
            Ok(r) => r,
            Err(refusal) => {
                tracing::debug!(path = %path, "merge offer: refused ({})", refusal.0);
                return Ok(false);
            }
        };
        if report.outcome == Outcome::Conflicts {
            // See the module doc: a merge that still needs a human is not an
            // offer. The conflict copy stays; the classic tl-merge UI handles it.
            return Ok(false);
        }
        // Serialize NOW to fail the offer early if canon refuses (accept would
        // hit the identical wall); the merged bytes themselves are recomputed
        // at accept — determinism is the whole point of storing manifests only.
        let merged_otio = cairn_tl::canon::serialize(&merged)
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("canon serialize: {e}")))?;
        debug_assert!(!merged_otio.is_empty());
        self.store.upsert_merge_offer(&cairn_store::MergeOfferRow {
            project_id: self.local_ns.clone(),
            path: path.to_string(),
            copy_path: copy_path.to_string(),
            base_manifest: base_manifest.to_string(),
            theirs_manifest: theirs_manifest.to_string(),
            report_json: report.to_json().to_string(),
            created_ms: self.store.clock().now_millis(),
        })?;
        tracing::info!(
            path = %path,
            outcome = ?report.outcome,
            "semantic merge offered for a conflicted timeline"
        );
        Ok(true)
    }

    /// Accept the pending offer for `path`: recompute the merge from the
    /// stored manifests, write the merged timeline to the ORIGINAL path,
    /// remove the conflict copy (row + file), delete the offer, and push the
    /// result through the normal pipeline — exactly ONE new journal entry for
    /// the original path (content-derived request id; server dedup on top).
    ///
    /// A missing offer or a missing/unparseable source is an `Err` that KEEPS
    /// the offer (honest failure: nothing is half-applied before the row is
    /// deleted — the disk write is the first destructive step).
    pub async fn accept_offer(&self, path: &str) -> Result<AcceptOutcome, CairnError> {
        let offer = self
            .store
            .get_merge_offer(&self.local_ns, path)
            .ok_or_else(|| {
                CairnError::new(ErrorKind::NotFound, format!("no merge offer for {path}"))
            })?;

        // Advance the cursor past the winner BEFORE re-pushing: the merged
        // append must claim a base the server's seq>base rule accepts, or it
        // would just conflict again (conflict-copy of the merge). In the
        // steady daemon the 1s sync loop has already pulled everything, so
        // this is usually a cheap no-op fetch; it exists for the
        // accept-immediately-after-conflict race.
        self.pull_now().await?;

        // Recompute (never trust stored bytes — there are none). A source
        // that vanished (GC, manual deletion) errors out with the offer kept.
        let mut manifest_cache: HashMap<String, cairn_core::manifest::Manifest> = HashMap::new();
        let base_bytes = crate::hydrate::hydrate_one(
            self.plane.as_ref(),
            None,
            &self.cas,
            &self.tenant_id,
            &offer.base_manifest,
            path,
            &mut manifest_cache,
        )
        .await?;
        let theirs_bytes = crate::hydrate::hydrate_one(
            self.plane.as_ref(),
            None,
            &self.cas,
            &self.tenant_id,
            &offer.theirs_manifest,
            path,
            &mut manifest_cache,
        )
        .await?;
        let ours_bytes = tokio::fs::read(self.rooted(&offer.copy_path))
            .await
            .map_err(|e| {
                CairnError::new(
                    ErrorKind::Io,
                    format!("conflict copy {} unreadable: {e}", offer.copy_path),
                )
            })?;

        let (Some(base), Some(ours), Some(theirs)) = (
            parse_timeline(path, &base_bytes),
            parse_timeline(path, &ours_bytes),
            parse_timeline(path, &theirs_bytes),
        ) else {
            return Err(CairnError::new(
                ErrorKind::ManifestFormat,
                "merge offer sources no longer parse as timelines",
            ));
        };
        let (merged, report) = merge_with(&base, &ours, &theirs, &MergeOptions { semantic: true })
            .map_err(|refusal| {
                CairnError::new(
                    ErrorKind::Conflict,
                    format!("stored offer no longer merges: {}", refusal.0),
                )
            })?;
        if report.outcome == Outcome::Conflicts {
            return Err(CairnError::new(
                ErrorKind::Conflict,
                "stored offer re-classifies as conflicted — keeping the offer and the copy",
            ));
        }
        let merged_otio = cairn_tl::canon::serialize(&merged)
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("canon serialize: {e}")))?;

        // Atomic write to the ORIGINAL path (tmp + rename, the conflict_copy
        // discipline): the first destructive step, so everything above can
        // fail without losing anything.
        let full = self.rooted(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("mkdir: {e}")))?;
        }
        let tmp = full.with_extension(format!(
            "cairn-merge-tmp-{}",
            self.store.clock().now_millis()
        ));
        std::fs::write(&tmp, merged_otio.as_bytes())
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("write merged: {e}")))?;
        std::fs::rename(&tmp, &full)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("publish merged: {e}")))?;

        // The conflict copy's story is over: row gone (hard delete — NOT a
        // tombstone; the copy's file-delete must never propagate as a journal
        // op), file gone.
        self.store.delete_file(&self.local_ns, &offer.copy_path)?;
        match std::fs::remove_file(self.rooted(&offer.copy_path)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CairnError::new(
                    ErrorKind::Io,
                    format!("remove conflict copy: {e}"),
                ))
            }
        }

        // Mark the original as a local edit so process_file re-ingests it:
        // fresh stat from the bytes we just wrote, no manifest yet (the push
        // records it), Dirty — the legal re-entry state for a push.
        let meta = std::fs::metadata(&full)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("stat merged: {e}")))?;
        self.store.put_file(&cairn_store::FileRow {
            path: path.to_string(),
            project_id: self.local_ns.clone(),
            manifest_hash: None,
            size: meta.len(),
            mode: "file".into(),
            mtime: crate::scan::mtime_millis(&meta),
            local_state: cairn_store::state::LocalState::Dirty.as_str().into(),
        })?;
        self.store.delete_merge_offer(&self.local_ns, path)?;

        // The journal write rides the normal pipeline (ONE FileUpsert,
        // content-derived request id). A failure here still leaves the row
        // Dirty — the next pass lands the same entry (idempotent).
        let mut stats = crate::engine::PassStats::default();
        self.process_file(path, &mut stats).await?;

        Ok(AcceptOutcome {
            report_json: report.to_json().to_string(),
        })
    }

    /// Decline the pending offer: only the affordance goes. The conflict copy
    /// STAYS on disk and in the table — that is the §7.1 contract (declining
    /// means "I'll resolve it myself", not "discard my edit"). Returns whether
    /// an offer was actually removed.
    pub fn decline_offer(&self, path: &str) -> Result<bool, CairnError> {
        let Some(_offer) = self.store.get_merge_offer(&self.local_ns, path) else {
            return Ok(false);
        };
        self.store.delete_merge_offer(&self.local_ns, path)?;
        Ok(true)
    }
}

/// What accepting produced — the report JSON feeds the dashboard toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptOutcome {
    pub report_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_predicate_matches_search_ext_rule() {
        assert!(is_timeline_path("session.otio"));
        assert!(is_timeline_path("SESSION.OTIO"));
        assert!(is_timeline_path("seq.FcpXML"));
        assert!(is_timeline_path("dir/sub/seq.fcpxml"));
        assert!(!is_timeline_path("notes.txt"));
        assert!(!is_timeline_path("hero.prproj"));
        assert!(!is_timeline_path("noext"));
        // a bare ".otio" still has an extension (empty stem)
        assert!(is_timeline_path(".otio"));
    }
}
