//! ADR-0031 Phase 3 — the daemon-side state materializer: `.cairn` becomes a
//! VIEW of the synced record set, not a source of truth.
//!
//! The directory STAYS (NLE tools, the shell extension and the CLI read
//! review.json / members.json locally; offline machines keep working), but
//! after every sync pass that APPLIES state records, this module rewrites the
//! machine-local files so they follow the records — the same relationship the
//! overlay state file has to store metadata. Machine caches (`proxy-cache`)
//! stay outside the record set permanently.
//!
//! # Enforcement note (honest boundary, MUST 3)
//!
//! `rbac_guard` (cairn-cli/src/daemon.rs) reads members.json through
//! `crate::members::load_classified` — it was NOT touched for Phase 3. With
//! materialization it now SEES synced roster changes right after apply,
//! because this module wrote them into the file the guard reads: that is
//! ADR-0031's "enforcement follows the records" outcome. The honest boundary:
//! a machine that is OFFLINE keeps its local authority until the records
//! actually arrive — records can only move enforcement, never teleport it.
//!
//! # Per-family semantics (the union view)
//!
//! * `member` → members.json: upsert per device_id (the record payload IS the
//!   Member JSON), tombstone → remove. Records are truth for the keys they
//!   carry; entries the local machine owns that the record set does not
//!   mention STAY (offline local writes remain authoritative for themselves
//!   until they publish — the materializer unions, it never rebuilds from
//!   records alone).
//! * `audit` → audit.json (`crate::audit`): recorded by the entry's content
//!   id (the record key IS `AuditFile::id_for`) — the same decision arriving
//!   twice collapses to one entry, idempotent under replay.
//! * `review_version` → review.json: versions MISSING from the local stack
//!   are inserted (matched by the stack number the record key carries);
//!   existing numbers are never rewritten (the local stack stays append-only).
//! * `review_link` → review.json links: upsert by token (role/note/
//!   expires_at/latest_only from the payload). A TOMBSTONE removes the link —
//!   the Phase-2+3 payoff: the local FILE now reflects the cross-machine
//!   revoke for every local reader, beside the Phase-2 portal consult.
//! * `review_comment` → the version's note file
//!   (`cairn_review::store::Store::save_comments`, `.cairn/review-notes/
//!   v{N}.json`): upsert the note by id. Payload shape (defined HERE because
//!   no in-repo publisher exists yet — when one lands it must publish exactly
//!   this): `{"version": <u32>, "note": { …cairn_tl Note JSON… }}`, keyed by
//!   the note's content id. A tombstone removes the note.
//!
//! # The LWW echo guard
//!
//! The engine only delivers records that won the TABLE merge, so a stale
//! record normally never reaches this module. But the FILE can be fresher
//! than the table: an offline local edit (made directly to the file, its
//! record publish still pending in the outbox) has no table row yet. A
//! winning remote record could still be OLDER than that local edit — applying
//! it would regress the file. Guard: a LIVE record whose `ts_ms` is older
//! than the file entry's own timestamp for that key (member `added_at_ms`,
//! link `created_at`, note `created_ms`) is SKIPPED; TOMBSTONES always apply
//! (fail-closed — a revoke must never lose to a timestamp race, matching the
//! table's own tie rule).
//!
//! # Concurrency
//!
//! review.json's one-writer shape is `cairn_review::store::Store` load →
//! mutate → save; the daemon's service actions publish through the same API.
//! VERIFIED (cairn-review/src/store.rs `atomic_write`): save writes
//! `<path>.json.tmp`, fsyncs, then renames over the target — a reader never
//! sees a half-written file. Two writers on one daemon can still interleave
//! load→save windows (last save wins, each based on its own load); the
//! materializer accepts that small race window: both writers carry
//! idempotent content, and the next applied record re-asserts the record
//! view. members.json / audit.json write through the same atomic temp+rename
//! helpers their primary writers use.
//!
//! Failures are per-record, logged, never fatal: the cache is best-effort;
//! the records remain the truth.

use std::path::{Path, PathBuf};

