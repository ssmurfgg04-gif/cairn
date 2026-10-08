//! End-to-end scenarios over the REAL server (Task 6): `World` boots a real
//! `cairn_server::ServerState` with the real journal, §7.1 conflict rule, and
//! object store, and each device runs the real engine behind an InProcPlane.
//!
//! * ADR-0031 Phase 1 state records: roster LWW (add + tombstone removal),
//!   audit union, review-link revoke — device to device through the actual
//!   append/pull/replay pipeline, including the I2 property (a record is
//!   durable in the outbox BEFORE its first send, so a kill -9 between
//!   enqueue and append loses nothing).
//! * CONTRACT-DEBT #1 merge offer: two devices conflict on a real .otio
//!   through the real server's seq>base rule → the CONFLICT arm offers a
//!   semantic merge → acceptance converges BOTH devices on the merged head.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cairn_sim::world::World;
use cairn_store::state::LocalState;
use cairn_store::{FileRow, Outbox};
use cairn_sync::state_records::{enqueue_state_record, PublishParams};
use cairn_tl::model::*;
use cairn_tl::rational::Rational;

// ---------------------------------------------------------------------------
// World helpers (the daemon shape: sync pass, then materialize)
// ---------------------------------------------------------------------------

/// Daemon-like pass: sync (push dirty, pull remote) THEN materialize —
/// the exact run_loop order in cairn-cli/src/projects.rs.
async fn pass(world: &mut World, i: usize) -> cairn_sync::PassStats {
    let dev = &mut world.devices[i];
    let engine = dev.engine.as_mut().expect("device live");
    let stats = engine.sync_pass().await.expect("sync pass");
    cairn_sync::hydrate::materialize_missing(
        engine.plane.as_ref(),
        None, // sim: plane-only hydration (swarm transport is ADR-0017, sim-side TBD)
        &engine.store,
        &engine.cas,
        &engine.headers,
        "t1",
        "p1",
    )
    .await
    .expect("materialize");
    stats
}

/// A couple of alternating passes absorbs either-order first-round skew.
async fn converge(world: &mut World, rounds: usize) {
    for _ in 0..rounds {
        pass(world, 0).await;
        pass(world, 1).await;
    }
}

fn ws(world: &World, i: usize) -> PathBuf {
    let p = world.devices[i].root.path().join("store").join("workspace");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn mark_dirty(world: &mut World, i: usize, path: &str, len: u64) {
    let meta = std::fs::metadata(
        world.devices[i]
            .root
            .path()
            .join("store/workspace")
            .join(path),
    )
    .unwrap();
    let dev = &mut world.devices[i];
    let engine = dev.engine.as_mut().expect("device live");
    engine
        .store
        .put_file(&FileRow {
            path: path.into(),
            project_id: "p1".into(),
            manifest_hash: None,
            size: len,
            mode: "file".into(),
            mtime: cairn_sync::scan::mtime_millis(&meta),
            local_state: LocalState::Dirty.as_str().into(),
        })
        .unwrap();
}

fn engine_store(world: &World, i: usize) -> &cairn_store::Store {
    &world.devices[i].engine.as_ref().expect("device live").store
}

// ---------------------------------------------------------------------------
// 1. Roster records: add propagates; a later tombstone removes on both
// ---------------------------------------------------------------------------

/// device0 enrolls a member; device1 discovers it through the journal. Then
/// device1 removes the member with a tombstone at a LATER ts — the LWW
/// register must converge to the tombstone on BOTH devices (the removal wins
/// everywhere; the stale enrollment never resurrects).
#[tokio::test]
async fn roster_record_reaches_second_device_e2e() {
    let mut world = World::boot(11).await;

    // device0 enrolls dev-9 through the durable enqueue path
    let payload: &[u8] =
        br#"{"device_id":"dev-9","name":"Rook","role":"colorist","added_at_ms":100,"added_by":"dev-0"}"#;
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "member",
            key: "dev-9",
            payload,
            ts_ms: 100,
            tombstone: false,
        },
    )
    .unwrap();
    // own-op suppression: the publisher holds the record locally already
    assert_eq!(
        engine_store(&world, 0)
            .list_state_records("p1", "member")
            .len(),
        1
    );
    assert_eq!(
        engine_store(&world, 1)
            .list_state_records("p1", "member")
            .len(),
        0
    );

    pass(&mut world, 0).await;
    pass(&mut world, 1).await;

    let got = engine_store(&world, 1)
        .get_state_record("p1", "member", "dev-9")
        .expect("member record reached device1");
    assert_eq!(
        got.device_id, "dev-0",
        "authorship = the journal entry's device"
    );
    assert_eq!(got.ts_ms, 100);
    assert!(!got.tombstone);
    assert_eq!(
        got.payload, payload,
        "identical payload bytes on the far side"
    );

    // device1 (offline edit rights) removes the member: tombstone, later ts
    enqueue_state_record(
        engine_store(&world, 1),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-1",
            family: "member",
            key: "dev-9",
            payload: br#"{"removed":true}"#,
            ts_ms: 200,
            tombstone: true,
        },
    )
    .unwrap();
    converge(&mut world, 2).await;

    for i in 0..2 {
        let row = engine_store(&world, i)
            .get_state_record("p1", "member", "dev-9")
            .unwrap_or_else(|| panic!("device{i}: member row missing"));
        assert!(row.tombstone, "device{i}: the removal tombstone wins (LWW)");
        assert_eq!(row.ts_ms, 200, "device{i}: the later write is the register");
        assert_eq!(row.device_id, "dev-1");
    }
}

