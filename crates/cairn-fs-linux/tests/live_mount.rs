//! LIVE mount round-trip — the FUSE "last mile" (runs ONLY where /dev/fuse exists).
//!
//! Ignored by default (the ubuntu-latest CI fleet has no /dev/fuse; the `fuse-linux`
//! job compiles and unit-gates this crate). The self-hosted runner labeled
//! `linux,fuse` runs it non-ignored via `fuse-mount-live.yml`:
//!
//! ```text
//! cargo test -p cairn-fs-linux --features fuse --test live_mount -- --ignored --nocapture
//! ```
//!
//! Everything here goes through the REAL kernel FUSE boundary — no API shortcuts:
//! std::fs on the mountpoint exercises open/write/flush/release (spool → FastCDC →
//! CAS → manifest write-back), reads serve from the header cache, readdir resolves
//! virtual directories, and a planted foreign domain lease surfaces as EBUSY
//! (ADR-0014 Phase 2 scoping through the actual kernel).

#![cfg(feature = "fuse")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cairn_core::clock::{SystemClock, WallClock};
use cairn_store::{Cas, Store};

struct Skip;

fn require_live_fuse() -> Result<(), Skip> {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: no /dev/fuse on this host (compile-only fleet)");
        return Err(Skip);
    }
    let have = ["fusermount3", "fusermount"].into_iter().any(|b| {
        Command::new(b)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    });
    if !have {
        eprintln!("SKIP: no fusermount3/fusermount binary (install the fuse3 package)");
        return Err(Skip);
    }
    Ok(())
}

fn fusermount_unmount(mp: &Path) {
    for (bin, args) in [
        ("fusermount3", vec!["-u", "-z"]),
        ("fusermount", vec!["-u", "-z"]),
        ("umount", vec!["-l"]),
    ] {
        if Command::new(bin)
            .args(&args)
            .arg(mp)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }
    }
    panic!(
        "could not unmount {} — stale mount left behind; unmount manually",
        mp.display()
    );
}

fn wait_mounted(mp: &Path, parent_dev: u64) -> Result<(), ()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(md) = std::fs::metadata(mp) {
            use std::os::unix::fs::MetadataExt;
            if md.dev() != parent_dev {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(())
}

fn open_store(dir: &Path) -> (Store, Cas) {
    let store = Store::open(&dir.join("store"), Arc::new(WallClock)).unwrap();
    let cas = Cas::open(&dir.join("blobs"), store.conn_handle()).unwrap();
    (store, cas)
}

#[test]
#[ignore = "requires /dev/fuse — run on the self-hosted fuse runner (fuse-mount-live.yml)"]
fn live_mount_roundtrip_through_kernel() {
    if require_live_fuse().is_err() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let (store, cas) = open_store(dir.path());
    let fs = cairn_fs_linux::for_project_device(store, cas, "p1", "dev-live").unwrap();

    let mp: PathBuf = dir.path().join("mnt");
    std::fs::create_dir(&mp).unwrap();
    use std::os::unix::fs::MetadataExt;
    let parent_dev = std::fs::metadata(dir.path()).unwrap().dev();

    let mount_fs = Arc::clone(&fs);
    let mount_path = mp.clone();
    let mount_thread = std::thread::spawn(move || {
        mount_fs
            .mount(&mount_path)
            .expect("fuser mount2 — check /dev/fuse perms and fusermount3");
    });
    wait_mounted(&mp, parent_dev).expect("mount did not appear within 10s");
    println!("mounted at {}", mp.display());

    // 1) multi-chunk write-back through the kernel: 1.5MB spans many FastCDC chunks
    let payload: Vec<u8> = (0..1_500_000usize)
        .map(|i| ((i.wrapping_mul(2_654_435_761) >> 24) & 0xFF) as u8)
        .collect();
    let f1 = mp.join("take1.bin");
    {
        let mut fh = std::fs::File::create(&f1).expect("create through FUSE");
        fh.write_all(&payload).unwrap();
        fh.sync_all().unwrap();
        // drop = close → flush → release → commit_spool (ingest is synchronous on close)
    }

    // 2) read back through the mount (header cache + verified ranged reads)
    let got = std::fs::read(&f1).expect("read back through FUSE");
    assert_eq!(got.len(), payload.len());
    assert!(
        got == payload,
        "byte-identical roundtrip through the kernel"
    );

    // 3) virtual directories: nested writes resolve synthesized intermediate dirs
    std::fs::create_dir_all(mp.join("sequences/A001")).unwrap();
    std::fs::create_dir_all(mp.join("sequences/B002")).unwrap();
    let f2 = mp.join("sequences/A001/scene.prproj");
    std::fs::write(&f2, b"scene-v1").unwrap();
    assert_eq!(std::fs::read(&f2).unwrap(), b"scene-v1");

    // 4) readdir lists synced files AND synthesized dirs
    let names: Vec<String> = std::fs::read_dir(&mp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "take1.bin"),
        "file listed: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "sequences"),
        "dir listed: {names:?}"
    );

    // 5) ADR-0014 Phase 2 END-TO-END: author the domains config THROUGH the mount
    //    (it is an ordinary synced project file), plant a live foreign pen on the
    //    DOMAIN root, and the next write-open inside that domain must EBUSY through
    //    the actual kernel. Disjoint domain + unscoped file proceed independently.
    std::fs::write(mp.join(".cairn-domains"), "sequences/A001\n").unwrap();

    // a genuinely live foreign process for the pid probe (alive for the whole check)
    let mut foreign_child = Command::new("sleep").arg("60").spawn().unwrap();
    let foreign = foreign_child.id();
    let now = WallClock.now_millis();
    let (store2, _cas2) = open_store(dir.path());
    store2
        .put_lease_pid(
            "sequences/A001",
            4242,
            now + 60_000,
            Some(i64::from(foreign)),
            Some("p1"),
            Some("dev-foreign"),
        )
        .unwrap();

    let blocked = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(mp.join("sequences/A001/scene02.prproj"));
    match blocked {
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::ResourceBusy,
            "foreign domain pen must EBUSY through the kernel: {e}"
        ),
        Ok(_) => panic!("same-domain second pen must fail (EBUSY)"),
    }
    std::fs::write(mp.join("sequences/B002/other.prproj"), b"ok")
        .expect("disjoint domain proceeds");
    std::fs::write(mp.join("audio.bin"), b"ok").expect("unscoped per-file proceeds");
    foreign_child.kill().ok();
    foreign_child.wait().ok(); // reap (clippy::zombie_processes)

    // 6) write-back really landed in the CAS (blobs populated)
    let blob_count = std::fs::read_dir(dir.path().join("blobs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .count();
    assert!(blob_count > 0, "CAS blobs must exist after write-back");

    // 7) unmount; the session loop exits and mount2 returns
    fusermount_unmount(&mp);
    mount_thread.join().expect("mount thread exits on unmount");
    fs.shutdown();

    // 8) after unmount the data lives in the STORE: a fresh view sees the committed
    //    files with real manifest rows (not mount-window ghosts)
    let (store3, _cas3) = open_store(dir.path());
    for path in ["take1.bin", "sequences/A001/scene.prproj", "audio.bin"] {
        let row = store3.get_file("p1", path);
        assert!(
            row.is_some(),
            "committed file {path} must survive in the store"
        );
    }
    let listing = store3.list_files("p1");
    assert!(listing.len() >= 4, "store listing: {} files", listing.len());
    println!(
        "live mount round-trip OK: {} files committed to the store",
        listing.len()
    );
}
