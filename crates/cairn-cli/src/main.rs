//! Cairn CLI + local daemon entry point.
//!
//! Daemon architecture (SPEC §11): single process; localhost gRPC ctl on 127.0.0.1:17777
//! (token-authenticated); local diagnostics dashboard on 127.0.0.1:17778 (ADR-0009). The ctl
//! contract is frozen in docs/ctl-api.md — breaking changes are bugs.

#![forbid(unsafe_code)]

mod audit;
mod daemon;
mod dashboard;
mod doctor;
mod handoff;
mod members;
mod projects;
mod proxy;
mod review;
mod search;
mod tlbranch;
mod win_attach;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cairn",
    about = "Cairn — content-addressed sync & storage for video teams",
    version
)]
pub struct Cli {
    /// Data directory (default ~/.cairn)
    #[arg(long, env = "CAIRN_HOME")]
    pub home: Option<String>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Enroll this device (token stored in OS keychain)
    Login {
        /// Server address (host:port)
        #[arg(long)]
        server: String,
        /// Enrollment code from an admin
        #[arg(long)]
        code: String,
        /// Device name
        #[arg(long, default_value = "workstation")]
        name: String,
        /// Dev fallback: store token in a 0600 file instead of the keychain
        #[arg(long, hide = true)]
        allow_plaintext_file: bool,
        /// CA cert (PEM) for TLS servers with self-signed certs
        #[arg(long)]
        ca: Option<String>,
    },
    /// Remove the stored device token (revokes nothing server-side)
    Logout,
    /// First-run setup: create the local store (~/.cairn) and report state
    /// (the device identity itself is issued server-side at `cairn login`)
    Init {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Attach a folder as a project root (scan → chunk → upload → sync loop)
    Attach {
        /// Folder to attach (becomes the project root)
        path: String,
        /// Project id (default: slug of the folder name)
        #[arg(long)]
        project: Option<String>,
        /// Server addr override (host:port; default: the one stored at login)
        #[arg(long)]
        server: Option<String>,
        /// Daemon ctl address (loopback)
        #[arg(long, default_value = "http://127.0.0.1:17777")]
        ctl: String,
    },
    /// Detach a project root (local files are NOT touched)
    Detach {
        /// Project id
        #[arg(long)]
        project: String,
        /// Daemon ctl address (loopback)
        #[arg(long, default_value = "http://127.0.0.1:17777")]
        ctl: String,
    },
    /// List attached projects (live ctl view)
    Projects {
        /// Daemon ctl address (loopback)
        #[arg(long, default_value = "http://127.0.0.1:17777")]
        ctl: String,
    },
    /// Show daemon + project sync status
    Status {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Run one sync pass over all attached roots
    Sync {
        /// Project id (default: all)
        #[arg(long)]
        project: Option<String>,
    },
    /// Snapshot operations
    Snapshot {
        #[command(subcommand)]
        cmd: snapshot::SnapshotCmd,
    },
    /// Pin a path (fetch + local CAS pin; eviction-exempt)
    Pin {
        /// Project id
        #[arg(long)]
        project: String,
        /// Project-relative path
        #[arg(long)]
        path: String,
    },
    /// Unpin a path
    Unpin {
        /// Project id
        #[arg(long)]
        project: String,
        /// Project-relative path
        #[arg(long)]
        path: String,
    },
    /// List active leases for a project
    Lease {
        /// Project id
        #[arg(long)]
        project: String,
    },
    /// Recall archived (cold) content with progress + ETA
    Recall {
        /// Project id
        #[arg(long)]
        project: String,
        /// Optional single path
        #[arg(long)]
        path: Option<String>,
    },
    /// Run the doctor diagnostics suite
    Doctor {
        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// (hidden, dev) Issue an enrollment code against a dev-insecure server
    #[command(hide = true)]
    DevEnrollCode {
        /// Server address (host:port)
        #[arg(long)]
        server: String,
        /// Tenant id
        #[arg(long, default_value = "t1")]
        tenant: String,
        /// Email for the code
        #[arg(long, default_value = "editor@studio.tv")]
        email: String,
    },
    /// (hidden) Count FastCDC chunks for a file (acceptance harness helper)
    #[command(hide = true)]
    ChunkCount { path: String },
    /// GC shadow-mode report (beta gate: must run clean)
    GcShadowReport {
        /// Tenant id
        #[arg(long)]
        tenant: String,
        /// Optional project filter
        #[arg(long)]
        project: Option<String>,
    },
    /// Run the storage server (metadata + data planes)
    Server {
        /// Data dir
        #[arg(long, default_value = "./.cairn-server")]
        data_dir: String,
        /// gRPC address
        #[arg(long, default_value = "127.0.0.1:7443")]
        grpc_addr: String,
        /// Object-store HTTP address (dev backend)
        #[arg(long, default_value = "127.0.0.1:7444")]
        objects_addr: String,
        /// Dev bootstrap: enroll codes without an admin token (DEV ONLY)
        #[arg(long)]
        dev_insecure: bool,
        /// TLS server cert (PEM) for the gRPC endpoint — enables TLS on 7443
        #[arg(long)]
        tls_cert: Option<String>,
        /// TLS server key (PEM)
        #[arg(long)]
        tls_key: Option<String>,
    },
    /// Capture a timeline document (stamp identities + sidecar manifest)
    TlCapture {
        /// Timeline file (.otio JSON or .fcpxml)
        path: String,
        /// Canonicalize IN PLACE (default: write <stem>.canonical.otio + sidecar)
        #[arg(long)]
        in_place: bool,
    },
    /// Three-way timeline merge (ADR-0015): base/ours/theirs -> merged + report
    TlMerge {
        /// Base timeline (the common ancestor, content-addressed)
        #[arg(long)]
        base: String,
        /// Our side (the save under the SURVIVING fence — SPEC §8)
        #[arg(long)]
        ours: String,
        /// Their side (the earlier save, rebased)
        #[arg(long)]
        theirs: String,
        /// Report only: do not write the merged document
        #[arg(long)]
        dry_run: bool,
        /// Explicit output path (default: <ours>.merged.otio)
        #[arg(long)]
        out: Option<String>,
        /// Zero-touch semantic policy (ADR-0023, OPT-IN): frame-disjoint
        /// re-cuts of the same clip auto-merge (C11) instead of escalating
        /// C3 — "ours re-cut the head, theirs re-cut the tail" lands without
        /// asking. Same-edge re-cuts still conflict under every policy.
        /// Without this flag the merge is bit-for-bit the conservative default.
        #[arg(long)]
        semantic: bool,
    },
    /// Round-trip audit (ADR-0018): verify a timeline that traveled through
    /// another NLE kept its clips, durations, effects, markers — the
    /// broken-speed-ramp / dropped-title detector. Exit 1 on any LOSS.
    TlVerify {
        /// The timeline BEFORE the round-trip
        #[arg(long)]
        base: String,
        /// What came back from the other NLE
        #[arg(long)]
        roundtrip: String,
        /// Machine-readable JSON report
        #[arg(long)]
        json: bool,
    },
    /// Timeline branches (ADR-0023): git-for-video, the foolproof cut —
    /// experiment fearlessly, cherry-pick the good parts back, soft-delete
    /// the failures. The working timeline is never mutated by branch ops.
    TlBranch {
        #[command(subcommand)]
        cmd: tlbranch::TlBranchCmd,
    },
    /// Intelligent clip search (ADR-0023): "search by what you see" — files
    /// by name/path tokens + every timeline's clips with their positions
    /// ("worried closeup" finds the clip AND where it was cut in). Offline,
    /// deterministic, no AI.
    Search {
        /// The query (quoted if multi-word)
        query: String,
        /// Project id (uses its attached workspace; requires --home with a
        /// store) OR a raw directory with --path
        #[arg(long)]
        project: Option<String>,
        /// Search a directory directly (no store needed — works on any
        /// machine, great for tests)
        #[arg(long)]
        path: Option<String>,
        /// Max results (default 50)
        #[arg(long, default_value = "50")]
        limit: usize,
        /// JSON output (machines) instead of the human table
        #[arg(long)]
        json: bool,
    },
    /// Review notes (ADR-0018): frame-anchored, three-way mergeable,
    /// CSV-interop with review tools (the scattered-feedback fix)
    Notes {
        #[command(subcommand)]
        cmd: notes::NotesCmd,
    },
    /// Bin-lock a path (ADR-0014 local pen): claim write authority so other
    /// editors see "locked by <device>" — no silent automatic merges
    Lock {
        /// Project id
        #[arg(long)]
        project: String,
        /// Project-relative path (or a directory prefix)
        #[arg(long)]
        path: String,
    },
    /// Release a bin-lock
    Unlock {
        /// Project id
        #[arg(long)]
        project: String,
        /// Project-relative path
        #[arg(long)]
        path: String,
    },
    /// Run the local daemon (ctl gRPC :17777 + dashboard :17778)
    Daemon {
        /// Bind address for ctl gRPC (loopback only)
        #[arg(long, default_value = "127.0.0.1:17777")]
        ctl_addr: String,
        /// Bind address for the local dashboard (loopback only, ADR-0009)
        #[arg(long, default_value = "127.0.0.1:17778")]
        ui_addr: String,
        /// Bind address for the CLIENT REVIEW portal (ADR-0020): the
        /// token-gated web player for guests. OFF unless set — bind
        /// 0.0.0.0:17778-style addresses for LAN/VPN clients. Every route
        /// fails closed without a valid guest link token.
        #[arg(long)]
        review_addr: Option<String>,
        /// Signal server (ADR-0017) — join every project's swarm for
        /// peer-first hydration (LAN-speed blocks, zero cloud egress)
        #[arg(long)]
        swarm_signal: Option<String>,
        /// Join code the swarm host shared with you (ADR-0017 §7). Required
        /// with --swarm-signal: nodes without the code are locked out.
        #[arg(long)]
        swarm_join_code: Option<String>,
        /// Use the well-known dev key instead of a join code (smoke tests
        /// only; pairs with `cairn signal --dev-key`)
        #[arg(long)]
        swarm_dev_key: bool,
        /// Zero-config LAN join (ADR-0019 §4): discover the swarm's signal
        /// server via its mDNS beacon instead of passing --swarm-signal.
        /// Requires --swarm-join-code (the beacon is matched by the code's
        /// fingerprint; the code still gates admission).
        #[arg(long)]
        swarm_mdns: bool,
        /// STUN server for WAN NAT discovery (two studios across the
        /// internet). Persisted in the home store (`swarm/stun`), so it
        /// survives restarts. Default: the public server list, tried in
        /// order (stun.cloudflare.com first).
        #[arg(long)]
        swarm_stun: Option<String>,
        /// Disable STUN discovery entirely (punch via signal-observed
        /// candidates only; relay still works). Persists.
        #[arg(long)]
        swarm_no_stun: bool,
    },
    /// AAF/OMF handoff ledger (ADR-0020 §6): bind exports to picture lock
    Handoff {
        #[command(subcommand)]
        cmd: handoff_cmd::HandoffCmd,
    },
    /// Role-based access control (ADR-0020 §4): members, roles, checks
    Member {
        #[command(subcommand)]
        cmd: member_cmd::MemberCmd,
    },
    /// Proxy workflow (ADR-0020 §3): generate/list lightweight media
    /// proxies for remote editors + the review portal
    Proxy {
        #[command(subcommand)]
        cmd: proxy_cmd::ProxyCmd,
    },
    /// Client review portal (ADR-0020): publish versions, mint guest
    /// links, read frame-accurate comments. The daemon's --review flag
    /// serves the web player to clients.
    Review {
        #[command(subcommand)]
        cmd: review_cmd::ReviewCmd,
    },
    /// Run the P2P signal server + relay (ADR-0017): the lightweight
    /// rendezvous directory nodes register business cards with, plus the
    /// encrypted pass-through relay for punch-proof firewalls. Never stores
    /// or reads media blocks.
    ///
    /// The swarm is join-code gated: a fresh code is generated and printed
    /// unless --join-code pins one — share it only with people who may join
    /// (everyone else is dropped silently, ADR-0017 §7).
    Signal {
        /// UDP bind for the signal directory
        #[arg(long, default_value = "0.0.0.0:17780")]
        bind: String,
        /// UDP bind for the relay
        #[arg(long, default_value = "0.0.0.0:17781")]
        relay_bind: String,
        /// Host with THIS join code (validated; peers must present the same
        /// code). Default: generate a fresh code and print it.
        #[arg(long)]
        join_code: Option<String>,
        /// Use the well-known dev key instead of a join code (smoke tests
        /// only; pairs with `cairn daemon --swarm-dev-key`)
        #[arg(long)]
        dev_key: bool,
        /// Disable the LAN mDNS beacon (ADR-0019 §4). By default, when a join
        /// code is active and the bind is wildcard, the signal server
        /// announces a fingerprint beacon so LAN joiners need ONLY the code.
        #[arg(long)]
        no_mdns: bool,
    },
}

pub mod handoff_cmd {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum HandoffCmd {
        /// Record an AAF/OMF export against the cut it came from (the
        /// picture-lock binding for the sound team)
        Record {
            #[arg(long, default_value = ".")]
            root: String,
            /// Exported AAF/OMF file, RELATIVE to the root
            #[arg(long)]
            file: String,
            /// The timeline (OTIO/FCPXML) the export was cut from
            #[arg(long)]
            timeline: Option<String>,
            /// Snapshot/commit hash the export was made from
            #[arg(long)]
            snapshot: Option<String>,
            #[arg(long, default_value = "")]
            note: String,
            #[arg(long, default_value = "editor")]
            by: String,
        },
        /// Verify every handoff: file digest + picture-lock fingerprint
        Verify {
            #[arg(long, default_value = ".")]
            root: String,
            /// One file only (default: all)
            #[arg(long)]
            file: Option<String>,
            /// Current timeline to check the picture-lock binding
            #[arg(long)]
            timeline: Option<String>,
        },
        /// List recorded handoffs
        List {
            #[arg(long, default_value = ".")]
            root: String,
        },
    }
}

pub mod member_cmd {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum MemberCmd {
        /// Add or change a member's role (owner action; syncs with the
        /// project via .cairn/members.json)
        Add {
            #[arg(long, default_value = ".")]
            root: String,
            /// Member device id
            #[arg(long)]
            device: String,
            /// Display name
            #[arg(long, default_value = "")]
            name: String,
            /// owner | lead-editor | editor | assistant | colorist |
            /// sound-designer | reviewer
            #[arg(long, default_value = "editor")]
            role: String,
            /// Acting device id (default $CAIRN_DEVICE or 'local')
            #[arg(long)]
            as_device: Option<String>,
        },
        /// Remove a member (owner action)
        Remove {
            #[arg(long, default_value = ".")]
            root: String,
            #[arg(long)]
            device: String,
            #[arg(long)]
            as_device: Option<String>,
        },
        /// List members + the implicit default role
        List {
            #[arg(long, default_value = ".")]
            root: String,
        },
        /// Does DEVICE hold PERM? (exit 1 = deny; for scripts/hooks)
        Check {
            #[arg(long, default_value = ".")]
            root: String,
            #[arg(long)]
            device: String,
            /// read | write-files | organize-bins | lock-file |
            /// lock-timeline | edit-timeline | color-grade | mix-audio |
            /// comment | manage-review | manage-members | verify |
            /// snapshot | restore
            #[arg(long)]
            perm: String,
        },
    }
}

pub mod proxy_cmd {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum ProxyCmd {
        /// Generate (or reuse) a lightweight proxy for one media file —
        /// remote editors and the review portal stream this instead of
        /// the full-res original
        Generate {
            #[arg(long, default_value = ".")]
            root: String,
            /// Media file, RELATIVE to the root
            #[arg(long)]
            media: String,
            /// Long-edge pixel cap (no upscaling)
            #[arg(long, default_value_t = 1080)]
            max_height: u32,
            /// H.264 CRF quality (lower = better)
            #[arg(long, default_value_t = 23)]
            crf: u32,
            /// Byte-copy transcoder (pipeline smoke tests ONLY — a copy is
            /// not a proxy)
            #[arg(long)]
            copy: bool,
        },
        /// List every indexed proxy with its status
        List {
            #[arg(long, default_value = ".")]
            root: String,
        },
        /// One media file's proxy state
        Status {
            #[arg(long, default_value = ".")]
            root: String,
            #[arg(long)]
            media: String,
        },
    }
}

pub mod review_cmd {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum ReviewCmd {
        /// Publish a new version to the review stack (append-only; guests
        /// always land on the newest)
        Publish {
            /// Project root (default: current directory)
            #[arg(long, default_value = ".")]
            root: String,
            /// Session title (sets it on first publish)
            #[arg(long)]
            title: Option<String>,
            /// Watchable media file, RELATIVE to the root
            #[arg(long)]
            media: String,
            /// Lightweight proxy, RELATIVE to the root (served to guests
            /// first). When omitted, one is GENERATED via the proxy
            /// pipeline if ffmpeg is available; --no-proxy skips that.
            #[arg(long)]
            proxy: Option<String>,
            /// Skip automatic proxy generation
            #[arg(long)]
            no_proxy: bool,
            /// Frame rate: "24", "25", "23.976", or an exact rational
            /// "24000/1001". Default: probed from the media (ffprobe);
            /// 24 when probing is impossible.
            #[arg(long)]
            fps: Option<String>,
            /// Total frames of the cut. Default: probed from the media —
            /// required when ffprobe cannot read it.
            #[arg(long)]
            frames: Option<u64>,
            /// Version label shown to clients
            #[arg(long, default_value = "")]
            label: String,
            /// Timeline file (OTIO/FCPXML) — binds this version to the
            /// timeline's content fingerprint (the AAF/OMF handoff
            /// verifies against it)
            #[arg(long)]
            timeline: Option<String>,
            /// Snapshot/commit hash this version was published from
            #[arg(long)]
            snapshot: Option<String>,
            /// Publisher identity recorded on the version
            #[arg(long, default_value = "editor")]
            by: String,
        },
        /// Mint a guest link (no account — the token is the identity)
        Link {
            #[arg(long, default_value = ".")]
            root: String,
            /// guest role: commenter (default) or viewer (view-only)
            #[arg(long, default_value = "commenter")]
            role: String,
            /// who this link is for (display only)
            #[arg(long, default_value = "")]
            note: String,
            /// link lifetime in hours (0 = no expiry)
            #[arg(long, default_value = "72")]
            ttl_hours: i64,
            /// restrict the link to the newest version only
            #[arg(long)]
            latest_only: bool,
        },
        /// List the version stack, links, and note counts
        List {
            #[arg(long, default_value = ".")]
            root: String,
        },
        /// List frame-anchored comments with timecodes
        Comments {
            #[arg(long, default_value = ".")]
            root: String,
            /// one version (default: all)
            #[arg(long)]
            version: Option<u32>,
        },
        /// Export a version's comments as NLE markers (FCP7 XML by
        /// default; --otio for the canonical timeline with markers)
        ExportMarkers {
            #[arg(long, default_value = ".")]
            root: String,
            #[arg(long)]
            version: u32,
            /// Output file (markers.xml or markers.otio)
            #[arg(long)]
            out: String,
            /// Export OTIO with markers instead of FCP7 XML
            #[arg(long)]
            otio: bool,
            /// Base timeline for OTIO export (default: marker-only shell)
            #[arg(long)]
            timeline: Option<String>,
        },
        /// Export a version's comments as an EDIT CHANGE LIST (the 3-step
        /// no-AI recipe, ADR-0023 §3): mechanical notes (cut/trim/delete/
        /// replace/gain) become structured ops; creative notes ride along
        /// highlighted for the human. Formats: json (the authoritative,
        /// applyable form), edl (CMX3600), fcpxml (FCP7 markers).
        ExportChangelist {
            #[arg(long, default_value = ".")]
            root: String,
            #[arg(long)]
            version: u32,
            /// Output file
            #[arg(long)]
            out: String,
            /// json (default) | edl | fcpxml
            #[arg(long, default_value = "json")]
            format: String,
        },
        /// Apply a JSON changelist to a timeline — the editor's YES/NO gate:
        /// PREVIEW by default (exit 1, nothing written); --yes writes
        /// <timeline>.changelist.otio (never in-place). Every op reports
        /// Applied or the honest Unresolved reason.
        ApplyChangelist {
            /// The timeline to edit
            #[arg(long)]
            timeline: String,
            /// Changelist JSON from `export-changelist`
            #[arg(long)]
            changelist: String,
            /// Explicit output path (default: <timeline>.changelist.otio)
            #[arg(long)]
            out: Option<String>,
            /// Actually write the result. Without this flag: preview only.
            #[arg(long)]
            yes: bool,
        },
        /// Mark a comment resolved (or reopened with --status OPEN)
        Resolve {
            #[arg(long, default_value = ".")]
            root: String,
            #[arg(long)]
            version: u32,
            /// comment id (`cairn review comments` shows them via
            /// --json in a follow-up; use the player for now)
            #[arg(long)]
            id: String,
            /// RESOLVED (default), OPEN, or REJECTED
            #[arg(long, default_value = "RESOLVED")]
            status: String,
        },
    }
}

pub mod notes {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum NotesCmd {
        /// Import a review-tool CSV (Frame.io export etc.) into a notes file
        Import {
            /// CSV file to import
            csv: String,
            /// Output .notes.json (default: <csv stem>.notes.json)
            #[arg(long)]
            out: Option<String>,
            /// Author for rows with no author column
            #[arg(long, default_value = "unknown")]
            author: String,
            /// Default frame rate when the CSV has none
            #[arg(long, default_value = "24")]
            rate: i64,
        },
        /// List the notes in a .notes.json file
        List {
            /// Notes file
            file: String,
        },
        /// Export a notes file to CSV (for review tools / spreadsheets)
        Export {
            /// Notes file
            file: String,
            /// Output CSV (default: stdout)
            #[arg(long)]
            out: Option<String>,
        },
        /// Three-way merge notes files (deterministic; conflicts reported)
        Merge {
            /// Base notes file (common ancestor)
            #[arg(long)]
            base: String,
            /// Our side
            #[arg(long)]
            ours: String,
            /// Their side
            #[arg(long)]
            theirs: String,
            /// Output merged notes file (default: <ours>.merged.notes.json)
            #[arg(long)]
            out: Option<String>,
        },
    }
}

pub mod snapshot {
    use clap::Subcommand;

    #[derive(Subcommand)]
    pub enum SnapshotCmd {
        /// Create a snapshot (fold trigger, on demand — SPEC §7.2)
        Create {
            #[arg(long)]
            project: String,
            #[arg(long, default_value = "")]
            label: String,
        },
        /// List snapshots for a project
        List {
            #[arg(long)]
            project: String,
        },
        /// Restore a snapshot to a target path
        Restore {
            #[arg(long)]
            project: String,
            #[arg(long)]
            commit: String,
            #[arg(long)]
            target: Option<String>,
        },
    }
}

fn main() -> anyhow::Result<()> {
    // Global allocator (ADR-0026): mimalloc — per-thread heaps, 8B header,
    // 12-week fragmentation 20.7% vs the system allocator's silent bleed on
    // a long-lived daemon (johal.in/internals-rust-185-memory-allocator:
    // 42ns/128B alloc, p99 89ns — 38% under the system allocator).
    #[global_allocator]
    static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

    // Round 20 Windows fix: even `--version` blew the 1 MiB main thread on a
    // real Windows host — clap's parse of this command tree in a debug
    // build plus the giant `run()` async state machine park far more stack
    // than Linux's 8 MiB default (which is why it was never seen there).
    // The whole boot sequence — provider init, tracing, CLI parse, the
    // runtime, run() — moves onto a thread with a guaranteed 16 MiB stack;
    // the platform's smallest thread no longer decides whether the CLI
    // boots. (Found by the new round20-windows CI leg: tl-capture on a
    // real windows-latest host overflowed where ubuntu never did.)
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| -> anyhow::Result<()> {
            // tonic's TLS pulls rustls with both providers feature-unified; pick
            // ring explicitly (workspace-standard, THIRD_PARTY.md) before any
            // TLS-capable code runs
            let _ = rustls::crypto::ring::default_provider().install_default();
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .try_init()
                .ok();
            let cli = Cli::parse();
            let home = std::path::PathBuf::from(cli.home.clone().unwrap_or_else(default_home));
            // Thread budget (ADR-0025, PostHog pattern): the rayon CPU lane is
            // installed at full core width, tokio I/O workers at half (the
            // runtime carries proxy/presence/dashboard I/O; hashing is the
            // throughput side). Both pools at full width = the oversubscription
            // that produced PostHog's 2.5s p99 spikes; the ingest semaphore
            // keeps the sum honest under dirty-file bursts.
            cairn_sync::offload::init_cpu_lanes();
            let (io_workers, _cpu_lanes) = cairn_sync::offload::thread_budget();
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(io_workers)
                .enable_all()
                .build()?
                .block_on(run(cli, home))
        })
        .expect("spawn cli thread")
        .join()
        .map_err(|_| anyhow::anyhow!("cli thread panicked"))?
}

fn default_home() -> String {
    dirs::home_dir()
        .map(|h| h.join(".cairn").to_string_lossy().into_owned())
        .unwrap_or_else(|| ".cairn".into())
}

async fn run(cli: Cli, home: std::path::PathBuf) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Server {
            data_dir,
            grpc_addr,
            objects_addr,
            dev_insecure,
            tls_cert,
            tls_key,
        } => {
            if dev_insecure {
                tracing::warn!("DEV-INSECURE mode: enrollment codes issued without admin auth");
            }
            cairn_server::run::run(cairn_server::run::ServerConfig {
                data_dir: data_dir.into(),
                grpc_addr,
                objects_addr,
                dev_insecure,
                tls_cert: tls_cert.map(std::path::PathBuf::from),
                tls_key: tls_key.map(std::path::PathBuf::from),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
        }
        Cmd::Daemon {
            ctl_addr,
            ui_addr,
            review_addr,
            swarm_signal,
            swarm_join_code,
            swarm_dev_key,
            swarm_mdns,
            swarm_stun,
            swarm_no_stun,
        } => {
            // WAN NAT discovery override persists in the home store (the
            // swarm join reads it later; durable like swarm/signal)
            if swarm_no_stun {
                if swarm_stun.is_some() {
                    return Err(anyhow::anyhow!(
                        "--swarm-stun and --swarm-no-stun are mutually exclusive"
                    ));
                }
                if let Ok(store) = cairn_store::Store::open(
                    &home,
                    std::sync::Arc::new(cairn_core::clock::WallClock),
                ) {
                    let _ = store.meta_set("swarm/stun", "off");
                }
                tracing::info!("stun discovery disabled (persisted; swarm/stun=off)");
            } else if let Some(stun) = swarm_stun {
                // validate early — a typo should fail the flag, not the swarm
                if !(stun.contains(':') || stun.contains(',')) {
                    return Err(anyhow::anyhow!(
                        "--swarm-stun expects host:port (got {stun})"
                    ));
                }
                if let Ok(store) = cairn_store::Store::open(
                    &home,
                    std::sync::Arc::new(cairn_core::clock::WallClock),
                ) {
                    let _ = store.meta_set("swarm/stun", &stun);
                }
                tracing::info!(stun = %stun, "stun server pinned (persisted)");
            }
            let swarm_signal = if swarm_mdns {
                // zero-config LAN join: browse for the beacon that matches
                // the join code's fingerprint (ADR-0019 §4)
                let code = swarm_join_code.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "--swarm-mdns requires --swarm-join-code — the beacon is \
                         matched by the code's fingerprint"
                    )
                })?;
                if swarm_signal.is_some() {
                    return Err(anyhow::anyhow!(
                        "--swarm-mdns and --swarm-signal are mutually exclusive \
                         (discovery fills in the address)"
                    ));
                }
                let code = cairn_p2p::JoinCode::parse(code)
                    .map_err(|e| anyhow::anyhow!("--swarm-join-code: {e}"))?;
                let fp = cairn_p2p::mdns::code_fingerprint(&code.display());
                let tx = std::sync::Arc::new(
                    cairn_p2p::mdns::UdpMdns::bind()
                        .map_err(|e| anyhow::anyhow!("mDNS socket: {e} (LAN multicast unavailable; pass --swarm-signal explicitly)"))?,
                );
                let found =
                    cairn_p2p::mdns::browse(tx, &fp, std::time::Duration::from_millis(1500)).await;
                let Some(beacon) = found.first() else {
                    return Err(anyhow::anyhow!(
                        "no mDNS beacon on this LAN matches the join code's fingerprint \
                         within 1.5s — pass --swarm-signal <host:port> explicitly"
                    ));
                };
                tracing::info!(signal = %beacon.signal_addr, "mDNS: discovered swarm signal");
                Some(beacon.signal_addr.to_string())
            } else {
                swarm_signal
            };
            let swarm = match swarm_signal {
                None => None,
                Some(signal) => {
                    if swarm_dev_key {
                        if swarm_join_code.is_some() {
                            return Err(anyhow::anyhow!(
                                "--swarm-dev-key and --swarm-join-code are mutually exclusive"
                            ));
                        }
                        tracing::warn!(
                            "--swarm-dev-key: using the well-known DEV key (smoke tests ONLY)"
                        );
                        Some(daemon::SwarmOpts {
                            signal,
                            join_code: None,
                        })
                    } else {
                        let code = match swarm_join_code.as_deref() {
                            Some(c) => cairn_p2p::JoinCode::parse(c)
                                .map_err(|e| anyhow::anyhow!("--swarm-join-code: {e}"))?,
                            None => {
                                return Err(anyhow::anyhow!(
                                    "--swarm-signal requires --swarm-join-code — the code the \
                                     swarm host shared with you (or --swarm-dev-key for smoke tests)"
                                ))
                            }
                        };
                        Some(daemon::SwarmOpts {
                            signal,
                            join_code: Some(code),
                        })
                    }
                }
            };
            daemon::run(home, ctl_addr, ui_addr, review_addr, swarm).await
        }
        Cmd::Signal {
            bind,
            relay_bind,
            join_code,
            dev_key,
            no_mdns,
        } => run_signal_server(&bind, &relay_bind, join_code, dev_key, no_mdns).await,
        Cmd::Handoff { cmd } => run_handoff(cmd),
        Cmd::Member { cmd } => run_member(cmd),
        Cmd::Proxy { cmd } => run_proxy(cmd),
        Cmd::Review { cmd } => run_review(cmd),
        Cmd::TlBranch { cmd } => tlbranch::run(cmd),
        Cmd::Search {
            query,
            project,
            path,
            limit,
            json,
        } => run_search(&query, project.as_deref(), path.as_deref(), limit, json),
        Cmd::TlCapture { path, in_place } => run_tl_capture(&path, in_place),
        Cmd::TlMerge {
            base,
            ours,
            theirs,
            dry_run,
            out,
            semantic,
        } => run_tl_merge(&base, &ours, &theirs, dry_run, out.as_deref(), semantic),
        Cmd::TlVerify {
            base,
            roundtrip,
            json,
        } => run_tl_verify(&base, &roundtrip, json),
        Cmd::Notes { cmd } => run_notes(cmd),
        Cmd::Lock { project, path } => run_lock(&home, &project, &path),
        Cmd::Unlock { project, path } => run_unlock(&home, &project, &path),
        Cmd::Status { json } => {
            // live daemon view first (projects + files_synced); doctor fallback offline.
            // ctl endpoint comes from the home store (daemon persists it at boot), so
            // multi-daemon machines poll THEIR daemon, not a hardcoded port.
            let ctl =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))
                    .ok()
                    .and_then(|s| s.meta_get("ctl/addr"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "http://127.0.0.1:17777".into());
            if let Ok(mut c) =
                cairn_proto::pb::ctl_status_client::CtlStatusClient::connect(ctl).await
            {
                if let Ok(out) = c.status(cairn_proto::pb::StatusRequest {}).await {
                    let s = out.into_inner();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "version": s.version,
                                "server_reachable": s.server_reachable,
                                "projects": s.projects.iter().map(|p| serde_json::json!({
                                    "project_id": p.project_id,
                                    "root_path": p.root_path,
                                    "state": p.state,
                                    "files_synced": p.files_synced,
                                    "cursor": p.cursor,
                                    "pending_outbox": p.pending_outbox,
                                    "last_error": p.last_error,
                                })).collect::<Vec<_>>(),
                            }))?
                        );
                    } else {
                        println!("daemon {}", s.version);
                        println!("server {}", s.server_reachable);
                        for p in &s.projects {
                            println!(
                                "   {:<24} {:<10} files={:<6} cursor={:<8} outbox={:<4} {}",
                                p.project_id,
                                p.state,
                                p.files_synced,
                                p.cursor,
                                p.pending_outbox,
                                p.root_path
                            );
                        }
                        if s.projects.is_empty() {
                            println!("   (no attached projects — `cairn attach <path>`)");
                        }
                    }
                    return Ok(());
                }
            }
            let report = doctor::collect(&home);
            if json {
                println!("{}", serde_json::to_string_pretty(&report.checks.iter()
                    .map(|c| serde_json::json!({"name": c.name, "ok": c.ok, "detail": c.detail}))
                    .collect::<Vec<_>>())?);
            } else {
                for c in &report.checks {
                    println!(
                        "{:3} {:<28} {}",
                        if c.ok { "ok" } else { "!!" },
                        c.name,
                        c.detail
                    );
                }
            }
            Ok(())
        }
        Cmd::Doctor { json } => {
            let report = doctor::collect(&home);
            report.print(json);
            std::process::exit(i32::from(!report.healthy()));
        }
        Cmd::DevEnrollCode {
            server,
            tenant,
            email,
        } => {
            let mut auth =
                cairn_proto::pb::auth_client::AuthClient::connect(format!("http://{server}"))
                    .await
                    .map_err(|e| anyhow::anyhow!("cannot reach server {server}: {e}"))?;
            let out = auth
                .enroll_code(cairn_proto::pb::EnrollCodeRequest {
                    tenant_id: tenant,
                    email,
                    scopes: "sync".into(),
                })
                .await?
                .into_inner();
            println!("{}", out.code);
            Ok(())
        }
        Cmd::ChunkCount { path } => {
            let bytes = std::fs::read(&path)?;
            let sh = cairn_core::chunker::StreamHash::compute(&bytes);
            println!("{}", sh.chunk_hashes.len());
            Ok(())
        }
        // Commands that require the sync engine / server land with M2–M5.
        Cmd::Login {
            server,
            code,
            name,
            allow_plaintext_file,
            ca,
        } => {
            let ca_pem = match &ca {
                Some(p) => Some(
                    std::fs::read_to_string(p)
                        .map_err(|e| anyhow::anyhow!("cannot read CA pem {p}: {e}"))?,
                ),
                None => None,
            };
            daemon::login_full(&home, &server, &code, &name, allow_plaintext_file, ca_pem).await
        }
        Cmd::Logout => {
            daemon::logout(&home);
            Ok(())
        }
        Cmd::Init { json } => {
            let store =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))?;
            let enrolled = projects::load_identity(&store).is_some();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "home": home.to_string_lossy(),
                        "store": "ok",
                        "enrolled": enrolled,
                    }))?
                );
            } else {
                println!("cairn home: {}", home.display());
                if enrolled {
                    println!("store ready — device enrolled on this machine");
                } else {
                    println!(
                        "store ready — not enrolled yet (device id is issued at `cairn login`)"
                    );
                }
                println!("next: `cairn daemon` then `cairn attach <folder>` (docs/BETA.md)");
            }
            Ok(())
        }
        Cmd::Attach {
            path,
            project,
            server,
            ctl,
        } => ctl_attach(&ctl, &path, project.as_deref(), server.as_deref()).await,
        Cmd::Detach { project, ctl } => {
            let mut c = cairn_proto::pb::ctl_projects_client::CtlProjectsClient::connect(ctl)
                .await
                .map_err(|e| anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}"))?;
            c.detach_root(cairn_proto::pb::DetachRootRequest {
                project_id: project.clone(),
            })
            .await?;
            println!("detached {project}");
            Ok(())
        }
        Cmd::Projects { ctl } => {
            let mut c = cairn_proto::pb::ctl_projects_client::CtlProjectsClient::connect(ctl)
                .await
                .map_err(|e| anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}"))?;
            let out = c
                .list_projects(cairn_proto::pb::ListProjectsCtlRequest {})
                .await?
                .into_inner();
            for p in out.projects {
                println!("{:<24} {:<10} {}", p.project_id, p.state, p.root_path);
            }
            Ok(())
        }
        Cmd::Sync { .. } => {
            anyhow::bail!("this command needs a running daemon: `cairn daemon` (wired through ctl gRPC; see docs/ctl-api.md)")
        }
        // ---- WO6-3: every ctl command now drives the REAL ctl RPCs ----
        Cmd::Snapshot { cmd } => match cmd {
            snapshot::SnapshotCmd::Create { project, label } => {
                let mut c = cairn_proto::pb::ctl_snapshots_client::CtlSnapshotsClient::connect(
                    ctl_addr(&home),
                )
                .await
                .map_err(daemon_down)?;
                let out = c
                    .create_snapshot(cairn_proto::pb::CreateSnapshotRequest {
                        project_id: project.clone(),
                        label: label.clone(),
                    })
                    .await?
                    .into_inner();
                println!("snapshot created: {} (project {project})", out.commit_hash);
                println!("note: label is recorded in the next server fold (additive field pending, docs/ctl-api.md)");
                Ok(())
            }
            snapshot::SnapshotCmd::List { project } => {
                let mut c = cairn_proto::pb::ctl_snapshots_client::CtlSnapshotsClient::connect(
                    ctl_addr(&home),
                )
                .await
                .map_err(daemon_down)?;
                let out = c
                    .list_snapshots(cairn_proto::pb::ListSnapshotsRequest {
                        project_id: project.clone(),
                    })
                    .await?
                    .into_inner();
                if out.snapshots.is_empty() {
                    println!("no snapshots yet for {project} — create one with `cairn snapshot create --project {project}`");
                }
                for s in out.snapshots {
                    println!(
                        "{}  seq={}  author={}  label={}",
                        s.commit_hash,
                        s.snapshot_seq,
                        if s.author.is_empty() { "-" } else { &s.author },
                        if s.label.is_empty() { "-" } else { &s.label }
                    );
                }
                Ok(())
            }
            snapshot::SnapshotCmd::Restore {
                project,
                commit,
                target,
            } => {
                let mut c = cairn_proto::pb::ctl_snapshots_client::CtlSnapshotsClient::connect(
                    ctl_addr(&home),
                )
                .await
                .map_err(daemon_down)?;
                let out = c
                    .restore_snapshot(cairn_proto::pb::RestoreSnapshotRequest {
                        project_id: project.clone(),
                        commit_hash: commit.clone(),
                        target_path: target.clone().unwrap_or_default(),
                    })
                    .await?
                    .into_inner();
                println!(
                    "restored {} files ({} bytes) from {commit} into {}",
                    out.restored_files,
                    out.bytes,
                    target.clone().unwrap_or_else(|| "the workspace".into())
                );
                Ok(())
            }
        },
        Cmd::Pin { project, path } => {
            let mut c = cairn_proto::pb::ctl_pins_client::CtlPinsClient::connect(ctl_addr(&home))
                .await
                .map_err(daemon_down)?;
            c.pin(cairn_proto::pb::PinRequest {
                project_id: project.clone(),
                path: path.clone(),
            })
            .await?;
            println!("pinned {path} (chunks recalled + eviction-exempt)");
            Ok(())
        }
        Cmd::Unpin { project, path } => {
            let mut c = cairn_proto::pb::ctl_pins_client::CtlPinsClient::connect(ctl_addr(&home))
                .await
                .map_err(daemon_down)?;
            c.unpin(cairn_proto::pb::UnpinRequest {
                project_id: project.clone(),
                path: path.clone(),
            })
            .await?;
            println!("unpinned {path} (evictable again)");
            Ok(())
        }
        Cmd::Lease { project } => {
            // leases are server state; surface via the server's ListLeases through
            // the daemon's server channel — v1 shows local leases (leases_local)
            let store =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))?;
            let rows = store.list_leases();
            let mine: Vec<_> = rows
                .iter()
                .filter(|l| project.is_empty() || l.0.contains(&project))
                .collect();
            if mine.is_empty() {
                println!("no active local leases");
            }
            // Round 20 fix: the filter built `mine` but the print loop
            // iterated ALL rows — a --project filter that matched nothing
            // still printed everything
            for (path, token, expires_at) in mine {
                println!("{path}  token={token}  expires_at={expires_at}");
            }
            Ok(())
        }
        Cmd::Recall { project, path } => {
            let mut c =
                cairn_proto::pb::ctl_recall_client::CtlRecallClient::connect(ctl_addr(&home))
                    .await
                    .map_err(daemon_down)?;
            let job = c
                .start_recall(cairn_proto::pb::StartRecallRequest {
                    project_id: project.clone(),
                    path: path.clone().unwrap_or_default(),
                })
                .await?
                .into_inner();
            println!("recall job {} started", job.job_id);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let st = c
                    .recall_status(cairn_proto::pb::RecallStatusRequest {
                        job_id: job.job_id.clone(),
                    })
                    .await?
                    .into_inner();
                println!(
                    "  state={} progress={:.0}% bytes_done={} bytes_total={}",
                    st.state,
                    st.progress * 100.0,
                    st.bytes_done,
                    st.bytes_total
                );
                if st.state == "completed" || st.state == "failed" {
                    break;
                }
            }
            Ok(())
        }
        Cmd::GcShadowReport { .. } => {
            anyhow::bail!("gc-shadow report runs against the storage server (server-side RPC; ADR'd in docs/ctl-api.md — not silently missing)")
        }
    }
}