use cairn_core::rbac::MemberFile;
use cairn_sync::state_records::AppliedRecord;

use crate::audit::{AuditEntry, AuditFile};
use crate::members;

/// Materializes applied records into ONE root's `.cairn` directory. The
/// daemon builds one per engine in the project run loop (`projects.rs`), so
/// the project-id → root mapping is resolved at construction time — the
/// engine hands us the local row scope (`project`, which names the store
/// rows) and we already know the root whose files those rows describe.
pub struct RootMaterializer {
    root: PathBuf,
}

impl RootMaterializer {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        RootMaterializer { root }
    }
}

impl cairn_sync::state_records::StateMaterializer for RootMaterializer {
    fn materialize(&self, project: &str, applied: &[AppliedRecord]) {
        for rec in applied {
            let res = match rec.family.as_str() {
                "member" => self.apply_member(rec),
                "audit" => self.apply_audit(rec),
                "review_version" => self.apply_review_version(rec),
                "review_link" => self.apply_review_link(rec),
                "review_comment" => self.apply_review_comment(rec),
                // unknown family: forward-compatible no-op (the engine's apply
                // path already rejected it — this is belt and braces)
                _ => Ok(()),
            };
            if let Err(e) = res {
                tracing::warn!(
                    project = %project,
                    family = %rec.family,
                    key = %rec.key,
                    error = %e,
                    "state record materialization failed — cache refresh skipped for this record (records remain the truth)"
                );
            }
        }
    }
}

/// A corrupt/unreadable cache file is NEVER clobbered by the materializer:
/// the local file may hold fresher local state than we can prove, so the
/// refresh for that family is skipped (logged by the caller) and the record
/// table stays the truth. `Err` text is the log line.
fn read_members(root: &Path) -> Result<MemberFile, String> {
    match std::fs::read(members::members_path(root)) {
        Ok(b) => MemberFile::from_json(&b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemberFile::default()),
        Err(e) => Err(format!("read members.json: {e}")),
    }
}

fn save_members(root: &Path, f: &MemberFile) -> Result<(), String> {
    let json = f.to_json()?;
    cairn_proxy::pipeline::atomic_write(&members::members_path(root), json.as_slice())
        .map_err(|e| format!("write members.json: {e}"))
}

impl RootMaterializer {
    fn apply_member(&self, rec: &AppliedRecord) -> Result<(), String> {
        let mut file = read_members(&self.root)?;
        if rec.tombstone {
            file.remove(&rec.key);
        } else {
            let m: cairn_core::rbac::Member =
                serde_json::from_slice(&rec.payload).map_err(|e| format!("member payload: {e}"))?;
            // LWW echo guard: the file's entry is fresher than this record
            // (an offline local edit whose publish is still pending) — skip.
            if let Some(existing) = file.members.get(&rec.key) {
                if rec.ts_ms < existing.added_at_ms {
                    tracing::debug!(
                        device = %rec.key,
                        "member record older than the local file entry — echo skipped"
                    );
                    return Ok(());
                }
            }
            file.members.insert(rec.key.clone(), m);
        }
        save_members(&self.root, &file)
    }

    fn apply_audit(&self, rec: &AppliedRecord) -> Result<(), String> {
        let entry: AuditEntry =
            serde_json::from_slice(&rec.payload).map_err(|e| format!("audit payload: {e}"))?;
        // the record key IS the content id (crate::state_records::publish_audit
        // keys the family by AuditFile::id_for) — same decision twice = one
        // entry, so the dedupe is the file's own id discipline.
        AuditFile::record_with_id(&self.root, entry, &rec.key)
    }

