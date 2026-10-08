//! Real-media editing-session E2E (Task 2-d MUST 5).
//!
//! HONEST SCOPE: licensed NLE hosts (Premiere, Resolve — GUI apps) remain
//! platform-gated AMBER cells in docs/nle-matrix-results; no test binary
//! can click inside them. This suite is the AUTOMATED STAND-IN: real media
//! files, real sync over the real server, real ffprobe stream parity, a
//! real full decode, the real cairn-proxy transcode pipeline, and the real
//! FCPXML bridge — everything a licensed host would consume, proven at the
//! file level.
//!
//! Topology: the `cairn_sim::net_harness` two-home shape — ONE real
//! cairn-server as the only meeting point, home A on 127.0.0.2, home B on
//! 127.0.0.3, both through the REAL GrpcPlane (no P2P by construction).
//!
//! Scenario:
//! 1. ffmpeg generates REAL media at home A: 6 s, 1280x720, 24 fps h264 + AAC (lavfi testsrc + sine — no assets needed);
//! 2. sync A→B through the real server; blake3-identical at B;
//! 3. (a) ffprobe on B's synced copy reports byte-identical stream metadata to the source (streams / fps / frames all match);
//! 4. (b) ffmpeg decodes B's copy END TO END (`-f null -`, exit 0) and the decoded frame count matches the source exactly;
//! 5. (c) editor round-trip: home B builds a 360p proxy through the SAME pipeline the proxy service runs (cairn_proxy::pipeline::generate + FfmpegTranscoder — the service calls exactly this inside spawn_blocking), the proxy bytes sync B→A, byte-identical at A, and they are a REAL 360p h264 file (ffprobe, not vibes);
//! 6. (d) timeline round-trip: a minimal REAL FCPXML referencing the synced media syncs A→B; cairn-tl parses the SYNCED bytes on B and the referenced media RESOLVES on B's disk.
//!
//! Graceful degradation: if ffmpeg/ffprobe are absent at runtime the test
//! eprintln-skips (they ARE present in CI/sandbox, so it RUNS there).

use cairn_sim::net_harness;
use cairn_tl::fcpxml::parse_fcpxml;

const TENANT: &str = "t-edit";
const PROJECT: &str = "p-edit";

/// 6 s @ 24 fps = 144 video frames (asserted ≥ 100 so a generator change
/// fails loudly, while the EXACT count is asserted equal source↔copy).
const EXPECTED_FRAMES_MIN: u64 = 100;