fn default_ctl() -> String {
    "http://127.0.0.1:17777".to_string()
}

/// Resolve the ctl endpoint from the HOME store's durable `ctl/addr`
/// (written by every daemon start) — multi-daemon machines (several ctl
/// ports) used to break snapshot/pin/recall/status because those commands
/// hardcoded 127.0.0.1:17777. Falls back to the default on any error.
fn ctl_addr(home: &std::path::Path) -> String {
    cairn_store::Store::open(home, std::sync::Arc::new(cairn_core::clock::WallClock))
        .ok()
        .and_then(|s| s.meta_get("ctl/addr"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_ctl)
}

fn daemon_down(e: tonic::transport::Error) -> anyhow::Error {
    anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}")
}

async fn ctl_attach(
    ctl: &str,
    path: &str,
    project: Option<&str>,
    server: Option<&str>,
) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path)
        .map_err(|e| anyhow::anyhow!("cannot open {path}: {e}"))?
        .to_string_lossy()
        .into_owned();
    let mut c = cairn_proto::pb::ctl_projects_client::CtlProjectsClient::connect(ctl.to_string())
        .await
        .map_err(|e| anyhow::anyhow!("daemon not reachable (run `cairn daemon`): {e}"))?;
    let out = c
        .attach_root(cairn_proto::pb::AttachRootRequest {
            root_path: root,
            server_addr: server.unwrap_or("").to_string(),
            project_id: project.unwrap_or("").to_string(),
        })
        .await?
        .into_inner();
    println!("attached {} as project `{}`", path, out.project_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// ADR-0015 timeline capture + merge (cairn-tl): pure library, CLI edges only.
// Exit-code contract (tl-merge): 0 clean · 1 merged-with-notes · 2 conflicts
// (human escalation — merged file still written, conflicting edits withheld)
// · 3 refused (C10 — no output touched).
// ---------------------------------------------------------------------------

/// How this signal server authenticates swarm members (ADR-0017 §7).
enum Admission {
    /// A join code: generated fresh or pinned via `--join-code`.
    JoinCode(cairn_p2p::JoinCode),
    /// The well-known dev key (smoke tests only — pairs with
    /// `cairn daemon --swarm-dev-key`).
    DevKey,
}

impl Admission {
    fn cluster_key(&self) -> Vec<u8> {
        match self {
            Admission::JoinCode(code) => code.cluster_key().to_vec(),
            Admission::DevKey => b"cairn-dev-swarm-key".to_vec(),
        }
    }
}

/// Run the signal server + relay until Ctrl-C (ADR-0017 §2/§5/§7).
async fn run_signal_server(
    bind: &str,
    relay_bind: &str,
    join_code: Option<String>,
    dev_key: bool,
    no_mdns: bool,
) -> anyhow::Result<()> {
    if dev_key && join_code.is_some() {
        return Err(anyhow::anyhow!(
            "--dev-key and --join-code are mutually exclusive"
        ));
    }
    let admission = if dev_key {
        tracing::warn!("--dev-key: well-known DEV cluster key (smoke tests ONLY)");
        Admission::DevKey
    } else {
        match join_code {
            // explicit code: validated HERE, so a typo fails with the
            // checksum message instead of a runtime mystery
            Some(s) => Admission::JoinCode(
                cairn_p2p::JoinCode::parse(&s).map_err(|e| anyhow::anyhow!("--join-code: {e}"))?,
            ),
            // default: create your own join code and share it — this IS the
            // hosting flow ("get others to join")
            None => Admission::JoinCode(cairn_p2p::JoinCode::generate()),
        }
    };
    let key = admission.cluster_key();
    let bind_addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("bad --bind {bind}: {e}"))?;
    let relay_addr: std::net::SocketAddr = relay_bind
        .parse()
        .map_err(|e| anyhow::anyhow!("bad --relay-bind {relay_bind}: {e}"))?;

    let signal = cairn_p2p::signal::SignalServer::spawn(bind_addr, &key).await?;
    let relay = cairn_p2p::relay::RelayServer::spawn(relay_addr, signal.local_addr, &key).await?;
    // LAN beacon (ADR-0019 §4): advertise a join-code FINGERPRINT so LAN
    // joiners can find this signal server with just the code (--swarm-mdns
    // on their daemon). Non-fatal: multicast-less environments simply skip
    // discovery and use explicit --swarm-signal.
    let mut mdns_task = None;
    let mut mdns_shutdown: Option<tokio::sync::watch::Sender<bool>> = None;
    if !no_mdns {
        if let Admission::JoinCode(code) = &admission {
            if bind_addr.ip().is_unspecified() {
                match cairn_p2p::mdns::UdpMdns::bind() {
                    Ok(tx) => {
                        let fp = cairn_p2p::mdns::code_fingerprint(&code.display());
                        let (tx_alive, rx_alive) = tokio::sync::watch::channel(false);
                        mdns_shutdown = Some(tx_alive);
                        mdns_task = Some(tokio::spawn(cairn_p2p::mdns::spawn_announcer(
                            std::sync::Arc::new(tx),
                            fp,
                            signal.local_addr.port(),
                            rx_alive,
                        )));
                    }
                    Err(e) => {
                        tracing::warn!("mDNS beacon unavailable ({e}); LAN discovery off");
                    }
                }
            }
        }
    }
    println!(
        "signal: {} (rendezvous directory, join-code gated)",
        signal.local_addr
    );
    println!(
        "relay:  {} (encrypted pass-through fallback)",
        relay.local_addr
    );
    let host = signal
        .local_addr
        .ip()
        .to_string()
        .trim_start_matches("0.0.0.0")
        .to_string();
    match &admission {
        Admission::JoinCode(code) => {
            println!();
            println!("  join code (share ONLY with people who may join this swarm):");
            println!();
            println!("  {}", code.display());
            println!();
            println!(
                "  point editors at:  cairn daemon --swarm-signal {host}:{} --swarm-join-code <code>",
                signal.local_addr.port()
            );
            println!();
            println!("  nodes without the code are dropped silently; rotating the code");
            println!("  (restart with --join-code <new>) locks out everyone who held the old one.");
        }
        Admission::DevKey => {
            println!(
                "point editors at:  cairn daemon --swarm-signal {host}:{} --swarm-dev-key",
                signal.local_addr.port()
            );
        }
    }
    tokio::signal::ctrl_c().await?;
    if let Some(tx) = mdns_shutdown.take() {
        let _ = tx.send(true);
    }
    if let Some(t) = mdns_task.take() {
        t.abort();
    }
    relay.task.abort();
    signal.task.abort();
    Ok(())
}