    fn apply_review_version(&self, rec: &AppliedRecord) -> Result<(), String> {
        let mut file = match cairn_review::store::Store::load(&self.root) {
            Ok(Some(f)) => f,
            Ok(None) => cairn_review::model::ReviewFile::default(),
            Err(e) => return Err(format!("read review.json: {e}")),
        };
        let number: u32 = rec
            .key
            .parse()
            .map_err(|_| format!("review_version key '{}' is not a stack number", rec.key))?;
        if file.version(number).is_some() {
            // the local stack already carries this position — append-only
            // means never rewriting an existing number (the merged read
            // surfaces in the dashboard read the record table anyway)
            return Ok(());
        }
        let v: serde_json::Value = serde_json::from_slice(&rec.payload)
            .map_err(|e| format!("review_version payload: {e}"))?;
        let version = cairn_review::model::ReviewVersion {
            number,
            label: v
                .get("label")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .into(),
            media_rel: v
                .get("media_rel")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .into(),
            proxy_rel: None,
            fps_num: v.get("fps_num").and_then(|x| x.as_u64()).unwrap_or(24) as u32,
            fps_den: v.get("fps_den").and_then(|x| x.as_u64()).unwrap_or(1) as u32,
            frames: v.get("frames").and_then(|x| x.as_u64()).unwrap_or(0),
            timeline_fingerprint: None,
            snapshot: None,
            published_by: v
                .get("published_by")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .into(),
            published_at: v
                .get("published_at")
                .and_then(|x| x.as_i64())
                .unwrap_or_default(),
        };
        // keep the stack ascending by number (insert, not publish() — publish
        // would reassign positions and clobber local history)
        let pos = file
            .versions
            .iter()
            .position(|x| x.number > number)
            .unwrap_or(file.versions.len());
        file.versions.insert(pos, version);
        cairn_review::store::Store::save(&self.root, &file)
    }

    fn apply_review_link(&self, rec: &AppliedRecord) -> Result<(), String> {
        let mut file = match cairn_review::store::Store::load(&self.root) {
            Ok(Some(f)) => f,
            Ok(None) => cairn_review::model::ReviewFile::default(),
            Err(e) => return Err(format!("read review.json: {e}")),
        };
        if rec.tombstone {
            // ALWAYS applies (fail-closed, the Phase-2+3 payoff): a cross-
            // machine revoke removes the link from the file every local
            // reader sees, regardless of any timestamp race.
            file.revoke_link(&rec.key);
        } else {
            let v: serde_json::Value = serde_json::from_slice(&rec.payload)
                .map_err(|e| format!("review_link payload: {e}"))?;
            let role = v
                .get("role")
                .and_then(|x| x.as_str())
                .and_then(cairn_review::model::GuestRole::parse)
                .unwrap_or(cairn_review::model::GuestRole::Commenter);
            let link = cairn_review::model::GuestLink {
                token: rec.key.clone(),
                role,
                note: v
                    .get("note")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .into(),
                expires_at: v.get("expires_at").and_then(|x| x.as_i64()).unwrap_or(0),
                latest_only: v
                    .get("latest_only")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                created_at: v
                    .get("created_at")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(rec.ts_ms),
            };
            // LWW echo guard: the file's link is fresher than this record —
            // skip (see the module doc). `created_at` is the link's mint ts.
            if let Some(existing) = file.links.iter().find(|l| l.token == rec.key) {
                if rec.ts_ms < existing.created_at {
                    tracing::debug!(
                        token = %rec.key,
                        "review_link record older than the local file entry — echo skipped"
                    );
                    return Ok(());
                }
            }
            match file.links.iter_mut().find(|l| l.token == rec.key) {
                Some(existing) => *existing = link,
                None => file.links.push(link),
            }
        }
        cairn_review::store::Store::save(&self.root, &file)
    }

    fn apply_review_comment(&self, rec: &AppliedRecord) -> Result<(), String> {
        if rec.tombstone {
            let note_id = &rec.key;
            let version = comment_version(&rec.payload)?;
            let mut set = cairn_review::store::Store::load_comments(&self.root, version)?;
            if set.notes.remove(note_id).is_none() {
                return Ok(());
            }
            return cairn_review::store::Store::save_comments(&self.root, version, &set);
        }
        let (version, note) = comment_note(&rec.payload)?;
        // LWW echo guard: the file's note is fresher than this record — skip.
        let mut set = cairn_review::store::Store::load_comments(&self.root, version)?;
        if let Some(existing) = set.notes.get(&note.id) {
            if rec.ts_ms < existing.created_ms {
                tracing::debug!(
                    note = %note.id,
                    "review_comment record older than the local note — echo skipped"
                );
                return Ok(());
            }
        }
        set.notes.insert(note.id.clone(), note);
        cairn_review::store::Store::save_comments(&self.root, version, &set)
    }
}

