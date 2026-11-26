//! `rehash` subcommand: convert an offline store to another hash.
//!
//! ```text
//! slicefs rehash --to <hash> [--dry-run] <store>
//! ```
//!
//! Every live value (file content, file manifests and metadata nodes) of
//! the current root and of each snapshot root is written again under the
//! target hash (`metadata::rehash`). Then the new roots become current in
//! one step, the header records the target hash, and a full GC frees the
//! old objects.
//!
//! Offline only: the command holds the exclusive store lock
//! (`header::lock_offline`) from the start to the end, GC included. It
//! refuses while a mount or another command holds the lock, while a
//! `mount.lock` file is present, and while the write log has records that
//! are not in a root.
//!
//! The steps of a real run, and what a crash after each step leaves:
//!
//! 1. Checks (lock, header, `mount.lock`, write log). Nothing written.
//! 2. The header flag of the target hash, if it needs one (SHA-256). The
//!    store still uses its old root; the flag alone is valid.
//! 3. Rewrite into new objects, with the temp files in
//!    `<store>/rehash-tmp/`; then a sync of the chunk store. The old root
//!    is current; the new objects are garbage for the next GC.
//! 4. The switch: one segment with the new root and the new root of every
//!    snapshot, made current by one rename. Before the rename the store
//!    has its old roots, after it the new roots.
//! 5. The header hash line is set to the target. Before that a mount
//!    writes with the old hash: a mixed store, which is valid.
//! 6. A full GC on the open store with the new roots frees the old objects.
//! 7. `rehash-tmp` is removed and the report is made.
//!
//! A run again after a crash at any step completes the job: values that
//! already have the target hash are kept, and the GC always runs.
//!
//! `--dry-run` takes the same lock and makes the same checks, then reads
//! and verifies the live tree through a read-only open and writes nothing.

use std::path::Path;
use std::time::Duration;

use chunk_store::{hasher_by_name, ObjectHasher};
use metadata::rehash::{rehash_roots, Mode, RehashOptions, RootSet, Stats};
use metadata::segment::{remove_switch_tmps, scan_store_with_log, write_switch_segment};

use crate::content::{self, REHASH_TMP_DIR};
use crate::gc;
use crate::header::{self, Want};

type Error = Box<dyn std::error::Error>;

/// The step boundaries of a real run, for the hidden stop hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Checked,
    FlagWritten,
    Rewritten,
    Synced,
    Switched,
    HeaderWritten,
    GcDone,
}

/// Least time between two progress lines of the rewrite.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(5);

/// The arguments of a run.
pub struct RehashArgs<'a> {
    pub store: &'a Path,
    /// The name of the target hash.
    pub to: &'a str,
    pub dry_run: bool,
    /// Least time between two progress lines of the rewrite.
    pub progress_every: Duration,
    /// For crash tests: stop with an error right after this step, with no
    /// cleanup.
    #[doc(hidden)]
    pub stop_after: Option<Step>,
    /// For crash tests: stop the rewrite after this many rewritten values.
    #[doc(hidden)]
    pub stop_after_values: Option<u64>,
}

impl<'a> RehashArgs<'a> {
    /// A run with progress lines every [`PROGRESS_EVERY`] and no hooks.
    pub fn new(store: &'a Path, to: &'a str, dry_run: bool) -> Self {
        RehashArgs { store, to, dry_run, progress_every: PROGRESS_EVERY, stop_after: None, stop_after_values: None }
    }
}

/// The refusal for write-log records after the last root.
pub fn msg_log_pending(store: &Path) -> String {
    format!("{} has write-log records that are not in a root. Mount and unmount the store once, then run rehash again.", store.display())
}

/// The refusal for a `mount.lock` file that no mount holds.
pub fn msg_stale_mount_lock(store: &Path) -> String {
    format!("{} has a mount.lock file, but no mount holds the store: a mount did not end cleanly. Mount and unmount the store once, then run rehash again.", store.display())
}

/// Stop here if the hook asks for it.
fn hook(args: &RehashArgs<'_>, step: Step) -> Result<(), Error> {
    if args.stop_after == Some(step) {
        return Err(format!("rehash stopped after step {step:?} (test hook)").into());
    }
    Ok(())
}

/// The progress line of the rewrite.
fn rewrite_line(s: &Stats) -> String {
    format!(
        "rehash: rewrite: root {} of {}: {} values read ({} bytes), {} rewritten",
        s.roots_done, s.roots_total, s.values_read, s.bytes_read, s.values_rewritten
    )
}