fn run_tl_capture(path: &str, in_place: bool) -> anyhow::Result<()> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    let looks_xml = raw.trim_start().starts_with("<?xml") && raw.contains("<fcpxml");
    let is_fcpxml = path.ends_with(".fcpxml") || looks_xml;
    let (mut timeline, sidecar, out_path) = if is_fcpxml {
        let (major, minor) = fcpxml_version(&raw);
        let tl = cairn_tl::fcpxml::parse_fcpxml(&raw)
            .map_err(|e| anyhow::anyhow!("fcpxml ingest: {e}"))?;
        let sc = cairn_tl::sidecar::Sidecar::for_fcpxml(major, minor, raw.as_bytes());
        let out = canonical_out_path(path);
        (tl, sc, out)
    } else {
        let tl =
            cairn_tl::parse::parse_otio(&raw).map_err(|e| anyhow::anyhow!("otio parse: {e}"))?;
        (
            tl,
            cairn_tl::sidecar::Sidecar::for_otio(raw.as_bytes()),
            canonical_out_path(path),
        )
    };
    // stamp identities (idempotent — already-stamped documents pass through)
    cairn_tl::model::stamp_all(&mut timeline);
    let canonical = cairn_tl::canon::serialize_file(&timeline)
        .map_err(|e| anyhow::anyhow!("canonical serialize: {e}"))?;
    let target = if in_place && !is_fcpxml {
        path.to_string()
    } else {
        out_path
    };
    std::fs::write(&target, &canonical)
        .map_err(|e| anyhow::anyhow!("cannot write {target}: {e}"))?;
    let sidecar_path = format!("{target}.cairn-timeline");
    std::fs::write(&sidecar_path, sidecar.to_json())
        .map_err(|e| anyhow::anyhow!("cannot write {sidecar_path}: {e}"))?;
    println!("captured: {target} (+ {sidecar_path})");
    println!("elements stamped: {}", timeline.tracks.count());
    Ok(())
}