#[tokio::test]
async fn editing_session_real_media_round_trip() {
    if !net_harness::ffmpeg_and_ffprobe_present() {
        // honest skip — but the sandbox/CI ships both, so this RUNS there
        eprintln!("skipping: ffmpeg+ffprobe required for the real-media editing session");
        return;
    }

    // ---- the meeting point + two homes (real server, real gRPC planes) ----
    let mp = net_harness::boot_meeting_point(TENANT, PROJECT).await;
    let (token_a, device_a) = net_harness::enroll(&mp, TENANT, "edit-a").await;
    let (token_b, device_b) = net_harness::enroll(&mp, TENANT, "edit-b").await;
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

    // ---- REAL media generated at home A (editorial ingest stand-in) ----
    let src_bytes = generate_source_media();
    home_a.write_and_mark_dirty("reel/hero.mp4", &src_bytes);

    // ---- sync A→B through the real server ----
    let (a_stats, _) = home_a.pass().await;
    assert!(
        a_stats.uploaded_chunks > 0,
        "the real media must upload through the server: {a_stats:?}"
    );
    home_b.pass().await;
    home_b.pass().await;
    assert_eq!(
        home_a.blake3_of("reel/hero.mp4"),
        home_b.blake3_of("reel/hero.mp4"),
        "the source media must arrive byte-identical at home B"
    );
    println!(
        "SYNCED real media reel/hero.mp4 ({} bytes) A→B via the meeting point",
        src_bytes.len()
    );

    // ---- (a) ffprobe stream parity: synced copy vs source ----
    let src_probe = probe_streams(&home_a.ws().join("reel/hero.mp4"));
    let b_probe = probe_streams(&home_b.ws().join("reel/hero.mp4"));
    assert!(!src_probe.is_empty(), "ffprobe must read the source");
    assert_eq!(
        src_probe, b_probe,
        "the synced copy's stream metadata must match the source EXACTLY"
    );
    // the metadata is not just identical but describes the REAL media
    assert!(
        src_probe.contains("codec_name=h264"),
        "h264 video: {src_probe}"
    );
    assert!(
        src_probe.contains("codec_name=aac"),
        "aac audio: {src_probe}"
    );
    assert!(src_probe.contains("width=1280"), "1280 wide: {src_probe}");
    assert!(src_probe.contains("height=720"), "720 tall: {src_probe}");
    assert!(
        src_probe.contains("r_frame_rate=24/1"),
        "24 fps: {src_probe}"
    );
    let src_frames = count_decoded_frames(&home_a.ws().join("reel/hero.mp4"));
    let b_frames = count_decoded_frames(&home_b.ws().join("reel/hero.mp4"));
    assert!(
        src_frames >= EXPECTED_FRAMES_MIN,
        "real frame count: {src_frames}"
    );
    assert_eq!(
        src_frames, b_frames,
        "the synced copy must decode to the EXACT source frame count"
    );

    // ---- (b) full decode of the synced copy: every packet, no errors ----
    let decode = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(home_b.ws().join("reel/hero.mp4"))
        .args(["-f", "null", "-"])
        .output()
        .expect("spawn ffmpeg decode");
    assert!(
        decode.status.success(),
        "full decode of the synced copy must succeed: {}",
        String::from_utf8_lossy(&decode.stderr)
    );
    println!("DECODED synced copy end to end: {b_frames} frames, exit 0");

    // ---- (c) editor round-trip: 360p proxy on B through the SERVICE's pipeline ----
    // The proxy service (cairn-cli/src/service/proxy.rs) calls exactly
    // `pipeline::generate(root, media_rel, &profile, &FfmpegTranscoder, now)`
    // inside spawn_blocking — same call, same transcoder, same index.
    let profile = cairn_proxy::model::ProxyProfile {
        max_height: 360,
        codec: "h264".into(),
        crf: 28,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let ws_b = home_b.ws(); // the attach root: what the service would bind
    let entry = cairn_proxy::pipeline::generate(
        &ws_b,
        "reel/hero.mp4",
        &profile,
        &cairn_proxy::transcode::FfmpegTranscoder,
        now_ms,
    )
    .expect("the real transcoder must produce the 360p proxy");
    assert!(entry.bytes > 0, "proxy bytes: {entry:?}");
    assert!(entry.proxy_rel.starts_with(".cairn/proxy-cache/"));
    // the index agrees it is READY for this digest (the status endpoint's source)
    let (latest, st) = cairn_proxy::pipeline::status_of_fast(&ws_b, "reel/hero.mp4", false)
        .expect("index readable")
        .expect("indexed entry");
    assert_eq!(latest.source_digest, entry.source_digest);
    assert!(
        matches!(st, cairn_proxy::model::ProxyStatus::Ready),
        "{st:?}"
    );
    // and it is a REAL 360p file, not a copy of the source
    let proxy_probe = probe_streams(&ws_b.join(&entry.proxy_rel));
    assert!(
        proxy_probe.contains("codec_name=h264"),
        "proxy codec: {proxy_probe}"
    );
    assert!(
        proxy_probe.contains("height=360"),
        "proxy is 360p: {proxy_probe}"
    );
    assert!(
        !proxy_probe.contains("height=720"),
        "proxy must NOT be the 720p source: {proxy_probe}"
    );

    // Editorial proxies that must travel ride the project tree (the SPEC §10
    // ignore list keeps derived `.cairn/` state OUT of the journal by
    // design): the editor materializes the proxy as a project file and syncs.
    let proxy_bytes = std::fs::read(ws_b.join(&entry.proxy_rel)).unwrap();
    home_b.write_and_mark_dirty("proxies/hero_360.mp4", &proxy_bytes);
    home_b.pass().await;
    home_a.pass().await;
    home_a.pass().await;
    assert_eq!(
        home_b.blake3_of("proxies/hero_360.mp4"),
        home_a.blake3_of("proxies/hero_360.mp4"),
        "the REAL proxy bytes must arrive byte-identical at home A"
    );
    let a_proxy_probe = probe_streams(&home_a.ws().join("proxies/hero_360.mp4"));
    assert_eq!(
        proxy_probe, a_proxy_probe,
        "the synced proxy must be the same real 360p media at A"
    );
    println!(
        "PROXY round-trip: 360p h264 proxy ({} bytes) transcoded at B, byte-identical at A",
        proxy_bytes.len()
    );

    // ---- (d) timeline round-trip: a REAL FCPXML referencing the synced media ----
    let fcpxml = minimal_fcpxml("file://reel/hero.mp4");
    home_a.write_and_mark_dirty("sessions/session.fcpxml", fcpxml.as_bytes());
    home_a.pass().await;
    home_b.pass().await;
    home_b.pass().await;
    assert_eq!(
        home_a.blake3_of("sessions/session.fcpxml"),
        home_b.blake3_of("sessions/session.fcpxml"),
        "the timeline must arrive byte-identical at home B"
    );

    // cairn-tl parses the SYNCED bytes on B (the bridge, not a shim)
    let synced_xml = std::fs::read_to_string(home_b.ws().join("sessions/session.fcpxml")).unwrap();
    let tl = parse_fcpxml(&synced_xml).expect("the synced FCPXML must parse");
    assert_eq!(tl.name, "Session Round Trip");
    let spine = &tl.tracks.children[0];
    let clip = &spine.children[0];
    assert_eq!(clip.name, "Hero");
    let url = clip
        .active_media_url()
        .expect("the asset-clip references its media");
    assert_eq!(
        url, "file://reel/hero.mp4",
        "the media reference survives sync"
    );
    // ...and the referenced media RESOLVES on B's disk (strip file:// → project rel)
    let rel = url
        .strip_prefix("file://")
        .expect("project-relative file URL");
    let resolved = home_b.ws().join(rel);
    assert!(
        resolved.is_file(),
        "the timeline's referenced media must resolve on B: {}",
        resolved.display()
    );
    assert_eq!(
        blake3::hash(&std::fs::read(&resolved).unwrap()).to_string(),
        blake3::hash(&src_bytes).to_string(),
        "the resolved media is the SAME bytes the timeline was cut against"
    );
    println!(
        "TIMELINE round-trip: FCPXML parsed on B, media reference resolves to the synced bytes"
    );

    println!("EDITING SESSION E2E: real media, real sync, real decodes, real proxy, real FCPXML — all green");
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Generate the source media: 6 s, 1280x720 @ 24 fps, h264 + AAC
/// (lavfi testsrc picture + sine tone — deterministic, asset-free).
fn generate_source_media() -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("hero.mp4");
    let out = std::process::Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=size=1280x720:rate=24"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", "6"])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a",
            "128k",
        ])
        .arg(&dst)
        .output()
        .expect("spawn ffmpeg source generation");
    assert!(
        out.status.success(),
        "ffmpeg source generation failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read(&dst).unwrap()
}