// ---------------------------------------------------------------------------
// 2. Audit union + review-link revoke
// ---------------------------------------------------------------------------

/// Both devices ledger DISTINCT audit decisions offline — the append family
/// must UNION on both (2 records everywhere, content ids derived alike).
/// Then device0 mints a guest link, device1 revokes it with a later-ts
/// tombstone: both end on the tombstone (the Phase-2 revoke substrate).
#[tokio::test]
async fn audit_union_and_review_link_revoke_e2e() {
    let mut world = World::boot(23).await;

    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "audit",
            key: "dash-attach-1",
            payload: br#"{"action":"dash/attach","allowed":true}"#,
            ts_ms: 100,
            tombstone: false,
        },
    )
    .unwrap();
    enqueue_state_record(
        engine_store(&world, 1),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-1",
            family: "audit",
            key: "ctl-detach-1",
            payload: br#"{"action":"ctl/detach-root","allowed":false}"#,
            ts_ms: 101,
            tombstone: false,
        },
    )
    .unwrap();
    converge(&mut world, 2).await;

    for i in 0..2 {
        let rows = engine_store(&world, i).list_state_records("p1", "audit");
        assert_eq!(
            rows.len(),
            2,
            "device{i}: the audit union holds both decisions"
        );
        let joined = rows
            .iter()
            .map(|r| String::from_utf8_lossy(&r.payload).into_owned())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("dash/attach") && joined.contains("ctl/detach-root"));
    }

    // device0 mints a guest link; it rides to device1
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "review_link",
            key: "tok-e2e",
            payload: br#"{"token":"tok-e2e","role":"viewer","created_at":100}"#,
            ts_ms: 100,
            tombstone: false,
        },
    )
    .unwrap();
    converge(&mut world, 1).await;
    let link = engine_store(&world, 1)
        .get_state_record("p1", "review_link", "tok-e2e")
        .expect("link reached device1");
    assert!(!link.tombstone);

    // device1 revokes with a LATER tombstone — both devices converge on it
    enqueue_state_record(
        engine_store(&world, 1),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-1",
            family: "review_link",
            key: "tok-e2e",
            payload: br#"{"token":"tok-e2e","revoked":true}"#,
            ts_ms: 200,
            tombstone: true,
        },
    )
    .unwrap();
    converge(&mut world, 2).await;

    for i in 0..2 {
        let row = engine_store(&world, i)
            .get_state_record("p1", "review_link", "tok-e2e")
            .unwrap_or_else(|| panic!("device{i}: link row missing"));
        assert!(row.tombstone, "device{i}: revocation propagated");
        assert_eq!(row.ts_ms, 200, "device{i}: the revoke is the winning write");
    }
}

// ---------------------------------------------------------------------------
// 3. I2 for records: crash between enqueue and send loses nothing
// ---------------------------------------------------------------------------