fn canonical_out_path(path: &str) -> String {
    let stem = path
        .strip_suffix(".otio")
        .unwrap_or_else(|| path.strip_suffix(".fcpxml").unwrap_or(path));
    format!("{stem}.canonical.otio")
}

fn fcpxml_version(raw: &str) -> (u32, u32) {
    // <fcpxml version="1.11"> — major 1, minor 11 (NOT 1.1: version parse bug
    // class from the pre-release, caught by the round-11 style gates)
    if let Some(pos) = raw.find("<fcpxml") {
        let head = &raw[pos..(pos + 80).min(raw.len())];
        if let Some(vpos) = head.find("version=\"") {
            let rest = &head[vpos + 9..];
            if let Some(end) = rest.find('"') {
                let v = &rest[..end];
                if let Some((ma, mi)) = v.split_once('.') {
                    return (ma.parse().unwrap_or(1), mi.parse().unwrap_or(0));
                }
            }
        }
    }
    (1, 0)
}

fn load_timeline_sidecar(
    path: &str,
) -> anyhow::Result<(
    cairn_tl::model::Timeline,
    Option<cairn_tl::sidecar::Sidecar>,
)> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    let sc = std::fs::read_to_string(format!("{path}.cairn-timeline"))
        .ok()
        .and_then(|t| cairn_tl::sidecar::Sidecar::parse(&t).ok());
    if path.ends_with(".fcpxml") {
        let tl = cairn_tl::fcpxml::parse_fcpxml(&raw)
            .map_err(|e| anyhow::anyhow!("fcpxml ingest {path}: {e}"))?;
        Ok((tl, sc))
    } else {
        let tl = cairn_tl::parse::parse_otio(&raw)
            .map_err(|e| anyhow::anyhow!("otio parse {path}: {e}"))?;
        Ok((tl, sc))
    }
}