/// `review_comment` payload → (version, note). The shape is defined HERE (see
/// the module doc): `{"version": N, "note": {…Note JSON…}}`.
fn comment_note(payload: &[u8]) -> Result<(u32, cairn_tl::notes::Note), String> {
    let v: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| format!("review_comment payload: {e}"))?;
    let version = v
        .get("version")
        .and_then(|x| x.as_u64())
        .ok_or("review_comment payload missing version")? as u32;
    let note: cairn_tl::notes::Note = serde_json::from_value(
        v.get("note")
            .cloned()
            .ok_or("review_comment payload missing note")?,
    )
    .map_err(|e| format!("review_comment note: {e}"))?;
    Ok((version, note))
}

/// Tombstone payloads carry `{"version": N}` (the note id is the key).
fn comment_version(payload: &[u8]) -> Result<u32, String> {
    let v: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| format!("review_comment payload: {e}"))?;
    v.get("version")
        .and_then(|x| x.as_u64())
        .map(|n| n as u32)
        .ok_or_else(|| "review_comment tombstone payload missing version".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_review::model::GuestLink;
    use cairn_sync::state_records::{AppliedRecord, StateMaterializer};

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Brand Film");
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    fn rec(family: &str, key: &str, payload: &[u8], ts: i64, tombstone: bool) -> AppliedRecord {
        AppliedRecord {
            family: family.into(),
            key: key.into(),
            record_id: key.into(),
            ts_ms: ts,
            device_id: "dev-A".into(),
            payload: payload.to_vec(),
            tombstone,
        }
    }

    fn m(root: &Path) -> RootMaterializer {
        RootMaterializer::new(root.to_path_buf())
    }

    fn members_of(root: &Path) -> MemberFile {
        MemberFile::from_json(&std::fs::read(members::members_path(root)).unwrap()).unwrap()
    }

    fn review_of(root: &Path) -> cairn_review::model::ReviewFile {
        cairn_review::store::Store::load(root).unwrap().unwrap()
    }

    // ---------- member ----------

    #[test]
    fn member_record_upserts_and_tombstone_removes_in_members_json() {
        let (_d, root) = tmp();
        m(&root).materialize(
            "p1",
            &[
                rec(
                    "member",
                    "dev-9",
                    br#"{"device_id":"dev-9","name":"Rook","role":"colorist","added_at_ms":100,"added_by":"dev-A"}"#,
                    100,
                    false,
                ),
                rec(
                    "member",
                    "dev-2",
                    br#"{"device_id":"dev-2","name":"Ada","role":"owner","added_at_ms":90,"added_by":"bootstrap"}"#,
                    90,
                    false,
                ),
            ],
        );
        let f = members_of(&root);
        assert_eq!(f.members.len(), 2);
        assert_eq!(f.members["dev-9"].role, cairn_core::rbac::Role::Colorist);
        assert_eq!(
            f.members["dev-9"].added_at_ms, 100,
            "record ts carried, not now()"
        );

        // role update on the same key (LWW register) lands in the file
        m(&root).materialize(
            "p1",
            &[rec(
                "member",
                "dev-9",
                br#"{"device_id":"dev-9","name":"Rook","role":"reviewer","added_at_ms":100,"added_by":"dev-A"}"#,
                300,
                false,
            )],
        );
        assert_eq!(
            members_of(&root).members["dev-9"].role,
            cairn_core::rbac::Role::Reviewer
        );

        // tombstone removes — and the OTHER member stays (union view)
        m(&root).materialize(
            "p1",
            &[rec("member", "dev-9", br#"{"removed":true}"#, 400, true)],
        );
        let f = members_of(&root);
        assert!(
            !f.members.contains_key("dev-9"),
            "tombstone removed the member"
        );
        assert!(
            f.members.contains_key("dev-2"),
            "unrelated local entries stay"
        );
    }

    #[test]
    fn member_echo_older_than_the_file_entry_is_skipped() {
        let (_d, root) = tmp();
        // local file holds an offline edit made at ts 500
        let mut f = MemberFile::default();
        f.upsert(
            "dev-9",
            "Rook Local",
            cairn_core::rbac::Role::Editor,
            "local",
            500,
        );
        save_members(&root, &f).unwrap();

        // an older remote record (ts 100) must NOT regress the file
        m(&root).materialize(
            "p1",
            &[rec(
                "member",
                "dev-9",
                br#"{"device_id":"dev-9","name":"Rook Remote","role":"viewer","added_at_ms":100,"added_by":"dev-A"}"#,
                100,
                false,
            )],
        );
        let f = members_of(&root);
        assert_eq!(f.members["dev-9"].name, "Rook Local", "older echo skipped");
        assert_eq!(f.members["dev-9"].role, cairn_core::rbac::Role::Editor);
    }

    // ---------- audit ----------

    #[test]
    fn audit_record_dedupes_by_content_id() {
        let (_d, root) = tmp();
        let entry = br#"{"ts_ms":42,"device":"dev-B","role":"editor","action":"ctl/detach-root","project":"p1","allowed":false}"#;
        // the SAME content id twice (a replay, or both machines ledgering the
        // same decision) → one line in the ledger
        m(&root).materialize(
            "p1",
            &[
                rec("audit", "cid-1", entry, 42, false),
                rec("audit", "cid-1", entry, 42, false),
            ],
        );
        let rows = AuditFile::load(&root).unwrap();
        assert_eq!(rows.len(), 1, "content id dedupe holds in the file");
        assert_eq!(rows[0].1.action, "ctl/detach-root");
        // a distinct decision adds a distinct entry
        m(&root).materialize(
            "p1",
            &[rec("audit", "cid-2", br#"{"ts_ms":43,"device":"dev-B","role":"editor","action":"dash/attach","project":"p1","allowed":true}"#, 43, false)],
        );
        assert_eq!(AuditFile::load(&root).unwrap().len(), 2);
    }

    // ---------- review versions ----------

    #[test]
    fn review_version_record_inserts_missing_stack_positions_only() {
        let (_d, root) = tmp();
        // local file already has v1 (its own publish)
        let mut f = cairn_review::model::ReviewFile {
            title: "Brand Film".into(),
            ..Default::default()
        };
        f.publish(cairn_review::model::ReviewVersion {
            number: 0,
            label: "v1".into(),
            media_rel: "cuts/v1.mp4".into(),
            proxy_rel: None,
            fps_num: 24,
            fps_den: 1,
            frames: 100,
            timeline_fingerprint: None,
            snapshot: None,
            published_by: "me".into(),
            published_at: 50,
        });
        cairn_review::store::Store::save(&root, &f).unwrap();

        // the record set carries v2 (from the other machine)
        m(&root).materialize(
            "p1",
            &[rec(
                "review_version",
                "2",
                br#"{"number":2,"label":"client cut","media_rel":"cuts/v2.mp4","fps_num":24,"fps_den":1,"frames":120,"published_by":"editor-a","published_at":200}"#,
                200,
                false,
            )],
        );
        let f = review_of(&root);
        assert_eq!(f.versions.len(), 2);
        assert_eq!(f.versions[0].number, 1, "local v1 untouched");
        assert_eq!(f.versions[0].label, "v1");
        assert_eq!(f.versions[1].number, 2, "missing version inserted");
        assert_eq!(f.versions[1].media_rel, "cuts/v2.mp4");
        assert_eq!(f.latest().unwrap().frames, 120);

        // a re-delivery of v2 (or another record claiming v2) rewrites nothing
        m(&root).materialize(
            "p1",
            &[rec(
                "review_version",
                "2",
                br#"{"number":2,"label":"IMPOSTOR","media_rel":"x.mp4","fps_num":1,"fps_den":1,"frames":1,"published_by":"x","published_at":1}"#,
                999,
                false,
            )],
        );
        assert_eq!(review_of(&root).versions[1].label, "client cut");
    }

    // ---------- review links ----------

    #[test]
    fn review_link_upsert_and_tombstone_removal_in_review_json() {
        let (_d, root) = tmp();
        let mut f = cairn_review::model::ReviewFile {
            title: "Brand Film".into(),
            ..Default::default()
        };
        f.publish(cairn_review::model::ReviewVersion {
            number: 0,
            label: "v1".into(),
            media_rel: "cuts/v1.mp4".into(),
            proxy_rel: None,
            fps_num: 24,
            fps_den: 1,
            frames: 100,
            timeline_fingerprint: None,
            snapshot: None,
            published_by: "me".into(),
            published_at: 50,
        });
        cairn_review::store::Store::save(&root, &f).unwrap();

        // a link minted on the OTHER machine lands in this file
        m(&root).materialize(
            "p1",
            &[rec(
                "review_link",
                "tok-1",
                br#"{"token":"tok-1","role":"viewer","note":"acme","expires_at":888,"latest_only":true,"created_at":100}"#,
                100,
                false,
            )],
        );
        let f = review_of(&root);
        assert_eq!(f.links.len(), 1);
        let link = &f.links[0];
        assert_eq!(link.token, "tok-1");
        assert_eq!(link.role, cairn_review::model::GuestRole::Viewer);
        assert_eq!(link.expires_at, 888);
        assert!(link.latest_only);

        // a role change on the same token upserts in place
        m(&root).materialize(
            "p1",
            &[rec(
                "review_link",
                "tok-1",
                br#"{"token":"tok-1","role":"commenter","note":"acme","expires_at":888,"latest_only":false,"created_at":100}"#,
                300,
                false,
            )],
        );
        let f = review_of(&root);
        assert_eq!(f.links.len(), 1, "upsert by token, no duplicate");
        assert_eq!(f.links[0].role, cairn_review::model::GuestRole::Commenter);

        // THE PHASE-3 PAYOFF: a revoke tombstone from the other machine
        // removes the link from the local FILE itself
        m(&root).materialize(
            "p1",
            &[rec(
                "review_link",
                "tok-1",
                br#"{"token":"tok-1","revoked":true}"#,
                400,
                true,
            )],
        );
        let f = review_of(&root);
        assert!(
            GuestLink::resolve(&f.links, "tok-1", 1_000).is_none(),
            "the file itself no longer lists the revoked link"
        );
        assert_eq!(
            f.versions.len(),
            1,
            "versions untouched by the link tombstone"
        );
    }

    #[test]
    fn review_link_tombstone_applies_even_over_a_fresher_local_link() {
        let (_d, root) = tmp();
        // local file has the link minted at 500 (an offline local edit)
        let mut f = cairn_review::model::ReviewFile::default();
        f.links.push(cairn_review::model::GuestLink {
            token: "tok-2".into(),
            role: cairn_review::model::GuestRole::Commenter,
            note: "local".into(),
            expires_at: 0,
            latest_only: false,
            created_at: 500,
        });
        cairn_review::store::Store::save(&root, &f).unwrap();

        // an older LIVE echo (ts 100 < 500) must not regress the file...
        m(&root).materialize(
            "p1",
            &[rec(
                "review_link",
                "tok-2",
                br#"{"token":"tok-2","role":"viewer","note":"remote-old","expires_at":1,"latest_only":false,"created_at":100}"#,
                100,
                false,
            )],
        );
        let f = review_of(&root);
        assert_eq!(f.links[0].note, "local", "older echo skipped");
        assert_eq!(f.links[0].role, cairn_review::model::GuestRole::Commenter);

        // ...but a TOMBSTONE always applies: revocation is fail-closed
        m(&root).materialize(
            "p1",
            &[rec(
                "review_link",
                "tok-2",
                br#"{"token":"tok-2","revoked":true}"#,
                50,
                true,
            )],
        );
        assert!(
            review_of(&root).links.is_empty(),
            "the revoke removes the link even over a fresher local entry"
        );
    }

    // ---------- review comments ----------

    #[test]
    fn review_comment_record_upserts_into_the_version_note_file() {
        let (_d, root) = tmp();
        let payload = format!(
            r#"{{"version":1,"note":{}}}"#,
            serde_json::to_string(&cairn_tl::notes::Note::new(
                "client-jane",
                "tighten the cut here",
                cairn_tl::notes::NoteAnchor {
                    clip: None,
                    frame: 42,
                    rate: 24,
                    range: None,
                },
                cairn_tl::notes::NoteStatus::Open,
                100,
            ))
            .unwrap()
        );
        m(&root).materialize(
            "p1",
            &[rec(
                "review_comment",
                "note-id-1",
                payload.as_bytes(),
                100,
                false,
            )],
        );
        let set = cairn_review::store::Store::load_comments(&root, 1).unwrap();
        assert_eq!(set.len(), 1);
        let note = set.notes.values().next().unwrap();
        assert_eq!(note.body, "tighten the cut here");
        assert_eq!(note.author, "client-jane");

        // B writes the comment OFFLINE; A's materializer lands it in v1 —
        // the same content id upserts in place (LWW register per note id)
        m(&root).materialize(
            "p1",
            &[rec(
                "review_comment",
                "note-id-1",
                payload.as_bytes(),
                100,
                false,
            )],
        );
        assert_eq!(
            cairn_review::store::Store::load_comments(&root, 1)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn review_comment_tombstone_removes_the_note() {
        let (_d, root) = tmp();
        let note = cairn_tl::notes::Note::new(
            "client-jane",
            "too loud",
            cairn_tl::notes::NoteAnchor {
                clip: None,
                frame: 7,
                rate: 24,
                range: None,
            },
            cairn_tl::notes::NoteStatus::Open,
            100,
        );
        let payload = format!(
            r#"{{"version":2,"note":{}}}"#,
            serde_json::to_string(&note).unwrap()
        );
        m(&root).materialize(
            "p1",
            &[rec(
                "review_comment",
                &note.id,
                payload.as_bytes(),
                100,
                false,
            )],
        );
        assert_eq!(
            cairn_review::store::Store::load_comments(&root, 2)
                .unwrap()
                .len(),
            1
        );

        let tomb = r#"{"version":2}"#.to_string();
        m(&root).materialize(
            "p1",
            &[rec("review_comment", &note.id, tomb.as_bytes(), 200, true)],
        );
        assert_eq!(
            cairn_review::store::Store::load_comments(&root, 2)
                .unwrap()
                .len(),
            0,
            "tombstone removes the note from the version file"
        );
    }

    // ---------- robustness ----------

    #[test]
    fn corrupt_cache_files_are_never_clobbered() {
        let (_d, root) = tmp();
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        std::fs::write(members::members_path(&root), b"{not json").unwrap();
        std::fs::write(root.join(".cairn").join("review.json"), b"{not json").unwrap();
        // every family errors per-record (logged, swallowed) — and the corrupt
        // bytes are still there: the materializer never destroys what it
        // cannot read
        m(&root).materialize(
            "p1",
            &[
                rec("member", "dev-9", br#"{"device_id":"dev-9","name":"R","role":"editor","added_at_ms":1,"added_by":"a"}"#, 1, false),
                rec("review_link", "tok-1", br#"{"token":"tok-1","role":"viewer","created_at":1}"#, 1, false),
                rec("review_version", "1", br#"{"number":1}"#, 1, false),
            ],
        );
        assert!(
            String::from_utf8_lossy(&std::fs::read(members::members_path(&root)).unwrap())
                .contains("not json")
        );
        assert!(String::from_utf8_lossy(
            &std::fs::read(root.join(".cairn").join("review.json")).unwrap()
        )
        .contains("not json"));
    }
}