/// device0 enqueues a record but does NOT sync. `crash_device` is the kill -9
/// analogue (drop engine handles, reopen from disk — no flush). The outbox
/// entry must survive reopen (durable-before-send) and the very next pass
/// delivers the record to device1.
#[tokio::test]
async fn crash_does_not_lose_a_pending_record() {
    let mut world = World::boot(31).await;

    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "member",
            key: "dev-7",
            payload: br#"{"device_id":"dev-7","name":"Wren","role":"editor"}"#,
            ts_ms: 100,
            tombstone: false,
        },
    )
    .unwrap();
    // durable in the outbox, NOT yet sent (no pass ran)
    assert_eq!(
        Outbox::new(engine_store(&world, 0).conn_handle())
            .pending("p1", 10)
            .len(),
        1,
        "the record op is durable before its first send (I2)"
    );

    // kill -9: drop + reopen, then the reopened device delivers on its next pass
    world.crash_device(0);
    assert_eq!(
        Outbox::new(engine_store(&world, 0).conn_handle())
            .pending("p1", 10)
            .len(),
        1,
        "WAL replay restores the pending record op after the crash"
    );
    pass(&mut world, 0).await;
    pass(&mut world, 1).await;

    let got = engine_store(&world, 1)
        .get_state_record("p1", "member", "dev-7")
        .expect("the crashed device's record still reached device1");
    assert_eq!(got.device_id, "dev-0");
    assert_eq!(got.ts_ms, 100);
    assert!(String::from_utf8_lossy(&got.payload).contains("Wren"));
}

// ---------------------------------------------------------------------------
// 4. The merge-offer flow through the REAL server journal
// ---------------------------------------------------------------------------

fn tv(v: i128, r: i128) -> TimeVal {
    TimeVal {
        value: Rational::new(v, 1).unwrap(),
        rate: Rational::new(r, 1).unwrap(),
    }
}

fn clip(name: &str, url: &str, start: i128, dur: i128) -> Element {
    let mut c = Element::leaf(Kind::Clip, name);
    c.media = Some(MediaRef::single(
        MediaKind::External,
        String::new(),
        Some(url.into()),
    ));
    c.source_range = Some(TimeRange {
        start: tv(start, 24),
        duration: tv(dur, 24),
    });
    c
}

fn doc(tracks: Vec<(&str, Vec<Element>)>) -> Timeline {
    let track_els: Vec<Element> = tracks
        .into_iter()
        .map(|(name, items)| Element::container(Kind::Track(TrackKind::Video), name, items))
        .collect();
    Timeline {
        name: "session".into(),
        global_start_time: None,
        metadata: JsonMap::new(),
        tracks: Element::container(Kind::Stack, "tracks", track_els),
        extra: JsonMap::new(),
    }
}

/// One hero clip, 96 frames, with FIXED element uuids (merge identity needs
/// stable ids; determinism needs no uuid crate here).
fn hero_base() -> Timeline {
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    base.tracks.stamp_uuid("u-stack");
    base.tracks.children[0].stamp_uuid("u-v1");
    base.tracks.children[0].children[0].stamp_uuid("u-hero");
    base
}

fn otio(t: &Timeline) -> Vec<u8> {
    cairn_tl::canon::serialize(t).unwrap().into_bytes()
}