fn run_tl_merge(
    base: &str,
    ours: &str,
    theirs: &str,
    dry_run: bool,
    out_arg: Option<&str>,
    semantic: bool,
) -> anyhow::Result<()> {
    // The exit-code contract: 3 means REFUSED — structural mismatch or any
    // input that cannot be parsed. Parsing failures are refusals (C10), not
    // runtime errors: no partial state, both inputs untouched.
    let refuse = |msg: String| -> ! {
        eprintln!("REFUSED: {msg}");
        eprintln!("no output written — both inputs remain untouched for the human");
        std::process::exit(3);
    };
    let (base_tl, base_sc) = load_timeline_sidecar(base).unwrap_or_else(|e| refuse(e.to_string()));
    let (ours_tl, ours_sc) = load_timeline_sidecar(ours).unwrap_or_else(|e| refuse(e.to_string()));
    let (theirs_tl, theirs_sc) =
        load_timeline_sidecar(theirs).unwrap_or_else(|e| refuse(e.to_string()));

    // sidecar version gate (C10) — only when sidecars exist (un-stamped docs
    // merge on their own bytes; the gate is the capture-substrate contract)
    if let (Some(b), Some(o), Some(t)) = (&base_sc, &ours_sc, &theirs_sc) {
        if let Err(e) = cairn_tl::sidecar::check_mergeable(b, o, t) {
            refuse(e.to_string());
        }
    }

    let options = cairn_tl::merge::MergeOptions { semantic };
    match cairn_tl::merge::merge_with(&base_tl, &ours_tl, &theirs_tl, &options) {
        Ok((merged, report)) => {
            let code = match report.outcome {
                cairn_tl::merge::Outcome::Clean => 0,
                cairn_tl::merge::Outcome::Notes => 1,
                cairn_tl::merge::Outcome::Conflicts => 2,
            };
            if dry_run {
                println!(
                    "dry-run: {}",
                    serde_json::to_string_pretty(&report.to_json())?
                );
            } else {
                // .name.merged.otio next to ours (ADR §2.5: NEW file, never
                // in-place — the conflict-copy machinery stays the backstop);
                // --out overrides for pipeline callers
                let out = out_arg.map(str::to_string).unwrap_or_else(|| {
                    format!("{}.merged.otio", ours.strip_suffix(".otio").unwrap_or(ours))
                });
                let bytes = cairn_tl::canon::serialize_file(&merged)
                    .map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
                std::fs::write(&out, bytes)
                    .map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
                // .cairn-timeline/reports/<seq>.json relative to the ours
                // document's directory (ADR §2.5); seq = existing count + 1
                let dir = std::path::Path::new(ours)
                    .parent()
                    .unwrap_or(std::path::Path::new("."));
                let reports_dir = dir.join(".cairn-timeline").join("reports");
                std::fs::create_dir_all(&reports_dir).ok();
                let seq = std::fs::read_dir(&reports_dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).count())
                    .unwrap_or(0)
                    + 1;
                let report_path = reports_dir.join(format!("{seq}.json"));
                std::fs::write(
                    &report_path,
                    serde_json::to_string_pretty(&report.to_json())?,
                )
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", report_path.display()))?;
                println!("merged: {out}");
                println!("report: {}", report_path.display());
            }
            println!(
                "outcome: {:?} (applied={}, withheld={}, deduped={})",
                report.outcome, report.stats.applied, report.stats.withheld, report.stats.deduped
            );
            for v in &report.verdicts {
                println!(
                    "  C{:<2} {:<7} ours=[{}] theirs=[{:?}] {}",
                    v.class,
                    format!("{:?}", v.verdict),
                    v.ours,
                    v.theirs,
                    v.note
                );
            }
            std::process::exit(code);
        }
        Err(refusal) => {
            eprintln!("REFUSED: {}", refusal.0);
            eprintln!("no output written — both inputs remain untouched for the human");
            std::process::exit(3);
        }
    }
}

