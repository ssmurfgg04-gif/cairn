//! Two-home cross-network E2E — the "two different home internet
//! connections" stand-in (Task 2-d MUST 4).
//!
//! TOPOLOGY (see `cairn_sim::net_harness` for the honest details): ONE real
//! cairn-server on 127.0.0.1 (gRPC + objects HTTP, ephemeral ports) is the
//! ONLY meeting point; TWO full engine stacks ("home A" on 127.0.0.2, "home
//! B" on 127.0.0.3) with own home dirs/stores/roots/identities talk to it
//! through the REAL `GrpcPlane`. P2P is OFF BY CONSTRUCTION: the engines
//! hold no swarm handle at all, so every byte of media and every journal
//! entry flows through the real server — exactly the "all traffic via the
//! hosted meeting point" topology the deployment runbook ships. Distinct
//! addresses/ports AND distinct identities are asserted below; if a host
//! refuses non-.1 loopback binds, the harness falls back to distinct
//! 127.0.0.1 ports and says so.
//!
//! SCENARIO: 50 MB of real generated media A→B (blake3 both sides) →
//! kill -9 home B mid-sync (metadata applied, bytes not yet materialized)
//! → restart → convergence → a concurrent write from BOTH homes to the
//! SAME path → §7.1 conflict copy on the loser, byte-identical convergence
//! on both → review-link mint + revoke on A rides the synced state-record
//! family to B's store. The Phase-2 PORTAL consult (portal refuses a
//! revoked link after sync) is Task 2-a's integration test; until that
//! branch merges, this test asserts the substrate: the tombstone record
//! ARRIVES at B via the state-records store.
//!
//! Licensed-NLE honesty: this file proves the TRANSPORT and CONVERGENCE
//! story between two homes. "Open and play properly in the NLE" is the
//! editing_session_e2e.rs suite (real decodes); licensed Premiere/Resolve
//! GUI hosts remain platform-gated amber cells in docs/nle-matrix-results.

use cairn_sim::net_harness::{self, Home, MeetingPoint};
use cairn_sync::state_records::{enqueue_state_record, PublishParams};

const TENANT: &str = "t-2home";
const PROJECT: &str = "p-2home";