/// The full CONTRACT-DEBT #1 flow over the real server: two devices edit a
/// real .otio from the same base, the REAL journal conflict rule refuses
/// device1's append (device0's winner has seq > device1's base_seq from a
/// different device), the CONFLICT arm offers the semantic merge, acceptance
/// lands ONE merged upsert and both devices converge on the merged head.
#[tokio::test]
async fn merge_offer_flow_through_real_server_e2e() {
    let mut world = World::boot(47).await;

    // device0 authors the base; both devices hold it
    let base = hero_base();
    let mut theirs = base.clone(); // device0: head re-cut (in-point 6 frames later)
    theirs.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(6, 24),
        duration: tv(90, 24),
    });
    let mut ours = base.clone(); // device1: tail re-cut (8 frames off the end)
    ours.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(0, 24),
        duration: tv(88, 24),
    });

    std::fs::write(ws(&world, 0).join("seq.otio"), otio(&base)).unwrap();
    mark_dirty(&mut world, 0, "seq.otio", otio(&base).len() as u64);
    pass(&mut world, 0).await;
    pass(&mut world, 1).await; // device1 discovers the base only (cursor = seq 1)

    // device0's winner lands while device1 is offline (journal seq 2)
    std::fs::write(ws(&world, 0).join("seq.otio"), otio(&theirs)).unwrap();
    mark_dirty(&mut world, 0, "seq.otio", otio(&theirs).len() as u64);
    pass(&mut world, 0).await;

    // device1 opts into the zero-touch policy, then syncs its tail re-cut:
    // the real server CONFLICTs the append -> conflict copy -> merge offer
    world.devices[1]
        .engine
        .as_mut()
        .expect("device live")
        .store
        .meta_set("flag:semantic_merge", "true")
        .unwrap();
    std::fs::write(ws(&world, 1).join("seq.otio"), otio(&ours)).unwrap();
    mark_dirty(&mut world, 1, "seq.otio", otio(&ours).len() as u64);
    let conflict_stats = pass(&mut world, 1).await;
    assert_eq!(
        conflict_stats.conflicts_resolved, 1,
        "the real journal rule refused the divergent append"
    );
    assert_eq!(
        conflict_stats.merge_offered, 1,
        "the conflict produced an offer"
    );

    let offer = engine_store(&world, 1)
        .get_merge_offer("p1", "seq.otio")
        .expect("offer row exists on the conflicted device");
    assert!(
        offer.report_json.contains(r#""outcome":"notes""#),
        "semantic head-vs-tail re-cuts merge to Notes: {}",
        offer.report_json
    );
    let copy_path = offer.copy_path.clone();

    // acceptance recomputes the merge and pushes it through the normal pipeline
    let outcome = world.devices[1]
        .engine
        .as_mut()
        .expect("device live")
        .accept_offer("seq.otio")
        .await
        .expect("accept recomputes the stored offer");
    assert!(outcome.report_json.contains(r#""outcome":"notes""#));
    assert!(engine_store(&world, 1)
        .get_merge_offer("p1", "seq.otio")
        .is_none());
    assert!(engine_store(&world, 1).get_file("p1", &copy_path).is_none());

    converge(&mut world, 2).await;

    // both devices converged on the merged head
    let merged_1 = engine_store(&world, 1)
        .get_file("p1", "seq.otio")
        .expect("device1 row")
        .manifest_hash
        .expect("merged manifest");
    let merged_0 = engine_store(&world, 0)
        .get_file("p1", "seq.otio")
        .expect("device0 row")
        .manifest_hash
        .expect("converged manifest");
    assert_eq!(merged_0, merged_1, "both devices hold the merged manifest");

    // the merged timeline carries BOTH re-cuts composed: in 6, 96 - 6 - 8 = 82
    let merged_bytes = std::fs::read(ws(&world, 1).join("seq.otio")).unwrap();
    let merged = cairn_tl::parse::parse_otio(std::str::from_utf8(&merged_bytes).unwrap()).unwrap();
    let hero = &merged.tracks.children[0].children[0];
    assert_eq!(hero.source_range.as_ref().unwrap().start.value.num, 6);
    assert_eq!(hero.source_range.as_ref().unwrap().duration.value.num, 82);

    // device0 even materializes the merged bytes (the sim's hydration pass)
    let bytes_0 = std::fs::read(ws(&world, 0).join("seq.otio")).unwrap();
    assert_eq!(bytes_0, merged_bytes, "byte-identical merged head on both");
}

// ---------------------------------------------------------------------------
// 5. ADR-0031 Phase 2: cross-machine revoke through the REAL portal gate
// ---------------------------------------------------------------------------

use cairn_review::http::{Portal, RootProvider};
use cairn_review::model::{GuestLink, GuestRole, ReviewFile, ReviewVersion};
use cairn_review::store as review_store;

/// The daemon-shaped RootProvider over ONE device (what `RuntimesProvider`
/// does in cairn-cli): roots from the machine-local review.json of the
/// attached workspace, revocations from the device's synced record table.
struct SimProvider {
    project: String,
    root: PathBuf,
    store: cairn_store::Store,
}

#[async_trait::async_trait]
impl RootProvider for SimProvider {
    async fn roots(&self) -> Vec<(String, PathBuf)> {
        vec![(self.project.clone(), self.root.clone())]
    }
    async fn link_revocations(&self, project_id: &str) -> Vec<String> {
        self.store
            .list_state_records(project_id, "review_link")
            .into_iter()
            .filter(|r| r.tombstone)
            .map(|r| r.key)
            .collect()
    }
}

fn portal_for(world: &World, i: usize) -> Portal {
    let dev = &world.devices[i];
    let store = cairn_store::Store::open(
        &dev.root.path().join("store"),
        std::sync::Arc::new(cairn_core::clock::WallClock),
    )
    .unwrap();
    Portal::new(std::sync::Arc::new(SimProvider {
        project: "p1".into(),
        root: ws(world, i),
        store,
    }))
}

/// Mint the same guest link on BOTH machines' review.json (the shared-link
/// shape: the token is a live link on A and B — e.g. re-minted from the
/// synced record, or a folder reachable from both), return the token.
fn seed_link(world: &World, i: usize, token: &str, created_at: i64) {
    let mut f = ReviewFile {
        title: "Brand Film".into(),
        ..Default::default()
    };
    f.publish(ReviewVersion {
        number: 0,
        label: "v1".into(),
        media_rel: "cuts/v1.mp4".into(),
        proxy_rel: None,
        fps_num: 24,
        fps_den: 1,
        frames: 100,
        timeline_fingerprint: None,
        snapshot: None,
        published_by: "editor-a".into(),
        published_at: created_at,
    });
    f.links.push(GuestLink {
        token: token.to_string(),
        role: GuestRole::Commenter,
        note: "acme client".into(),
        expires_at: 0, // never expires — only a revoke kills it
        latest_only: false,
        created_at,
    });
    review_store::Store::save(&ws(world, i), &f).unwrap();
}

/// THE Phase-2 security payoff, end to end over the real server journal:
/// device A mints a guest link and publishes its record; BOTH portals
/// (A's and B's) serve the token. A revokes; the tombstone rides ONE sync
/// pass; the portal on the OTHER machine then refuses the token — answered
/// exactly like an unknown one — even though B's review.json still lists
/// the link as live.
#[tokio::test]
async fn cross_machine_revoke_kills_the_link_on_the_other_portal_e2e() {
    let mut world = World::boot(59).await;
    let token = "tok-phase2-e2e-revoke0000000000000000"; // 32 chars, link-shaped

    // both machines hold the live link locally
    seed_link(&world, 0, token, 100);
    seed_link(&world, 1, token, 100);

    // device A publishes the mint record through the durable enqueue path
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "review_link",
            key: token,
            payload: br#"{"token":"tok-phase2-e2e-revoke0000000000000000","role":"commenter"}"#,
            ts_ms: 100,
            tombstone: false,
        },
    )
    .unwrap();
    pass(&mut world, 0).await;
    pass(&mut world, 1).await;

    // both portals serve the token (resolve == the gate the HTTP routes use)
    assert!(
        portal_for(&world, 0).resolve(token).await.is_some(),
        "portal A serves the minted link"
    );
    assert!(
        portal_for(&world, 1).resolve(token).await.is_some(),
        "portal B (the OTHER machine) serves the same link"
    );

    // device A revokes NOW: tombstone, later ts (the dashboard's
    // review_revoke publishes exactly this record)
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "review_link",
            key: token,
            payload: br#"{"token":"tok-phase2-e2e-revoke0000000000000000","revoked":true}"#,
            ts_ms: 200,
            tombstone: true,
        },
    )
    .unwrap();

    // ONE sync pass each way: the tombstone reaches B's record table
    pass(&mut world, 0).await;
    pass(&mut world, 1).await;
    let row = engine_store(&world, 1)
        .get_state_record("p1", "review_link", token)
        .expect("the tombstone reached device B");
    assert!(row.tombstone, "B holds the revocation record");

    // B's machine-local review.json STILL lists the link as live — the
    // synced record is what kills it (this is the whole point of Phase 2)
    let b_file = review_store::Store::load(&ws(&world, 1)).unwrap().unwrap();
    assert!(
        GuestLink::resolve(&b_file.links, token, 1_000).is_some(),
        "B's local file is untouched: the synced record must decide"
    );

    // the portal on the OTHER machine now refuses the token, exactly like
    // an unknown one (fresh portal = a fresh resolve; a warm portal picks
    // the tombstone up within the 3s revocation cache TTL)
    let p_b = portal_for(&world, 1);
    assert!(
        p_b.resolve(token).await.is_none(),
        "revoked on A => dead on B's portal"
    );
    assert!(
        p_b.resolve("never-a-token").await.is_none(),
        "unknown stays unknown"
    );
    // ...and A's own portal refuses it too (local file still lists it; the
    // synced tombstone decides)
    assert!(
        portal_for(&world, 0).resolve(token).await.is_none(),
        "revoked is dead on the revoking machine as well"
    );
}