// ---------------------------------------------------------------------------
// TlVerify — the round-trip audit (ADR-0018): the broken-speed-ramp /
// dropped-title detector. Exit codes: 0 = clean, 1 = LOSS(es), 3 = refused.
// ---------------------------------------------------------------------------

fn run_tl_verify(base: &str, roundtrip: &str, json: bool) -> anyhow::Result<()> {
    let (base_tl, _) = load_timeline_sidecar(base)
        .map_err(|e| anyhow::anyhow!("cannot parse base {base}: {e}"))?;
    let (rt_tl, _) = load_timeline_sidecar(roundtrip)
        .map_err(|e| anyhow::anyhow!("cannot parse round-trip {roundtrip}: {e}"))?;
    let rep = cairn_tl::verify::verify_roundtrip(&base_tl, &rt_tl);
    if json {
        println!("{}", serde_json::to_string_pretty(&rep.to_json())?);
    } else {
        for c in &rep.checks {
            let mark = match c.severity {
                cairn_tl::verify::Severity::Loss => "LOSS",
                cairn_tl::verify::Severity::Warn => "warn",
            };
            println!("[{mark}] {}: {}", c.name, c.detail);
        }
        println!(
            "result: {} (loss={}, warn={})",
            if rep.passed() {
                "PASSED — round-trip is frame-accurate"
            } else {
                "FAILED — content was lost"
            },
            rep.loss_count,
            rep.warn_count
        );
    }
    std::process::exit(i32::from(!rep.passed()));
}

// ---------------------------------------------------------------------------
// Notes — frame-anchored review notes (ADR-0018)
// ---------------------------------------------------------------------------

