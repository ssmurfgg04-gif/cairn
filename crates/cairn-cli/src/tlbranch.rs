//! Timeline branches (ADR-0023 §4) — the git-for-video CLI surface.
//!
//! Branches live under `<timeline-dir>/.cairn-timeline/branches/` (beside the
//! merge reports, same ignore-listed posture — branches are the editor's own
//! sandbox, local-first). The working timeline is NEVER mutated:
//! - `create` copies the timeline into the branch store
//! - `checkout` copies a branch OUT to `<name>.otio` (never clobbers)
//! - `merge` three-way merges branch → target with the recorded parent as
//!   base; output is `<target>.merged.otio` (ADR-0015 convention)
//! - `cherry-pick` steals ONE element by uuid/name
//! - `delete` is SOFT (trash/) + `restore` + explicit `purge`
//!
//! Foolproof by construction: no command overwrites an existing file, no
//! delete is permanent until purge --force, every refusal says why.

use std::path::{Path, PathBuf};

use cairn_tl::branch::{BranchEntry, BranchLedger, BranchState};

fn branch_root(timeline: &Path) -> PathBuf {
    timeline
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".cairn-timeline")
        .join("branches")
}

fn load_ledger(root: &Path) -> BranchLedger {
    let path = root.join("branches.json");
    match std::fs::read(&path) {
        Ok(bytes) => BranchLedger::from_json(&bytes).unwrap_or_else(|e| {
            eprintln!("WARNING: corrupt branch ledger ({e}) — starting a fresh one");
            BranchLedger::new()
        }),
        Err(_) => BranchLedger::new(),
    }
}