// ---------------------------------------------------------------------------
// 6. ADR-0031 Phase 3: the file-level payoff — `.cairn` materializes records
// ---------------------------------------------------------------------------

use cairn_sync::state_records::{AppliedRecord, StateMaterializer};

/// The daemon-shaped materializer over ONE device root (what
/// `RootMaterializer` does in cairn-cli/src/materialize.rs, mirrored here in
/// miniature: cairn-cli is a bin-only crate cairn-sim cannot depend on, so
/// the E2E drives the same record→file shapes through the same public APIs —
/// `cairn_core::rbac::MemberFile` for members.json, `cairn_review::store` for
/// review.json / notes). The per-family unit tests live on the production
/// impl; this one exists to prove the SEAM end to end over the real server.
struct FileMaterializer {
    root: PathBuf,
}

impl StateMaterializer for FileMaterializer {
    fn materialize(&self, _project: &str, applied: &[AppliedRecord]) {
        for rec in applied {
            match rec.family.as_str() {
                "member" => self.member(rec),
                "review_version" => self.version(rec),
                "review_link" => self.link(rec),
                "review_comment" => self.comment(rec),
                _ => {}
            }
        }
    }
}

impl FileMaterializer {
    fn member(&self, rec: &AppliedRecord) {
        let path = ws_members(&self.root);
        let mut f = match std::fs::read(&path) {
            Ok(b) => cairn_core::rbac::MemberFile::from_json(&b).unwrap(),
            Err(_) => cairn_core::rbac::MemberFile::default(),
        };
        if rec.tombstone {
            f.remove(&rec.key);
        } else {
            let m: cairn_core::rbac::Member = serde_json::from_slice(&rec.payload).unwrap();
            if f.members
                .get(&rec.key)
                .is_some_and(|e| rec.ts_ms < e.added_at_ms)
            {
                return; // LWW echo guard (see the production impl's module doc)
            }
            f.members.insert(rec.key.clone(), m);
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, f.to_json().unwrap()).unwrap();
    }