fn run_notes(cmd: notes::NotesCmd) -> anyhow::Result<()> {
    use cairn_tl::notes::{csv, NoteSet};

    match cmd {
        notes::NotesCmd::Import {
            csv: csv_path,
            out,
            author,
            rate,
        } => {
            let text = std::fs::read_to_string(&csv_path)
                .map_err(|e| anyhow::anyhow!("{csv_path}: {e}"))?;
            let set = csv::import(&text, &author, i128::from(rate)).map_err(|errs| {
                anyhow::anyhow!(
                    "csv import: {} bad row(s): {}",
                    errs.len(),
                    errs.iter()
                        .map(|e| format!("line {}: {}", e.line, e.reason))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })?;
            let out = out.unwrap_or_else(|| {
                format!(
                    "{}.notes.json",
                    csv_path.strip_suffix(".csv").unwrap_or(&csv_path)
                )
            });
            let bytes = set.to_json().map_err(|e| anyhow::anyhow!("{e}"))?;
            std::fs::write(&out, bytes).map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
            println!("imported {} notes → {out}", set.len());
            Ok(())
        }
        notes::NotesCmd::List { file } => {
            let bytes = std::fs::read(&file).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
            let set = NoteSet::from_json(&bytes).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
            let mut rows: Vec<_> = set.notes.values().collect();
            rows.sort_by_key(|n| (n.anchor.frame, n.id.clone()));
            println!(
                "{:<8} {:<18} {:<10} {:<9} BODY",
                "FRAME", "TC", "AUTHOR", "STATUS"
            );
            for n in rows {
                println!(
                    "{:<8} {:<18} {:<10} {:<9} {}",
                    n.anchor.frame,
                    csv::timecode(n.anchor.frame, n.anchor.rate),
                    n.author,
                    n.status.as_str(),
                    n.body
                );
            }
            Ok(())
        }
        notes::NotesCmd::Export { file, out } => {
            let bytes = std::fs::read(&file).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
            let set = NoteSet::from_json(&bytes).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
            let csv_text = csv::export(&set);
            match out {
                Some(path) => {
                    std::fs::write(&path, csv_text)
                        .map_err(|e| anyhow::anyhow!("cannot write {path}: {e}"))?;
                    println!("exported {} notes → {path}", set.len());
                }
                None => print!("{csv_text}"),
            }
            Ok(())
        }
        notes::NotesCmd::Merge {
            base,
            ours,
            theirs,
            out,
        } => {
            let read = |p: &str| -> anyhow::Result<NoteSet> {
                let bytes = std::fs::read(p).map_err(|e| anyhow::anyhow!("{p}: {e}"))?;
                NoteSet::from_json(&bytes).map_err(|e| anyhow::anyhow!("{p}: {e}"))
            };
            let b = read(&base)?;
            let o = read(&ours)?;
            let t = read(&theirs)?;
            let m = cairn_tl::notes::merge_notes(&b, &o, &t);
            let out = out.unwrap_or_else(|| {
                format!(
                    "{}.merged.notes.json",
                    ours.strip_suffix(".notes.json").unwrap_or(&ours)
                )
            });
            let bytes = m.merged.to_json().map_err(|e| anyhow::anyhow!("{e}"))?;
            std::fs::write(&out, bytes).map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
            println!("merged {} notes → {out}", m.merged.len());
            if m.conflicts.is_empty() {
                println!("conflicts: none");
            } else {
                println!("CONFLICTS ({}): a human decides:", m.conflicts.len());
                for c in &m.conflicts {
                    println!("  anchor {}:", c.anchor);
                    println!("    ours:   [{}] {}", c.ours.author, c.ours.body);
                    println!("    theirs: [{}] {}", c.theirs.author, c.theirs.body);
                    println!("    reason: {}", c.reason);
                }
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Lock/Unlock — bin-locking write authority (ADR-0014 local pens; the
// "keep your hands off my sequence" contract). Exit 2 when ALREADY held.
// ---------------------------------------------------------------------------

fn run_lock(home: &std::path::Path, project: &str, path: &str) -> anyhow::Result<()> {
    let store = cairn_store::Store::open(home, std::sync::Arc::new(cairn_core::clock::WallClock))
        .map_err(|e| anyhow::anyhow!("store: {e}"))?;
    // held by a LIVE process? (this machine's truth; the daemon heartbeats the
    // server-side pen — a dead process's row is reaped by the keepalive)
    if let Some((token, _expires)) = store.get_lease(path) {
        let holder = store.list_leases_pid().into_iter().find(|r| r.path == path);
        let pid_live = holder
            .as_ref()
            .and_then(|r| r.pid)
            .map(cairn_store::db::process_alive)
            .unwrap_or(true);
        let proj_ok = holder.as_ref().and_then(|r| r.project_id.clone());
        if pid_live && proj_ok.as_deref() != Some("") && proj_ok != Some(project.to_string()) {
            eprintln!("LOCKED by another project on this machine (token {token})");
            std::process::exit(2);
        }
    }
    // device id from the logged-in identity (falls back to a fresh id when
    // unlocked — the lock still works, it just names an anonymous device)
    let device = crate::projects::load_identity(&store)
        .map(|i| i.device_id)
        .unwrap_or_else(cairn_core::ids::new_device_id);
    let now_ms = <cairn_core::clock::WallClock as cairn_core::clock::SystemClock>::now_millis(
        &cairn_core::clock::WallClock,
    );
    let expires = now_ms + cairn_sync::LEASE_TTL_MS as i64;
    let token = u64::try_from(now_ms).unwrap_or(1).max(1);
    store
        .put_lease_pid(
            path,
            token,
            expires,
            Some(i64::from(std::process::id())),
            Some(project),
            Some(&device),
        )
        .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
    println!(
        "locked: {project}/{path} (device {device}, TTL {}s)",
        cairn_sync::LEASE_TTL_MS / 1000
    );
    Ok(())
}

fn run_unlock(home: &std::path::Path, project: &str, path: &str) -> anyhow::Result<()> {
    let store = cairn_store::Store::open(home, std::sync::Arc::new(cairn_core::clock::WallClock))
        .map_err(|e| anyhow::anyhow!("store: {e}"))?;
    store
        .drop_lease(path)
        .map_err(|e| anyhow::anyhow!("unlock: {e}"))?;
    println!("unlocked: {project}/{path}");
    Ok(())
}

// ---------- review portal (ADR-0020) ----------

fn run_review(cmd: review_cmd::ReviewCmd) -> anyhow::Result<()> {
    use review_cmd::ReviewCmd;
    use std::path::Path;
    match cmd {
        ReviewCmd::Publish {
            root,
            title,
            media,
            proxy,
            no_proxy,
            fps,
            frames,
            label,
            timeline,
            snapshot,
            by,
        } => {
            // RBAC (ADR-0020 §4): publishing is ManageReview
            members::guard(
                Path::new(&root),
                &members::acting_device(None),
                cairn_core::rbac::Permission::ManageReview,
            )?;
            // rate + duration: explicit flags win, ffprobe fills the rest,
            // honest failure when neither knows (a guessed frame count
            // corrupts every comment timecode after it — dogfood #1)
            let probed = review::probe_media(&Path::new(&root).join(&media));
            let fps_spec = match (&fps, &probed) {
                (Some(f), _) => f.clone(),
                (None, Some((n, d, _))) => {
                    println!("probed {media}: {n}/{d} fps");
                    format!("{n}/{d}")
                }
                (None, None) => "24".to_string(),
            };
            let frames_v = match (frames, &probed) {
                (Some(f), _) => f,
                (None, Some((_, _, fr))) => {
                    println!("probed {media}: {fr} frames");
                    *fr
                }
                (None, None) => anyhow::bail!(
                    "cannot determine the cut's frame count: pass --frames \
                     (ffprobe could not read {media})"
                ),
            };
            let fps = review::parse_fps(&fps_spec)
                .map_err(|e| anyhow::anyhow!("--fps: {e}"))
                .unwrap();
            // proxy: explicit > generated > none (guests then stream the
            // full file — publish never fails on a transcode problem)
            let proxy_rel = match proxy {
                Some(p) => Some(p),
                None if !no_proxy => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    match cairn_proxy::pipeline::generate(
                        Path::new(&root),
                        &media,
                        &cairn_proxy::model::ProxyProfile::review(),
                        &cairn_proxy::transcode::FfmpegTranscoder,
                        now,
                    ) {
                        Ok(entry) => {
                            println!(
                                "proxy: {} ({} bytes, {:.1}% of source)",
                                entry.proxy_rel,
                                entry.bytes,
                                100.0 * entry.bytes as f64
                                    / std::fs::metadata(Path::new(&root).join(&media))
                                        .map(|m| m.len().max(1) as f64)
                                        .unwrap_or(1.0)
                            );
                            Some(entry.proxy_rel)
                        }
                        Err(e) => {
                            println!(
                                "note: no proxy generated ({e}) — guests stream the full file"
                            );
                            None
                        }
                    }
                }
                None => None,
            };
            let timeline_fp = timeline.map(|p| {
                let (tl, _) = load_timeline_sidecar(&p)
                    .map_err(|e| anyhow::anyhow!("timeline {p}: {e}"))
                    .unwrap();
                tl.tracks.content_fingerprint()
            });
            let n = review::cmd_publish(
                Path::new(&root),
                title.as_deref(),
                &media,
                proxy_rel.as_deref(),
                fps,
                frames_v,
                &label,
                timeline_fp.as_deref(),
                snapshot.as_deref(),
                &by,
            )?;
            println!("published v{n} to the review stack");
            println!("next: cairn review link --note 'client' (mint a guest link)");
        }
        ReviewCmd::Link {
            root,
            role,
            note,
            ttl_hours,
            latest_only,
        } => {
            // RBAC: minting client links is ManageReview
            members::guard(
                Path::new(&root),
                &members::acting_device(None),
                cairn_core::rbac::Permission::ManageReview,
            )?;
            let role = cairn_review::model::GuestRole::parse(&role)
                .unwrap_or_else(|| panic!("--role must be commenter or viewer"));
            let (token, exp) =
                review::cmd_link(Path::new(&root), role, &note, ttl_hours, latest_only)?;
            println!("token: {token}");
            println!(
                "url:   http://<review-host>:17778/r/{token}   (cairn daemon --review 0.0.0.0:17778)"
            );
            if exp > 0 {
                let dt = chrono_like(exp);
                println!("expires: {dt}");
            } else {
                println!("expires: never");
            }
        }
        ReviewCmd::List { root } => review::cmd_list(Path::new(&root))?,
        ReviewCmd::Comments { root, version } => {
            review::cmd_comments(Path::new(&root), version)?;
        }
        ReviewCmd::ExportMarkers {
            root,
            version,
            out,
            otio,
            timeline,
        } => {
            handoff::cmd_export_markers(
                Path::new(&root),
                version,
                &out,
                otio,
                timeline.as_deref(),
            )?;
        }
        ReviewCmd::ExportChangelist {
            root,
            version,
            out,
            format,
        } => {
            run_export_changelist(Path::new(&root), version, &out, &format)?;
        }
        ReviewCmd::ApplyChangelist {
            timeline,
            changelist,
            out,
            yes,
        } => {
            run_apply_changelist(&timeline, &changelist, out.as_deref(), yes)?;
        }
        ReviewCmd::Resolve {
            root,
            version,
            id,
            status,
        } => {
            let st = cairn_tl::notes::NoteStatus::parse(&status).ok_or_else(|| {
                anyhow::anyhow!("--status must be OPEN, RESOLVED, or REJECTED (got {status:?})")
            })?;
            review::cmd_resolve(Path::new(&root), version, &id, st)?;
            println!("comment {id} -> {}", st.as_str());
        }
    }
    Ok(())
}

/// Millis → a readable UTC stamp without pulling a date crate: fixed
/// civil-from-days math (Howard Hinnant's algorithm).
#[allow(clippy::many_single_char_names)] // Hinnant's published variable names
fn chrono_like(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

// ---------- proxy workflow (ADR-0020 §3) ----------

fn run_proxy(cmd: proxy_cmd::ProxyCmd) -> anyhow::Result<()> {
    use proxy_cmd::ProxyCmd;
    use std::path::Path;
    match cmd {
        ProxyCmd::Generate {
            root,
            media,
            max_height,
            crf,
            copy,
        } => {
            proxy::cmd_generate(Path::new(&root), &media, max_height, crf, copy)?;
        }
        ProxyCmd::List { root } => proxy::cmd_list(Path::new(&root))?,
        ProxyCmd::Status { root, media } => proxy::cmd_status(Path::new(&root), &media)?,
    }
    Ok(())
}

// ---------- membership / RBAC (ADR-0020 §4) ----------

fn run_member(cmd: member_cmd::MemberCmd) -> anyhow::Result<()> {
    use member_cmd::MemberCmd;
    use std::path::Path;
    match cmd {
        MemberCmd::Add {
            root,
            device,
            name,
            role,
            as_device,
        } => {
            let role = cairn_core::rbac::Role::parse(&role)
                .ok_or_else(|| anyhow::anyhow!("--role: unknown role '{role}'"))?;
            members::cmd_add(Path::new(&root), &device, &name, role, as_device.as_deref())?;
        }
        MemberCmd::Remove {
            root,
            device,
            as_device,
        } => {
            members::cmd_remove(Path::new(&root), &device, as_device.as_deref())?;
        }
        MemberCmd::List { root } => members::cmd_list(Path::new(&root))?,
        MemberCmd::Check { root, device, perm } => {
            if !members::cmd_check(Path::new(&root), &device, &perm)? {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

// ---------- AAF/OMF handoff ledger (ADR-0020 §6) ----------

fn run_handoff(cmd: handoff_cmd::HandoffCmd) -> anyhow::Result<()> {
    use handoff_cmd::HandoffCmd;
    use std::path::Path;
    match cmd {
        HandoffCmd::Record {
            root,
            file,
            timeline,
            snapshot,
            note,
            by,
        } => {
            members::guard(
                Path::new(&root),
                &members::acting_device(None),
                cairn_core::rbac::Permission::Snapshot,
            )?;
            handoff::cmd_record(
                Path::new(&root),
                &file,
                timeline.as_deref(),
                snapshot.as_deref(),
                &note,
                &by,
            )?;
        }
        HandoffCmd::Verify {
            root,
            file,
            timeline,
        } => {
            handoff::cmd_verify(Path::new(&root), file.as_deref(), timeline.as_deref())?;
        }
        HandoffCmd::List { root } => handoff::cmd_list(Path::new(&root))?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Search — intelligent clip search (ADR-0023 §5)
// ---------------------------------------------------------------------------

fn cli_home() -> String {
    std::env::var("CAIRN_HOME").unwrap_or_else(|_| default_home())
}

fn run_search(
    query: &str,
    project: Option<&str>,
    path: Option<&str>,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let (root, files): (std::path::PathBuf, Vec<(String, u64)>) = match (project, path) {
        (Some(pid), None) => {
            let home = std::path::PathBuf::from(cli_home());
            let store =
                cairn_store::Store::open(&home, std::sync::Arc::new(cairn_core::clock::WallClock))
                    .map_err(|e| anyhow::anyhow!("cannot open home store: {}", e.message))?;
            let root = cairn_sync::workspace::workspace_dir(&store, pid);
            if !root.exists() {
                anyhow::bail!("project `{pid}` has no attached workspace on this machine");
            }
            let rows = store.list_files(pid);
            let files = rows.into_iter().map(|r| (r.path, r.size)).collect();
            (root, files)
        }
        (None, Some(p)) => {
            let root = std::path::PathBuf::from(p);
            if !root.is_dir() {
                anyhow::bail!("{} is not a directory", root.display());
            }
            // direct mode: walk the disk (bounded, sorted, deterministic)
            fn walk(dir: &std::path::Path, out: &mut Vec<(String, u64)>, root: &std::path::Path) {
                let Ok(rd) = std::fs::read_dir(dir) else {
                    return;
                };
                let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
                entries.sort_by_key(|e| e.file_name());
                for e in entries {
                    let Ok(meta) = e.metadata() else { continue };
                    let p = e.path();
                    if meta.is_dir() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with(".cairn") {
                            continue;
                        }
                        walk(&p, out, root);
                    } else {
                        let rel = p
                            .strip_prefix(root)
                            .map(|r| r.to_string_lossy().into_owned())
                            .unwrap_or_else(|_| p.to_string_lossy().into_owned());
                        out.push((rel, meta.len()));
                    }
                }
            }
            let mut files = Vec::new();
            walk(&root, &mut files, &root);
            (root, files)
        }
        _ => anyhow::bail!("pass exactly one of --project <id> or --path <dir>"),
    };
    let hits = search::search_project(&root, &files, query, limit);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "query": query,
                "root": root.to_string_lossy(),
                "hits": hits.iter().map(|h| serde_json::json!({
                    "kind": h.kind,
                    "path": h.path,
                    "score": h.score,
                    "clip_name": h.clip_name,
                    "clip_media": h.clip_media,
                    "clip_tc_in": h.clip_tc_in,
                    "clip_dur": h.clip_dur,
                })).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!("searching: {}", root.display());
        search::render(&hits);
        println!("{} hit(s)", hits.len());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Changelist — the 3-step no-AI recipe (ADR-0023 §3)
// ---------------------------------------------------------------------------

fn run_export_changelist(
    root: &std::path::Path,
    version: u32,
    out: &str,
    format: &str,
) -> anyhow::Result<()> {
    let file = cairn_review::Store::load(root)
        .map_err(|e| anyhow::anyhow!("cannot load review session: {e}"))?;
    let file = file.ok_or_else(|| anyhow::anyhow!("no review session in {}", root.display()))?;
    let vn = file
        .version(version)
        .ok_or_else(|| anyhow::anyhow!("version {version} not found"))?;
    let notes = cairn_review::Store::load_comments(root, version)
        .map_err(|e| anyhow::anyhow!("cannot load comments: {e}"))?;
    if notes.is_empty() {
        anyhow::bail!("version {version} has no comments to turn into a changelist");
    }
    let cl = cairn_tl::note_ops::Changelist::from_notes(&notes);
    let title = format!("{} v{version}", file.title);
    let body = match format {
        "json" => serde_json::to_vec_pretty(&cl.to_json(&title))?,
        "edl" => cairn_tl::note_ops::changelist_edl(&cl, &title).into_bytes(),
        "fcpxml" => {
            let (timebase, ntsc) =
                cairn_tl::markers::fcpxml_rate_fields(i64::from(vn.fps_num), i64::from(vn.fps_den));
            cairn_tl::note_ops::changelist_fcpxml(&cl, timebase, ntsc, &title).into_bytes()
        }
        other => anyhow::bail!("unknown format {other:?} (json | edl | fcpxml)"),
    };
    std::fs::write(out, body).map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
    println!("changelist: {out} (format {format})");
    println!(
        "  mechanical: {} note(s) — the robot applies these after your YES",
        cl.mechanical.len()
    );
    println!(
        "  creative:   {} note(s) — highlighted + timestamped, YOUR call",
        cl.creative.len()
    );
    for m in &cl.mechanical {
        for op in &m.ops {
            println!("  [mech] @{} {}: {}", m.frame, m.author, op.summary());
        }
    }
    for c in &cl.creative {
        println!("  [creative] @{} {}: {}", c.frame, c.author, c.body);
    }
    Ok(())
}

fn run_apply_changelist(
    timeline: &str,
    changelist: &str,
    out_arg: Option<&str>,
    yes: bool,
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(changelist)
        .map_err(|e| anyhow::anyhow!("cannot read {changelist}: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("cannot parse {changelist}: {e}"))?;
    if v["schema"] != "cairn-changelist/v1" {
        anyhow::bail!(
            "{changelist} is not a cairn changelist (schema {:?})",
            v["schema"]
        );
    }
    // rebuild MechItems from the JSON (note ops + anchors)
    let mut items: Vec<cairn_tl::note_ops::MechItem> = Vec::new();
    for m in v["mechanical"].as_array().unwrap_or(&Vec::new()) {
        let mut ops = Vec::new();
        for op in m["ops"].as_array().unwrap_or(&Vec::new()) {
            let kind = op["kind"].as_str().unwrap_or("");
            let seconds = op["seconds"]["num"]
                .as_i64()
                .zip(op["seconds"]["den"].as_i64());
            let mag = seconds
                .map(|(num, den)| {
                    let r =
                        cairn_tl::rational::Rational::new(i128::from(num), i128::from(den)).ok();
                    r.map(|r| {
                        let f = r.to_f64_approx();
                        if (f - f.round()).abs() < 1e-9 {
                            format!("{}", f.round() as i64)
                        } else {
                            let s = format!("{f:.3}");
                            s.trim_end_matches('0').trim_end_matches('.').to_string()
                        }
                    })
                    .unwrap_or_default()
                })
                .unwrap_or_default();
            let op = match kind {
                "trim_out" => cairn_tl::note_ops::MechOp::TrimOut { seconds: mag },
                "trim_in" => cairn_tl::note_ops::MechOp::TrimIn { seconds: mag },
                "delete" => cairn_tl::note_ops::MechOp::Delete,
                "replace" => cairn_tl::note_ops::MechOp::Replace {
                    target: m["body"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .trim_start_matches("replace with ")
                        .to_string(),
                },
                "gain" => cairn_tl::note_ops::MechOp::Gain { db: mag },
                _ => continue,
            };
            ops.push(op);
        }
        if ops.is_empty() {
            continue;
        }
        items.push(cairn_tl::note_ops::MechItem {
            note_id: m["note_id"].as_str().unwrap_or("").to_string(),
            author: m["author"].as_str().unwrap_or("").to_string(),
            frame: m["frame"].as_i64().unwrap_or(0).into(),
            rate: m["rate"].as_i64().unwrap_or(24).into(),
            body: m["body"].as_str().unwrap_or("").to_string(),
            ops,
            remainder: Vec::new(),
        });
    }
    if items.is_empty() {
        anyhow::bail!("changelist carries no mechanical ops — nothing to apply");
    }
    let (tl, _) = load_timeline_sidecar(timeline)
        .map_err(|e| anyhow::anyhow!("cannot parse {timeline}: {e}"))?;
    let (out_tl, ledger) = cairn_tl::note_ops::apply_changelist(&tl, &items);

    // the YES/NO gate: preview is the default; nothing is written without it
    println!("changelist apply — preview ({} op(s)):", ledger.len());
    let mut applied = 0usize;
    for l in &ledger {
        match &l.status {
            cairn_tl::note_ops::ApplyStatus::Applied => {
                applied += 1;
                println!("  [applied]   {} — {}", l.note_id, l.summary);
            }
            cairn_tl::note_ops::ApplyStatus::Unresolved(why) => {
                println!("  [unresolved] {} — {} ({why})", l.note_id, l.summary);
            }
        }
    }
    if !yes {
        println!(
            "\nDRY RUN — nothing written. Pass --yes to write the result ({} of {} ops would land).",
            applied,
            ledger.len()
        );
        std::process::exit(1);
    }
    let out = out_arg.map(str::to_string).unwrap_or_else(|| {
        format!(
            "{}.changelist.otio",
            timeline.strip_suffix(".otio").unwrap_or(timeline)
        )
    });
    let bytes = cairn_tl::canon::serialize_file(&out_tl)
        .map(String::into_bytes)
        .map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    std::fs::write(&out, bytes).map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
    println!(
        "\nwrote {out} ({applied}/{}) ops applied — the source timeline is untouched",
        ledger.len()
    );
    Ok(())
}

#[cfg(test)]
mod chrono_tests {
    #[test]
    fn civil_date_math_matches_known_stamp() {
        // 2026-09-04 00:00:00 UTC = 1788480000 s
        assert_eq!(
            super::chrono_like(1_788_480_000_000),
            "2026-09-04 00:00:00 UTC"
        );
        assert_eq!(super::chrono_like(0), "1970-01-01 00:00:00 UTC");
        // 2000-02-29 12:30:45 UTC = 951827445
        assert_eq!(
            super::chrono_like(951_827_445_000),
            "2000-02-29 12:30:45 UTC"
        );
    }
}