/// `ffprobe -show_streams` default key=value output (one block per stream).
fn probe_streams(path: &std::path::Path) -> String {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// FULL-decode frame count of the video stream (`-count_frames` decodes).
fn count_decoded_frames(path: &std::path::Path) -> u64 {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe count");
    assert!(
        out.status.success(),
        "ffprobe -count_frames failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or_else(|e| panic!("nb_read_frames not a number: {e}"))
}

/// A minimal but real-shaped FCPXML 1.11 document (same structural spine
/// subset cairn-tl's bridge tests use): one format, one asset whose
/// media-rep points at the PROJECT-RELATIVE synced media, one asset-clip on
/// the spine. 6 s = 144/24 s, exact rationals end to end.
fn minimal_fcpxml(media_url: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE fcpxml>
<fcpxml version="1.11">
  <resources>
    <format id="r1" name="FFVideoFormat720p24" frameDuration="100/2400s" width="1280" height="720"/>
    <asset id="a1" name="Hero" uid="hero-session-1" start="0s" duration="144/24s" hasVideo="1" hasAudio="1" format="r1">
      <media-rep kind="original-media" src="{media_url}"/>
    </asset>
  </resources>
  <library>
    <event name="Editing Session">
      <project name="Session Round Trip">
        <sequence id="s1" format="r1" duration="144/24s" tcStart="0s" tcFormat="NDF" name="Seq">
          <spine>
            <asset-clip name="Hero" ref="a1" offset="0s" duration="144/24s" start="0s"/>
          </spine>
        </sequence>
      </project>
    </event>
  </library>
</fcpxml>
"#
    )
}