/// Run a rehash and return its report. Progress lines go to `progress`.
pub fn rehash_report_with(args: &RehashArgs<'_>, progress: &mut dyn FnMut(&str)) -> Result<String, Error> {
    let store = args.store;
    // Step 1: the checks. Nothing in this part writes the store.
    content::require_store(store)?;
    let target = hasher_by_name(args.to).ok_or_else(|| content::parse_hash(args.to).unwrap_err())?;
    let _store_lock = header::lock_offline(store)?;
    // Read the header, then refuse what a command that writes refuses (a
    // check with `Want::Write` could write a new header).
    if let Some(h) = header::check(store, Want::Read)?.header {
        header::decide(store, &h, Want::Write)?;
    }
    if store.join("mount.lock").exists() {
        return Err(msg_stale_mount_lock(store).into());
    }
    let segs = store.join("segments");
    let (root, snapshots, log) = scan_store_with_log(&segs).map_err(|e| format!("failed to load segments: {e}"))?;
    if !log.is_empty() {
        return Err(msg_log_pending(store).into());
    }
    let current = root.ok_or_else(|| format!("no committed state found in segments at {}", segs.display()))?;
    let old = RootSet { current, snapshots };
    progress("rehash: checks passed");
    hook(args, Step::Checked)?;

    if args.dry_run {
        return dry_run(args, target, &old, progress);
    }

    // Step 2: the flag of the target hash, before its first object.
    if content::ensure_hash_flag(store, target)? {
        progress("rehash: flag written");
    }
    hook(args, Step::FlagWritten)?;

    // Step 3: the rewrite (the open removes a stale `rehash-tmp`).
    remove_switch_tmps(&segs)?;
    let cs = content::open_with_hasher(store, target)?;
    let before = gc::store_usage(store);
    let map_dir = store.join(REHASH_TMP_DIR);
    let mut on_stats = |s: &Stats| progress(&rewrite_line(s));
    let opts = RehashOptions {
        target,
        mode: Mode::Rewrite { map_dir },
        progress_every: args.progress_every,
        progress: Some(&mut on_stats),
        stop_after_values: args.stop_after_values,
    };
    let (new, _stats) = rehash_roots(&cs, &old, opts)
        .map_err(|e| format!("rehash stopped before the switch: {e}. Nothing was switched; the store still uses its old root."))?;
    hook(args, Step::Rewritten)?;
    cs.sync()?;
    hook(args, Step::Synced)?;

    // Step 4: the switch, when a root changed.
    let changed = new != old;
    if changed {
        write_switch_segment(&segs, new.current, &new.snapshots)?;
    }
    hook(args, Step::Switched)?;

    // Step 5: the header hash line (the flag is already there).
    content::check_write(store, Some(target.name()))?;
    hook(args, Step::HeaderWritten)?;

    // Step 6: always a full GC, so a run after a crash between the switch
    // and the GC frees the old objects.
    let roots: Vec<_> = std::iter::once(new.current).chain(new.snapshots.iter().map(|s| s.root)).collect();
    let freed = gc::gc_on_open_store(&cs, &roots, true)?;
    hook(args, Step::GcDone)?;

    // Step 7: clean up and report.
    cs.close()?;
    drop(cs);
    content::remove_rehash_tmp(store)?;
    let after = gc::store_usage(store);
    Ok(format!(
        "Rehash to {}: done.\nBytes freed: {}.\n{}",
        target.name(),
        freed.removed_bytes,
        gc::usage_line(before, after)
    ))
}

/// The dry-run: read and verify the live tree, count what a real run
/// rewrites. It writes nothing.
fn dry_run(
    args: &RehashArgs<'_>,
    target: &'static dyn ObjectHasher,
    old: &RootSet,
    progress: &mut dyn FnMut(&str),
) -> Result<String, Error> {
    let cs = content::open_read_only(args.store)?;
    let mut on_stats = |s: &Stats| progress(&rewrite_line(s));
    let opts = RehashOptions {
        target,
        mode: Mode::DryRun,
        progress_every: args.progress_every,
        progress: Some(&mut on_stats),
        stop_after_values: None,
    };
    let (_, stats) = rehash_roots(&cs, old, opts)?;
    Ok(format!(
        "Rehash to {} (dry run, nothing written): {} objects of another hash.",
        target.name(),
        stats.other_tag_objects
    ))
}

/// `slicefs rehash`: progress lines on stderr, the report on stdout.
pub fn run_rehash(store: &Path, to: &str, dry_run: bool) -> Result<(), Error> {
    let report = rehash_report_with(&RehashArgs::new(store, to, dry_run), &mut |l| eprintln!("{l}"))?;
    println!("{report}");
    Ok(())
}