    fn review(&self) -> ReviewFile {
        cairn_review::store::Store::load(&self.root)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn version(&self, rec: &AppliedRecord) {
        let mut f = self.review();
        let number: u32 = rec.key.parse().unwrap();
        if f.version(number).is_some() {
            return;
        }
        let v: serde_json::Value = serde_json::from_slice(&rec.payload).unwrap();
        f.versions.push(cairn_review::model::ReviewVersion {
            number,
            label: v["label"].as_str().unwrap_or_default().into(),
            media_rel: v["media_rel"].as_str().unwrap_or_default().into(),
            proxy_rel: None,
            fps_num: v["fps_num"].as_u64().unwrap_or(24) as u32,
            fps_den: v["fps_den"].as_u64().unwrap_or(1) as u32,
            frames: v["frames"].as_u64().unwrap_or(0),
            timeline_fingerprint: None,
            snapshot: None,
            published_by: v["published_by"].as_str().unwrap_or_default().into(),
            published_at: v["published_at"].as_i64().unwrap_or_default(),
        });
        f.versions.sort_by_key(|x| x.number);
        cairn_review::store::Store::save(&self.root, &f).unwrap();
    }

    fn link(&self, rec: &AppliedRecord) {
        let mut f = self.review();
        if rec.tombstone {
            // always applies (fail-closed — the Phase-3 revoke payoff)
            f.revoke_link(&rec.key);
        } else {
            let v: serde_json::Value = serde_json::from_slice(&rec.payload).unwrap();
            let link = GuestLink {
                token: rec.key.clone(),
                role: GuestRole::parse(v["role"].as_str().unwrap_or("commenter"))
                    .unwrap_or(GuestRole::Commenter),
                note: v["note"].as_str().unwrap_or_default().into(),
                expires_at: v["expires_at"].as_i64().unwrap_or(0),
                latest_only: v["latest_only"].as_bool().unwrap_or(false),
                created_at: v["created_at"].as_i64().unwrap_or(rec.ts_ms),
            };
            if f.links
                .iter()
                .any(|l| l.token == rec.key && rec.ts_ms < l.created_at)
            {
                return; // LWW echo guard
            }
            f.links.retain(|l| l.token != rec.key);
            f.links.push(link);
        }
        cairn_review::store::Store::save(&self.root, &f).unwrap();
    }

    fn comment(&self, rec: &AppliedRecord) {
        let v: serde_json::Value = serde_json::from_slice(&rec.payload).unwrap();
        let version = v["version"].as_u64().unwrap() as u32;
        let mut set = cairn_review::store::Store::load_comments(&self.root, version).unwrap();
        if rec.tombstone {
            set.notes.remove(&rec.key);
        } else {
            let note: cairn_tl::notes::Note = serde_json::from_value(v["note"].clone()).unwrap();
            if set
                .notes
                .get(&note.id)
                .is_some_and(|e| rec.ts_ms < e.created_ms)
            {
                return; // LWW echo guard
            }
            set.notes.insert(note.id.clone(), note);
        }
        cairn_review::store::Store::save_comments(&self.root, version, &set).unwrap();
    }
}

fn ws_members(root: &Path) -> PathBuf {
    root.join(".cairn").join("members.json")
}

fn install_materializers(world: &mut World) {
    for i in 0..2 {
        let root = ws(world, i);
        world.devices[i]
            .engine
            .as_mut()
            .expect("device live")
            .materializer = Some(Arc::new(FileMaterializer { root }));
    }
}

fn members_at(world: &World, i: usize) -> cairn_core::rbac::MemberFile {
    cairn_core::rbac::MemberFile::from_json(&std::fs::read(ws_members(&ws(world, i))).unwrap())
        .unwrap()
}

/// THE Phase-3 payoff, end to end over the real server journal: A mints a
/// member + a review version + a guest link; B never wrote any of it — its
/// `.cairn` directory MATERIALIZES from the applied records (members.json
/// gains the member, review.json gains version + link). A revokes → B's
/// review.json itself no longer lists the link (beside the Phase-2 portal
/// consult pinned by the test above). B writes a comment offline → A's
/// review-notes file gains it.
#[tokio::test]
async fn phase3_records_materialize_into_local_cairn_files_e2e() {
    let mut world = World::boot(67).await;
    install_materializers(&mut world);
    let token = "tok-phase3-materialize000000000000000"; // 32 chars

    // A publishes v1 (the real file write + the synced review_version record,
    // the exact payload shape service/actions.rs publishes)
    let mut f = ReviewFile {
        title: "Brand Film".into(),
        ..Default::default()
    };
    f.publish(ReviewVersion {
        number: 0,
        label: "v1".into(),
        media_rel: "cuts/v1.mp4".into(),
        proxy_rel: None,
        fps_num: 24,
        fps_den: 1,
        frames: 100,
        timeline_fingerprint: None,
        snapshot: None,
        published_by: "editor-a".into(),
        published_at: 50,
    });
    cairn_review::store::Store::save(&ws(&world, 0), &f).unwrap();
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "review_version",
            key: "1",
            payload: br#"{"number":1,"label":"v1","media_rel":"cuts/v1.mp4","fps_num":24,"fps_den":1,"frames":100,"published_by":"editor-a","published_at":50}"#,
            ts_ms: 60,
            tombstone: false,
        },
    )
    .unwrap();

    // A mints the guest link (the real file write + the synced record)
    let mut f = cairn_review::store::Store::load(&ws(&world, 0))
        .unwrap()
        .unwrap();
    f.links.push(GuestLink {
        token: token.into(),
        role: GuestRole::Commenter,
        note: "acme client".into(),
        expires_at: 0,
        latest_only: false,
        created_at: 100,
    });
    cairn_review::store::Store::save(&ws(&world, 0), &f).unwrap();
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "review_link",
            key: token,
            payload: br#"{"token":"tok-phase3-materialize000000000000000","role":"commenter","note":"acme client","expires_at":0,"latest_only":false,"created_at":100}"#,
            ts_ms: 100,
            tombstone: false,
        },
    )
    .unwrap();
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "member",
            key: "dev-9",
            payload: br#"{"device_id":"dev-9","name":"Rook","role":"colorist","added_at_ms":90,"added_by":"dev-0"}"#,
            ts_ms: 90,
            tombstone: false,
        },
    )
    .unwrap();

    // one pass each way: B (which never opened the portal, never wrote a
    // members file) materializes A's state into ITS OWN .cairn directory
    pass(&mut world, 0).await;
    pass(&mut world, 1).await;

    let b_members = members_at(&world, 1);
    assert!(
        b_members.members.contains_key("dev-9"),
        "B's members.json gained the member from the record"
    );
    assert_eq!(
        b_members.members["dev-9"].role,
        cairn_core::rbac::Role::Colorist
    );
    let b_file = review_store::Store::load(&ws(&world, 1))
        .unwrap()
        .expect("B's review.json was materialized from the records");
    assert_eq!(b_file.versions.len(), 1, "B gained the version stack");
    assert_eq!(b_file.versions[0].media_rel, "cuts/v1.mp4");
    assert!(
        GuestLink::resolve(&b_file.links, token, 1_000).is_some(),
        "B's review.json gained the guest link"
    );

    // A revokes; the tombstone rides one pass — and now the FILE-level
    // payoff: B's review.json itself no longer lists the link (the Phase-2
    // test above pins the record-level consult; THIS is what Phase 3 adds)
    enqueue_state_record(
        engine_store(&world, 0),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-0",
            family: "review_link",
            key: token,
            payload: br#"{"token":"tok-phase3-materialize000000000000000","revoked":true}"#,
            ts_ms: 200,
            tombstone: true,
        },
    )
    .unwrap();
    pass(&mut world, 0).await;
    pass(&mut world, 1).await;
    let b_file = review_store::Store::load(&ws(&world, 1)).unwrap().unwrap();
    assert!(
        GuestLink::resolve(&b_file.links, token, 1_000).is_none(),
        "the cross-machine revoke removed the link from B's local FILE"
    );
    assert_eq!(b_file.versions.len(), 1, "the revoke touched only the link");

    // B writes a comment OFFLINE (no version file existed on B before): the
    // synced review_comment record carries it to A, whose notes file
    // materializes it — the comment crosses machines as a record, not a file
    let note = cairn_tl::notes::Note::new(
        "client-jane",
        "tighten the cut here",
        cairn_tl::notes::NoteAnchor {
            clip: None,
            frame: 42,
            rate: 24,
            range: None,
        },
        cairn_tl::notes::NoteStatus::Open,
        300,
    );
    let payload = format!(
        r#"{{"version":1,"note":{}}}"#,
        serde_json::to_string(&note).unwrap()
    );
    enqueue_state_record(
        engine_store(&world, 1),
        PublishParams {
            tenant_id: "t1",
            project_id: "p1",
            local_ns: "p1",
            device_id: "dev-1",
            family: "review_comment",
            key: &note.id,
            payload: payload.as_bytes(),
            ts_ms: 300,
            tombstone: false,
        },
    )
    .unwrap();
    pass(&mut world, 1).await; // B pushes its outbox entry through the server
    pass(&mut world, 0).await; // A pulls the record and materializes the note file

    let a_notes =
        cairn_review::store::Store::load_comments(&ws(&world, 0), 1).expect("A's v1 notes file");
    assert!(
        a_notes.notes.contains_key(&note.id),
        "A's .cairn/review-notes/v1.json gained B's offline comment"
    );
    // (B's own file is NOT written for its own record — own-op suppression
    // means the local machine that authors a comment also writes its own
    // file at authoring time, exactly like every other publish surface.)
}