// ---------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_homes_convergence_and_recovery() {
    // ---- ONE real server = the hosted meeting point on 127.0.0.1 ----
    let mp: MeetingPoint = net_harness::boot_meeting_point(TENANT, PROJECT).await;
    let (token_a, device_a) = net_harness::enroll(&mp, TENANT, "home-a").await;
    let (token_b, device_b) = net_harness::enroll(&mp, TENANT, "home-b").await;
    assert_ne!(device_a, device_b, "two distinct server identities");

    // ---- TWO homes on DISTINCT loopback addresses ----
    let mut home_a = net_harness::open_home(
        "A",
        "127.0.0.2:0",
        &mp,
        &token_a,
        &device_a,
        TENANT,
        PROJECT,
    )
    .await;
    let mut home_b = net_harness::open_home(
        "B",
        "127.0.0.3:0",
        &mp,
        &token_b,
        &device_b,
        TENANT,
        PROJECT,
    )
    .await;
    assert_ne!(
        home_a.console_addr, home_b.console_addr,
        "the two homes hold distinct addresses/ports"
    );

    // ---- 1) ~50 MB of REAL generated media, written at home A ----
    // Incompressible-ish pseudo-random bytes (a 50 MB camera file does not
    // compress to nothing; neither does this). One REAL small mp4 joins it
    // when ffmpeg exists (it does in CI; the hermetic fallback keeps the
    // transport proof runnable anywhere).
    let big = net_harness::payload(50 * 1024 * 1024, 0xC41A7);
    home_a.write_and_mark_dirty("reel/interview.braw", &big);
    let mp4_bytes = generate_real_mp4(home_a.ws().join("reel/slate.mp4"));
    if let Some(ref mp4) = mp4_bytes {
        home_a.write_and_mark_dirty("reel/slate.mp4", mp4);
    }

    // A pushes through the real server; B discovers and materializes
    let (a_stats, _) = home_a.pass().await;
    assert!(
        a_stats.uploaded_chunks > 0,
        "A must upload chunks through the server: {a_stats:?}"
    );
    home_b.pass().await;
    home_b.pass().await; // second pass absorbs cursor/first-round skew

    // ---- 2) byte-identical convergence, proven by blake3 on BOTH sides ----
    assert_eq!(
        home_a.blake3_of("reel/interview.braw"),
        home_b.blake3_of("reel/interview.braw"),
        "50 MB media must arrive byte-identical at the second home"
    );
    if mp4_bytes.is_some() {
        assert_eq!(
            home_a.blake3_of("reel/slate.mp4"),
            home_b.blake3_of("reel/slate.mp4"),
            "the real mp4 must arrive byte-identical too"
        );
    }
    println!(
        "CONVERGED reel/interview.braw ({} bytes) across two homes via the server",
        big.len()
    );

    // ---- 3) kill -9 home B MID-SYNC, restart, converge ----
    // A publishes another real file while B is "online"; B applies the
    // metadata (sync pass) but is killed BEFORE the bytes are materialized.
    let second = net_harness::payload(8 * 1024 * 1024, 0xB105);
    home_a.write_and_mark_dirty("reel/broll.mov", &second);
    home_a.pass().await;
    let mid = home_b.sync_only().await;
    assert!(
        mid.applied_entries > 0,
        "pre-crash: B applied the metadata only, got {mid:?}"
    );
    assert!(
        !home_b.ws().join("reel/broll.mov").exists(),
        "pre-crash: metadata only, file not yet materialized"
    );
    // kill -9 analogue: drop every handle without flush/close; reopen from
    // disk (WAL replay). The meeting point never knew.
    net_harness::crash_and_restart(&mut home_b, &mp.grpc_url, &token_b).await;
    home_b.pass().await;
    assert_eq!(
        home_a.blake3_of("reel/broll.mov"),
        home_b.blake3_of("reel/broll.mov"),
        "post-crash home B must converge byte-identical (I2: nothing lost mid-sync)"
    );
    println!("RECOVERED reel/broll.mov at home B after mid-sync kill -9 + restart");

    // ---- 4) concurrent write from BOTH homes to the SAME path (§7.1) ----
    // Common base first: both homes hold notes/cut.otio.
    let base = b"otio base - shared cut".to_vec();
    home_a.write_and_mark_dirty("notes/cut.otio", &base);
    home_a.pass().await;
    net_harness::converge(&mut home_a, &mut home_b, 4).await;
    assert_eq!(
        home_b.blake3_of("notes/cut.otio"),
        blake3::hash(&base).to_hex().to_string()
    );

    // ...then BOTH sides edit the same path offline ("two homes, same night"):
    let version_a = b"otio - home A re-cut (head trim)".to_vec();
    let version_b = b"otio - home B re-cut (tail trim)".to_vec();
    home_a.write_and_mark_dirty("notes/cut.otio", &version_a);
    home_b.write_and_mark_dirty("notes/cut.otio", &version_b);

    // A wins the journal race; B's append is refused by the server's
    // seq>base rule → the engine renames B's edit to a conflict copy.
    home_a.pass().await;
    let (b_stats, _) = home_b.pass().await;
    assert_eq!(
        b_stats.conflicts_resolved, 1,
        "the real server must refuse the divergent append (§7.1): {b_stats:?}"
    );

    // Converge: original path = the winner on BOTH homes; the loser's edit
    // survives as a conflict copy that ALSO reaches the winner via sync.
    net_harness::converge(&mut home_a, &mut home_b, 6).await;
    assert_eq!(
        home_a.blake3_of("notes/cut.otio"),
        blake3::hash(&version_a).to_hex().to_string(),
        "original path converges on A's winner"
    );
    assert_eq!(
        home_b.blake3_of("notes/cut.otio"),
        blake3::hash(&version_a).to_hex().to_string(),
        "original path converges on A's winner at home B too"
    );
    // find B's conflict copy row (§7.1 naming: "cut (conflict — <device> — DATE).otio")
    let copy_rel = {
        let engine = home_b.engine.as_ref().expect("home b live");
        engine
            .store
            .list_files(PROJECT)
            .into_iter()
            .map(|r| r.path)
            .find(|p| *p != "notes/cut.otio" && p.contains("cut") && p.contains("conflict"))
            .expect("conflict copy row must exist at home B")
    };
    assert_eq!(
        home_b.blake3_of(&copy_rel),
        blake3::hash(&version_b).to_hex().to_string(),
        "the conflict copy preserves home B's edit byte-identical"
    );
    assert_eq!(
        home_a.blake3_of(&copy_rel),
        blake3::hash(&version_b).to_hex().to_string(),
        "the conflict copy SYNCED to home A too — nobody loses an edit across homes"
    );
    println!("CONFLICT §7.1 resolved: copy '{copy_rel}' preserved on both homes");

    // ---- 5) review-link mint + revoke on A rides the state records to B ----
    // Phase-1 substrate check: the synced review_link family carries the
    // mint and the LATER tombstone; LWW converges both stores on the
    // tombstone. (The portal refusing revoked links after sync is Task
    // 2-a's Phase-2 integration test — not merged into this branch.)
    let link_key = "tok-2homes";
    enqueue_state_record(
        &home_a.engine.as_ref().expect("home a live").store,
        PublishParams {
            tenant_id: TENANT,
            project_id: PROJECT,
            local_ns: PROJECT,
            device_id: &home_a.device_id,
            family: "review_link",
            key: link_key,
            payload: br#"{"token":"tok-2homes","role":"viewer","note":"guest cut"}"#,
            ts_ms: 1_000,
            tombstone: false,
        },
    )
    .unwrap();
    net_harness::converge(&mut home_a, &mut home_b, 4).await;
    let link = engine_of(&mut home_b)
        .store
        .get_state_record(PROJECT, "review_link", link_key)
        .expect("minted link record must reach home B");
    assert!(!link.tombstone, "pre-revoke: the link is live at home B");

    enqueue_state_record(
        &home_a.engine.as_ref().expect("home a live").store,
        PublishParams {
            tenant_id: TENANT,
            project_id: PROJECT,
            local_ns: PROJECT,
            device_id: &home_a.device_id,
            family: "review_link",
            key: link_key,
            payload: br#"{"token":"tok-2homes","revoked":true}"#,
            ts_ms: 2_000,
            tombstone: true,
        },
    )
    .unwrap();
    net_harness::converge(&mut home_a, &mut home_b, 4).await;

    for home in [&mut home_a, &mut home_b] {
        let row = engine_of(home)
            .store
            .get_state_record(PROJECT, "review_link", link_key)
            .unwrap_or_else(|| panic!("home {}: link row missing", home.tag));
        assert!(
            row.tombstone,
            "home {}: the revoke tombstone wins (LWW)",
            home.tag
        );
        assert_eq!(
            row.ts_ms, 2_000,
            "home {}: the revoke is the latest write",
            home.tag
        );
    }
    println!("REVOKE: tombstone for '{link_key}' converged on both homes via the server");

    // The distinct-address topology held for the whole scenario.
    assert_ne!(home_a.console_addr, home_b.console_addr);
    let _ = (&home_a.console, &home_b.console);
}

fn engine_of(home: &mut Home) -> &mut cairn_sync::Engine {
    home.engine.as_mut().expect("home is live")
}

/// Generate a small REAL mp4 with ffmpeg (lavfi testsrc) when ffmpeg is
/// available; None otherwise (the default run stays hermetic on random
/// bytes — the media generation itself is exercised in
/// editing_session_e2e.rs, which asserts real decodes).
fn generate_real_mp4(dst: std::path::PathBuf) -> Option<Vec<u8>> {
    if !net_harness::ffmpeg_and_ffprobe_present() {
        eprintln!("no ffmpeg in this environment — running hermetic (random bytes only)");
        return None;
    }
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=24",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&dst)
        .output()
        .expect("spawn ffmpeg");
    if !out.status.success() {
        eprintln!(
            "ffmpeg source generation failed ({}) — running hermetic",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(std::fs::read(&dst).unwrap())
}