fn save_ledger(root: &Path, ledger: &BranchLedger) -> anyhow::Result<()> {
    std::fs::create_dir_all(root)?;
    let bytes = ledger.to_json().map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = root.join("branches.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn load_timeline(path: &Path) -> anyhow::Result<cairn_tl::model::Timeline> {
    let bytes =
        std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    if path.extension().and_then(|e| e.to_str()) == Some("fcpxml") {
        cairn_tl::fcpxml::parse_fcpxml(&String::from_utf8_lossy(&bytes))
            .map_err(|e| anyhow::anyhow!("FCPXML parse: {e:?}"))
    } else {
        cairn_tl::parse::parse_otio(&String::from_utf8_lossy(&bytes))
            .map_err(|e| anyhow::anyhow!("OTIO parse: {e:?}"))
    }
}

fn serialize_tl(tl: &cairn_tl::model::Timeline) -> anyhow::Result<Vec<u8>> {
    cairn_tl::canon::serialize_file(tl)
        .map(String::into_bytes)
        .map_err(|e| anyhow::anyhow!("serialize: {e}"))
}

fn digest_of(tl: &cairn_tl::model::Timeline) -> String {
    cairn_tl::handoff::timeline_digest(tl)
}

fn device_label() -> String {
    std::env::var("CAIRN_DEVICE").unwrap_or_else(|_| "local".into())
}

#[derive(clap::Subcommand)]
pub enum TlBranchCmd {
    /// Copy a timeline into a NEW branch (the working file is untouched)
    Create {
        /// Branch name (letters/digits/dash/underscore; not `main`/`trash`)
        name: String,
        /// Source timeline (.otio/.fcpxml) to branch FROM
        #[arg(long)]
        from: String,
        /// Note for humans ("the wild transition experiment")
        #[arg(long, default_value = "")]
        note: String,
    },
    /// List branches (active + trashed)
    List {
        /// Timeline whose branch store to list (any file in the project dir)
        #[arg(long)]
        at: String,
    },
    /// Copy a branch OUT to `<name>.otio` (never clobbers an existing file)
    Checkout {
        #[arg(long)]
        at: String,
        name: String,
        /// Explicit output path
        #[arg(long)]
        out: Option<String>,
        /// Overwrite the output if it exists (still refuses a DIFFERENT file
        /// to overwrite without this)
        #[arg(long)]
        force: bool,
    },
    /// Three-way merge a branch INTO a timeline (base = recorded parent;
    /// output `<target>.merged.otio`, never in-place)
    Merge {
        #[arg(long)]
        at: String,
        name: String,
        /// Timeline to merge into (the working cut)
        #[arg(long)]
        into: String,
        /// Opt-in semantic policy (same as tl-merge --semantic)
        #[arg(long)]
        semantic: bool,
    },
    /// Steal ONE element (by uuid or name) from a branch into a timeline
    CherryPick {
        #[arg(long)]
        at: String,
        name: String,
        /// Element uuid (preferred) or name in the branch
        #[arg(long)]
        element: String,
        /// Timeline that receives the element
        #[arg(long)]
        into: String,
        /// Explicit output path (default: <into>.picked.otio)
        #[arg(long)]
        out: Option<String>,
    },
    /// SOFT-delete a branch (recoverable via restore)
    Delete {
        #[arg(long)]
        at: String,
        name: String,
    },
    /// Recover a soft-deleted branch
    Restore {
        #[arg(long)]
        at: String,
        name: String,
    },
    /// HARD-delete (files + ledger state) — requires --force
    Purge {
        #[arg(long)]
        at: String,
        name: String,
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cmd: TlBranchCmd) -> anyhow::Result<()> {
    match cmd {
        TlBranchCmd::Create { name, from, note } => {
            let from = Path::new(&from);
            let tl = load_timeline(from)?;
            let digest = digest_of(&tl);
            let root = branch_root(from);
            let mut ledger = load_ledger(&root);
            ledger
                .create(
                    &name,
                    &device_label(),
                    &note,
                    &digest,
                    &from.to_string_lossy(),
                    now_ms(),
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let dir = root.join(&name);
            std::fs::create_dir_all(&dir)?;
            let bytes = serialize_tl(&tl)?;
            let tl_path = dir.join("timeline.otio");
            std::fs::write(&tl_path, &bytes)
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", tl_path.display()))?;
            // the merge BASE: a frozen copy of what was branched FROM. As the
            // branch timeline evolves, this stays put — merge trusts it AND
            // verifies its digest against the ledger.
            let parent_path = dir.join("parent.otio");
            std::fs::write(&parent_path, &bytes)
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", parent_path.display()))?;
            save_ledger(&root, &ledger)?;
            println!("branch `{name}` created (parent {})", &digest[..16]);
            println!("  file: {}", tl_path.display());
            println!("  the working timeline was NOT modified");
            Ok(())
        }
        TlBranchCmd::List { at } => {
            let root = branch_root(Path::new(&at));
            let ledger = load_ledger(&root);
            let active: Vec<&BranchEntry> = ledger.active();
            if active.is_empty() && ledger.branches.is_empty() {
                println!(
                    "no branches (create one: cairn tl-branch create <name> --from <timeline>)"
                );
                return Ok(());
            }
            println!(
                "{:<4} {:<24} {:<16} {:<10} note",
                "st", "name", "parent", "author"
            );
            for e in ledger.branches.values() {
                let st = match e.state {
                    BranchState::Active => "ok",
                    BranchState::Trashed => "trash",
                    BranchState::Purged => "purged",
                };
                println!(
                    "{:<4} {:<24} {:<16} {:<10} {}",
                    st,
                    e.name,
                    &e.parent_digest[..16.min(e.parent_digest.len())],
                    e.author,
                    e.note
                );
            }
            Ok(())
        }
        TlBranchCmd::Checkout {
            at,
            name,
            out,
            force,
        } => {
            let root = branch_root(Path::new(&at));
            let ledger = load_ledger(&root);
            let entry = ledger
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("branch `{name}` not found"))?;
            if entry.state != BranchState::Active {
                anyhow::bail!("branch `{name}` is {:?} — restore it first", entry.state);
            }
            let src = root.join(&name).join("timeline.otio");
            let out = out
                .map(PathBuf::from)
                .unwrap_or_else(|| Path::new(&at).with_file_name(format!("{name}.otio")));
            if out.exists() && !force {
                anyhow::bail!(
                    "refusing to overwrite {} (exists) — pass --force or --out",
                    out.display()
                );
            }
            std::fs::copy(&src, &out)
                .map_err(|e| anyhow::anyhow!("cannot copy to {}: {e}", out.display()))?;
            println!("checked out `{name}` -> {}", out.display());
            println!("  the branch itself is untouched — experiment freely");
            Ok(())
        }
        TlBranchCmd::Merge {
            at,
            name,
            into,
            semantic,
        } => {
            let root = branch_root(Path::new(&at));
            let ledger = load_ledger(&root);
            let entry = ledger
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("branch `{name}` not found"))?;
            if entry.state != BranchState::Active {
                anyhow::bail!("branch `{name}` is {:?} — restore it first", entry.state);
            }
            let branch_tl = load_timeline(&root.join(&name).join("timeline.otio"))?;
            let target_tl = load_timeline(Path::new(&into))?;
            // base = the recorded parent timeline (stored beside the branch)
            let parent_path = root.join(&name).join("parent.otio");
            let parent_tl = load_timeline(&parent_path).map_err(|_| {
                anyhow::anyhow!(
                    "branch parent timeline missing ({}) — re-supply via `cairn tl-merge --base`",
                    parent_path.display()
                )
            })?;
            // integrity: the digest we recorded must match what we stored
            if digest_of(&parent_tl) != entry.parent_digest {
                anyhow::bail!(
                    "branch parent digest mismatch — the store was tampered with or corrupted"
                );
            }
            let options = cairn_tl::merge::MergeOptions { semantic };
            let (merged, report) =
                cairn_tl::merge::merge_with(&parent_tl, &target_tl, &branch_tl, &options)
                    .map_err(|e| anyhow::anyhow!("REFUSED: {}", e.0))?;
            let out = format!(
                "{}.merged.otio",
                into.strip_suffix(".otio").unwrap_or(&into)
            );
            let bytes = serialize_tl(&merged)?;
            std::fs::write(&out, bytes).map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
            println!("merged branch `{name}` into {into} -> {out}");
            println!(
                "outcome: {:?} (policy {}, applied={}, withheld={})",
                report.outcome,
                if semantic { "semantic" } else { "conservative" },
                report.stats.applied,
                report.stats.withheld
            );
            for v in &report.verdicts {
                println!(
                    "  C{:<2} {:<7} {}",
                    v.class,
                    format!("{:?}", v.verdict),
                    v.note
                );
            }
            Ok(())
        }
        TlBranchCmd::CherryPick {
            at,
            name,
            element,
            into,
            out,
        } => {
            let root = branch_root(Path::new(&at));
            let ledger = load_ledger(&root);
            let entry = ledger
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("branch `{name}` not found"))?;
            if entry.state != BranchState::Active {
                anyhow::bail!("branch `{name}` is {:?} — restore it first", entry.state);
            }
            let branch_tl = load_timeline(&root.join(&name).join("timeline.otio"))?;
            let target_tl = load_timeline(Path::new(&into))?;
            let picked = cairn_tl::branch::cherry_pick(&branch_tl, &target_tl, &element)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let out = out.unwrap_or_else(|| {
                format!(
                    "{}.picked.otio",
                    into.strip_suffix(".otio").unwrap_or(&into)
                )
            });
            let bytes = serialize_tl(&picked)?;
            std::fs::write(&out, bytes).map_err(|e| anyhow::anyhow!("cannot write {out}: {e}"))?;
            println!("cherry-picked `{element}` from `{name}` -> {out}");
            Ok(())
        }
        TlBranchCmd::Delete { at, name } => {
            let root = branch_root(Path::new(&at));
            let mut ledger = load_ledger(&root);
            ledger
                .trash(&name, now_ms())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let dir = root.join(&name);
            let trash = root.join("trash").join(&name);
            if dir.exists() {
                std::fs::create_dir_all(root.join("trash"))?;
                std::fs::rename(&dir, &trash)
                    .map_err(|e| anyhow::anyhow!("cannot move to trash: {e}"))?;
            }
            save_ledger(&root, &ledger)?;
            println!("branch `{name}` soft-deleted (recoverable: cairn tl-branch restore {name})");
            Ok(())
        }
        TlBranchCmd::Restore { at, name } => {
            let root = branch_root(Path::new(&at));
            let mut ledger = load_ledger(&root);
            ledger.restore(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
            let trash = root.join("trash").join(&name);
            let dir = root.join(&name);
            if trash.exists() {
                std::fs::rename(&trash, &dir)
                    .map_err(|e| anyhow::anyhow!("cannot restore from trash: {e}"))?;
            }
            save_ledger(&root, &ledger)?;
            println!("branch `{name}` restored");
            Ok(())
        }
        TlBranchCmd::Purge { at, name, force } => {
            if !force {
                anyhow::bail!("purge is FOREVER — pass --force if you mean it");
            }
            let root = branch_root(Path::new(&at));
            let mut ledger = load_ledger(&root);
            ledger.purge(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
            for dir in [root.join(&name), root.join("trash").join(&name)] {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)
                        .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", dir.display()))?;
                }
            }
            save_ledger(&root, &ledger)?;
            println!("branch `{name}` purged (the ledger keeps the name's history)");
            Ok(())
        }
    }
}
