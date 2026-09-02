//! `cairn-fuse` — mount a Cairn project as a Linux filesystem (SPEC §10, ADR-0014).
//!
//! The artifact the workspace ships for Linux NLE workstations: editors open project
//! files through the mount; Cairn serves verified content (I1 header cache, ranged
//! chunk streaming) and commits write-backs with pid-bound leases — conflicts
//! self-heal (crash → pid reaper), native collab paths stand down.
//!
//! Usage:
//! ```text
//! cairn-fuse --store /var/lib/cairn --project p1 --mount /mnt/project [--device dev1]
//! ```
//!
//! Build (pure-Rust fuser — no libfuse headers needed, only runtime /dev/fuse):
//! ```text
//! cargo build -p cairn-fs-linux --features fuse --bin cairn-fuse
//! ```

#[cfg(not(feature = "fuse"))]
fn main() {
    eprintln!(
        "cairn-fuse was built WITHOUT the `fuse` feature — rebuild with:\n  \
         cargo build -p cairn-fs-linux --features fuse --bin cairn-fuse"
    );
    std::process::exit(2);
}

#[cfg(feature = "fuse")]
fn main() {
    let mut store_dir: Option<String> = None;
    let mut project: Option<String> = None;
    let mut mountpoint: Option<String> = None;
    let mut device = "fuse-mount".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--store" => store_dir = args.next(),
            "--project" => project = args.next(),
            "--mount" => mountpoint = args.next(),
            "--device" => device = args.next().unwrap_or(device),
            "--version" | "-V" => {
                println!("cairn-fuse {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    "cairn-fuse — mount a Cairn project as a filesystem\n\n\
                     --store DIR    store root (db.sqlite + blobs/ + staging/)\n\
                     --project ID   project to expose\n\
                     --mount PATH   mountpoint (must exist)\n\
                     --device ID    lease device id (default: fuse-mount)"
                );
                return;
            }
            other => {
                eprintln!("unknown argument: {other} (see --help)");
                std::process::exit(2);
            }
        }
    }
    let (Some(store_dir), Some(project), Some(mountpoint)) = (store_dir, project, mountpoint)
    else {
        eprintln!("required: --store DIR --project ID --mount PATH (see --help)");
        std::process::exit(2);
    };

    // simple stderr logging (tracing subscriber stays opt-in; no extra deps)
    eprintln!(
        "cairn-fuse: project {project} from {store_dir} → {mountpoint} (device {device}; \
         unmount with: fusermount -u {mountpoint})"
    );
    let fs = match run(&store_dir, &project, &device) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("cairn-fuse: {e}");
            std::process::exit(1);
        }
    };
    let heartbeat = cairn_fs_linux::spawn_heartbeat(fs.clone());
    let fs_for_mount = fs.clone();
    if let Err(e) = fs_for_mount.mount(std::path::Path::new(&mountpoint)) {
        eprintln!("cairn-fuse: mount failed: {e}");
        fs.shutdown(); // stop the heartbeat loop before joining it (it terminates within one beat)
        heartbeat.join().ok();
        std::process::exit(1);
    }
    // unmounted: release leases owned by still-open spools, exit clean
    fs.shutdown();
    heartbeat.join().ok();
}

#[cfg(feature = "fuse")]
fn run(
    store_dir: &str,
    project: &str,
    device: &str,
) -> Result<std::sync::Arc<cairn_fs_linux::CairnFs>, cairn_core::CairnError> {
    use cairn_store::{Cas, Store};
    let root = std::path::Path::new(store_dir);
    let store = Store::open(root, std::sync::Arc::new(cairn_core::clock::WallClock))?;
    let conn = store.conn_handle();
    let cas = Cas::open(&root.join("blobs"), conn)?;
    cairn_fs_linux::for_project_device(store, cas, project, device)
}
