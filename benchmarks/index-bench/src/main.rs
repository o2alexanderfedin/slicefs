//! Measure the chunk store index (#66): open time, RAM per object, lookups
//! and recovery, on stores of many small synthetic objects.
//!
//! ```text
//! index-bench create --dir D --objects N [--pack-limit BYTES] [--memtable-limit ENTRIES]
//! index-bench open   --dir D [--cache warm|cold]
//! index-bench lookup --dir D [--hits K] [--misses K] [--seed S]
//! index-bench settle --dir D [--timeout-s S]
//! index-bench recover --dir D [--packs 1|all] [--read-only]
//! index-bench gc     --dir D [--live-every K] [--dead-run R]
//! index-bench rewrite --dir D --objects N [--rewrite K] [--pack-limit BYTES] [--cache on|off]
//! index-bench run    --dir D --objects N [--reps R] [--pack-limit BYTES] [--memtable-limit ENTRIES]
//!                    [--hits K] [--misses K] [--drop-caches]
//!   every step also takes --backend NAME (default chunk-store)
//! ```
//!
//! `lookup` times two passes over the same sample in one process: the
//! first pass takes the page faults of the mapped index, the second
//! (`*_2` keys) is warm. `settle` opens the store with the default
//! background work (the idle merge of the index, #111), waits until it is
//! done, and reports the runs and the bytes that the merges wrote; every
//! other step opens with no idle merge, so it measures the index as the
//! step before left it.
//!
//! `gc` is not part of `run`: it removes objects, so it needs a store made
//! for it, and every step refuses that store after it.
//!
//! `create` makes a store of N unique 8-byte objects (object i is the bytes
//! of `i` as a little-endian u64) through the public store API, closes it,
//! and writes the marker file `D/index-bench.txt`. Every other step refuses a
//! directory without the marker, so the tool never touches a real store.
//!
//! Each step prints `key=value` lines on stdout, one key per line, in a fixed
//! order. The keys are a stable contract: later phases compare against them.
//!
//! `run` starts this binary once per step. Each RSS reading must come from a
//! fresh process: the peak RSS (`ru_maxrss`) only goes up, and the creator
//! process already holds the full index. `run` never prints the D path, so
//! its tables can go into public docs as they are.

mod backend;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use backend::{backend_by_name, Backend, CreateOpts, Scope, SAVED_DIR};
use chunk_store::{LiveFilter, LiveSet, ObjectId, DEFAULT_HASHER};

const USAGE: &str = "usage: index-bench create|open|lookup|settle|recover|gc|run --dir D [--backend NAME] [step flags] (see the doc comment of src/main.rs)";

/// The marker file of a store made by this tool: the output of `create`.
const MARKER: &str = "index-bench.txt";

// ── Errors and arguments ─────────────────────────────────────────────────────

/// A failure: bad arguments (exit 2, with the usage line) or a failed step
/// (exit 1).
enum Fail {
    Usage(String),
    Run(String),
}

impl From<String> for Fail {
    fn from(m: String) -> Self {
        Fail::Run(m)
    }
}

impl From<&str> for Fail {
    fn from(m: &str) -> Self {
        Fail::Run(m.to_string())
    }
}

/// The flags of one subcommand.
struct Flags {
    vals: HashMap<String, String>,
    bools: HashSet<String>,
}

/// The value flags and the flags without a value of each subcommand.
fn spec(cmd: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match cmd {
        "create" => (&["dir", "backend", "objects", "pack-limit", "memtable-limit"], &[]),
        "open" => (&["dir", "backend", "cache"], &[]),
        "lookup" => (&["dir", "backend", "hits", "misses", "seed"], &[]),
        "settle" => (&["dir", "backend", "timeout-s"], &[]),
        "recover" => (&["dir", "backend", "packs"], &["read-only"]),
        "gc" => (&["dir", "backend", "live-every", "dead-run"], &[]),
        "rewrite" => (&["dir", "backend", "objects", "rewrite", "pack-limit", "cache", "seed"], &[]),
        "run" => (
            &["dir", "backend", "objects", "reps", "pack-limit", "memtable-limit", "hits", "misses"],
            &["drop-caches"],
        ),
        _ => return None,
    })
}

fn parse(args: &[String]) -> Result<(String, Flags), Fail> {
    let Some(cmd) = args.first() else { return Err(Fail::Usage("no subcommand".into())) };
    let Some((vals, bools)) = spec(cmd) else { return Err(Fail::Usage(format!("unknown subcommand {cmd}"))) };
    let mut flags = Flags { vals: HashMap::new(), bools: HashSet::new() };
    let mut rest = args[1..].iter();
    while let Some(a) = rest.next() {
        let Some(name) = a.strip_prefix("--") else { return Err(Fail::Usage(format!("unexpected argument {a}"))) };
        if bools.contains(&name) {
            flags.bools.insert(name.to_string());
        } else if vals.contains(&name) {
            let Some(v) = rest.next() else { return Err(Fail::Usage(format!("--{name} needs a value"))) };
            flags.vals.insert(name.to_string(), v.clone());
        } else {
            return Err(Fail::Usage(format!("unknown flag --{name} for {cmd}")));
        }
    }
    Ok((cmd.clone(), flags))
}

impl Flags {
    fn get(&self, k: &str) -> Option<&str> {
        self.vals.get(k).map(String::as_str)
    }

    fn has(&self, k: &str) -> bool {
        self.bools.contains(k)
    }

    fn dir(&self) -> Result<PathBuf, Fail> {
        self.get("dir").map(PathBuf::from).ok_or_else(|| Fail::Usage("--dir is required".into()))
    }

    fn u64(&self, k: &str) -> Result<Option<u64>, Fail> {
        self.get(k)
            .map(|v| v.parse::<u64>().map_err(|_| Fail::Usage(format!("--{k} needs a whole number, not {v}"))))
            .transpose()
    }

    fn required_u64(&self, k: &str) -> Result<u64, Fail> {
        self.u64(k)?.ok_or_else(|| Fail::Usage(format!("--{k} is required")))
    }

    fn backend(&self) -> Result<Box<dyn Backend>, Fail> {
        let name = self.get("backend").unwrap_or("chunk-store");
        backend_by_name(name).ok_or_else(|| Fail::Usage(format!("unknown backend {name}")))
    }
}

// ── Measurement helpers ──────────────────────────────────────────────────────

/// User + system CPU seconds of this process.
fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(ru.ru_utime) + t(ru.ru_stime)
}

/// Peak RSS of this process so far, in bytes. `ru_maxrss` is in bytes on
/// macOS and in KiB on Linux.
fn max_rss_bytes() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    (ru.ru_maxrss.max(0) as u64) * unit
}

/// Current RSS of this process, in bytes (`task_info` on macOS).
#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc points mach_task_self to the mach2 crate; no new crate here
fn current_rss_bytes() -> Option<u64> {
    let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    let kr = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut libc::mach_task_basic_info as libc::task_info_t,
            &mut count,
        )
    };
    (kr == libc::KERN_SUCCESS).then_some(info.resident_size)
}

/// Current RSS of this process, in bytes (`/proc/self/statm` on Linux).
#[cfg(not(target_os = "macos"))]
fn current_rss_bytes() -> Option<u64> {
    if cfg!(target_os = "linux") {
        linux_rss_bytes()
    } else {
        None
    }
}

/// The resident pages of this process in two parts, in bytes: anonymous
/// pages (heap, stacks) and file-backed pages (mapped files, the binary).
///
/// macOS: the `internal` and `external` fields of `task_info` with the
/// `TASK_VM_INFO` flavor. `internal` counts resident anonymous pages (not
/// the compressed ones); `external` counts resident pages of mapped files.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc points mach_task_self to the mach2 crate; no new crate here
fn rss_split_bytes() -> Option<(u64, u64)> {
    /// `struct task_vm_info` of `<mach/task_info.h>` up to `phys_footprint`
    /// (revision 1), under `#pragma pack(4)`. libc does not define it.
    #[repr(C, packed(4))]
    #[derive(Default)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }
    const TASK_VM_INFO: libc::task_flavor_t = 22;
    const COUNT: libc::mach_msg_type_number_t =
        (std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<libc::natural_t>()) as _;
    let mut info = TaskVmInfo::default();
    let mut count = COUNT;
    let kr = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            TASK_VM_INFO,
            &mut info as *mut TaskVmInfo as libc::task_info_t,
            &mut count,
        )
    };
    (kr == libc::KERN_SUCCESS).then_some((info.internal, info.external))
}

/// The resident pages of this process in two parts, in bytes: `RssAnon` and
/// `RssFile` of `/proc/self/status` on Linux.
#[cfg(not(target_os = "macos"))]
fn rss_split_bytes() -> Option<(u64, u64)> {
    if cfg!(target_os = "linux") {
        linux_rss_split_bytes()
    } else {
        None
    }
}

/// `RssAnon` and `RssFile` of `/proc/self/status` (in kB there). Compiled on
/// every system, so that a build on macOS checks it too.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn linux_rss_split_bytes() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = |key: &str| -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with(key))?;
        line[key.len()..].trim().trim_end_matches("kB").trim().parse::<u64>().ok().map(|v| v * 1024)
    };
    Some((kb("RssAnon:")?, kb("RssFile:")?))
}

/// Resident pages (second field of `/proc/self/statm`) times the page size.
/// Compiled on every system, so that a build on macOS checks it too.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn linux_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page > 0).then(|| pages * page as u64)
}

/// Drop the page cache before a cold open: Linux only, as root.
fn drop_caches() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err(drop_caches_unsupported());
    }
    unsafe { libc::sync() };
    std::fs::write("/proc/sys/vm/drop_caches", "3").map_err(|e| {
        format!("cannot write /proc/sys/vm/drop_caches ({e}); --drop-caches needs a privileged container and root")
    })
}

fn drop_caches_unsupported() -> String {
    "--drop-caches works only on Linux. On macOS, run `sudo purge` by hand, then run the open step with --cache cold"
        .to_string()
}

/// The bytes of synthetic object `i`.
fn object(i: u64) -> [u8; 8] {
    i.to_le_bytes()
}

/// The ID of synthetic object `i`. A content ID, so it is the same for every
/// backend.
fn object_id(i: u64) -> ObjectId {
    DEFAULT_HASHER.id(&object(i))
}

/// The next value of a splitmix64 sequence (a small PRNG; std only).
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ── Output ───────────────────────────────────────────────────────────────────

/// A float with up to 3 decimals; `n/a` when it is not finite.
fn fmt_f(x: f64) -> String {
    if !x.is_finite() {
        return "n/a".into();
    }
    let s = format!("{x:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

/// `key=value` lines.
fn kv_text(kv: &[(&str, String)]) -> String {
    kv.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

/// `key=value` lines to a map.
fn parse_kv(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Print the output of a step and exit at once, without a drop of the store:
/// a drop of a 100M-entry map takes seconds and is not measured.
fn finish(text: &str) -> ! {
    print!("{text}");
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

// ── Marker ───────────────────────────────────────────────────────────────────

/// The marker of an index-bench store made with this backend.
fn read_marker(dir: &Path, backend: &dyn Backend) -> Result<HashMap<String, String>, String> {
    let not_a_store = || "the directory is not an index-bench store (no index-bench.txt from the create step)".to_string();
    let text = std::fs::read_to_string(dir.join(MARKER)).map_err(|_| not_a_store())?;
    let kv = parse_kv(&text);
    if kv.get("step").map(String::as_str) != Some("create") {
        return Err(not_a_store());
    }
    match kv.get("backend") {
        Some(b) if b == backend.name() => {}
        Some(b) => {
            return Err(format!(
                "the directory is not an index-bench store of backend {} (its marker says backend={b})",
                backend.name()
            ))
        }
        None => return Err(not_a_store()),
    }
    if kv.contains_key(GC_DONE) {
        return Err("the gc step changed this store (gc_done=1 in its marker), so it no longer has the objects \
                    of the create step; make a new store"
            .into());
    }
    Ok(kv)
}

/// The marker key that the gc step adds before it changes the store.
const GC_DONE: &str = "gc_done";

/// Sum of the file sizes under `dir`, recursively; 0 if `dir` does not
/// exist. With `store`, `dir` is a store directory: the marker file and the
/// saved-files directory of `recover` at its top are not counted, and every
/// other file is (packs, `.idx` files and the index directory).
fn dir_bytes(dir: &Path, store: bool) -> Result<u64, String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("cannot list the store files: {e}")),
    };
    let mut sum = 0;
    for e in rd {
        let e = e.map_err(|e| format!("cannot list the store files: {e}"))?;
        let name = e.file_name();
        if store && (name == MARKER || name == SAVED_DIR) {
            continue;
        }
        let m = e.metadata().map_err(|e| format!("cannot read a file size: {e}"))?;
        sum += if m.is_dir() { dir_bytes(&e.path(), false)? } else { m.len() };
    }
    Ok(sum)
}

/// The object count N of a marker.
fn marker_objects(kv: &HashMap<String, String>) -> Result<u64, String> {
    kv.get("objects").and_then(|v| v.parse().ok()).ok_or_else(|| "the marker has no objects= line".to_string())
}

// ── Steps ────────────────────────────────────────────────────────────────────

fn step_create(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let n = f.required_u64("objects")?;
    let opts = CreateOpts {
        pack_limit: f.u64("pack-limit")?,
        memtable_limit: f.u64("memtable-limit")?.map(|v| v as usize),
        locality_cache: None,
    };
    if let Ok(mut rd) = std::fs::read_dir(&dir) {
        if rd.next().is_some() {
            return Err("the directory is not empty; create needs a new or empty directory".into());
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make the directory: {e}"))?;

    let t0 = Instant::now();
    let c0 = cpu_secs();
    let store = backend.create(&dir, opts)?;
    for i in 0..n {
        store.put(&object(i))?;
        if i > 0 && i % 10_000_000 == 0 {
            eprintln!("create: {i} objects");
        }
    }
    store.close()?;
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - c0;
    let objects = store.objects();
    if objects != n {
        return Err(format!("the store has {objects} objects after create, not {n}").into());
    }
    let disk = dir_bytes(&dir, true)?;
    let index = dir_bytes(&dir.join("index"), false)?;
    let per_obj = if n > 0 { disk as f64 / n as f64 } else { f64::NAN };
    let index_per_obj = if n > 0 { index as f64 / n as f64 } else { f64::NAN };
    let ik = store.index_keys();
    let key = |k: &str| ik.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_else(|| "n/a".into());
    let merge_bytes = key("index_merge_bytes");
    let merge_per_index = match merge_bytes.parse::<f64>() {
        Ok(m) if index > 0 => fmt_f(m / index as f64),
        _ => "n/a".into(),
    };
    let text = kv_text(&[
        ("step", "create".into()),
        ("backend", backend.name().into()),
        ("objects", n.to_string()),
        ("packs", store.packs().to_string()),
        ("disk_bytes", disk.to_string()),
        ("disk_bytes_per_object", fmt_f(per_obj)),
        ("wall_s", fmt_f(wall)),
        ("cpu_s", fmt_f(cpu)),
        ("puts_per_s", fmt_f(n as f64 / wall)),
        ("max_rss_bytes", max_rss_bytes().to_string()),
        ("index_bytes", index.to_string()),
        ("index_bytes_per_object", fmt_f(index_per_obj)),
        ("index_runs", key("index_runs")),
        ("merge_bytes", merge_bytes),
        ("merge_bytes_per_index_byte", merge_per_index),
    ]);
    std::fs::write(dir.join(MARKER), &text).map_err(|e| format!("cannot write the marker file: {e}"))?;
    finish(&text);
}

fn step_open(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let cache = f.get("cache").unwrap_or("warm");
    if cache != "warm" && cache != "cold" {
        return Err(Fail::Usage(format!("--cache is warm or cold, not {cache}")));
    }
    let n = marker_objects(&read_marker(&dir, &*backend)?)?;

    let split_before = rss_split_bytes();
    let rss_before = current_rss_bytes();
    let max_before = max_rss_bytes();
    let t0 = Instant::now();
    let c0 = cpu_secs();
    let store = backend.open(&dir, false)?;
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - c0;
    let max_after = max_rss_bytes();
    let rss_after = current_rss_bytes();
    let split_after = rss_split_bytes();

    let objects = store.objects();
    if objects != n {
        return Err(format!("the store has {objects} objects, the marker says {n}").into());
    }
    let per_obj = |b: u64| if objects > 0 { fmt_f(b as f64 / objects as f64) } else { "n/a".into() };
    let opt = |v: Option<u64>| v.map_or("n/a".to_string(), |v| v.to_string());
    let steady = match (rss_before, rss_after) {
        (Some(b), Some(a)) => per_obj(a.saturating_sub(b)),
        _ => "n/a".into(),
    };
    let mut kv = vec![
        ("step", "open".into()),
        ("backend", backend.name().into()),
        ("cache", cache.into()),
        ("objects", objects.to_string()),
        ("packs", store.packs().to_string()),
        ("wall_s", fmt_f(wall)),
        ("cpu_s", fmt_f(cpu)),
        ("max_rss_before_bytes", max_before.to_string()),
        ("max_rss_after_bytes", max_after.to_string()),
        ("rss_before_bytes", opt(rss_before)),
        ("rss_after_bytes", opt(rss_after)),
        ("peak_bytes_per_object", per_obj(max_after.saturating_sub(max_before))),
        ("steady_bytes_per_object", steady),
    ];
    kv.extend(store.index_keys());
    kv.extend(split_keys(OPEN_SPLIT, split_before, split_after, objects));
    finish(&kv_text(&kv));
}

/// The split keys of the open step: anonymous RSS before and after, file
/// RSS before and after, the growth of each per object.
const OPEN_SPLIT: [&str; 6] = [
    "rss_anon_before_bytes",
    "rss_anon_after_bytes",
    "rss_file_before_bytes",
    "rss_file_after_bytes",
    "steady_anon_bytes_per_object",
    "steady_file_bytes_per_object",
];

/// The split keys of the lookup step, around the timed lookups.
const LOOKUP_SPLIT: [&str; 6] = [
    "rss_anon_before_lookups_bytes",
    "rss_anon_after_lookups_bytes",
    "rss_file_before_lookups_bytes",
    "rss_file_after_lookups_bytes",
    "lookups_anon_bytes_per_object",
    "lookups_file_bytes_per_object",
];

/// The RSS split (anonymous and file-backed pages) before and after a part
/// of a step, and the growth of each part per object, under the names in
/// `keys`. `n/a` where the system gives no split.
fn split_keys(
    keys: [&'static str; 6],
    before: Option<(u64, u64)>,
    after: Option<(u64, u64)>,
    objects: u64,
) -> Vec<(&'static str, String)> {
    let opt = |v: Option<u64>| v.map_or("n/a".to_string(), |v| v.to_string());
    let per = |b: Option<u64>, a: Option<u64>| match (b, a) {
        (Some(b), Some(a)) if objects > 0 => fmt_f(a.saturating_sub(b) as f64 / objects as f64),
        _ => "n/a".into(),
    };
    let (ab, fb) = (before.map(|s| s.0), before.map(|s| s.1));
    let (aa, fa) = (after.map(|s| s.0), after.map(|s| s.1));
    vec![
        (keys[0], opt(ab)),
        (keys[1], opt(aa)),
        (keys[2], opt(fb)),
        (keys[3], opt(fa)),
        (keys[4], per(ab, aa)),
        (keys[5], per(fb, fa)),
    ]
}

/// The default seed of the lookup sample, so runs repeat.
const LOOKUP_SEED: u64 = 66;

fn step_lookup(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let n = marker_objects(&read_marker(&dir, &*backend)?)?;
    let default_k = n.min(1_000_000);
    let hits = f.u64("hits")?.unwrap_or(default_k);
    let misses = f.u64("misses")?.unwrap_or(default_k);
    let mut seed = f.u64("seed")?.unwrap_or(LOOKUP_SEED);
    if n == 0 && hits > 0 {
        return Err("the store has no objects, so there are no hits to look up".into());
    }

    // The sample, before any timing: hits are objects below N (with
    // replacement), misses are objects at N or above. The IDs are made here,
    // so hashing is not in the contains timing.
    let hit_vals: Vec<u64> = (0..hits).map(|_| splitmix64(&mut seed) % n).collect();
    let hit_ids: Vec<ObjectId> = hit_vals.iter().map(|&i| object_id(i)).collect();
    let miss_ids: Vec<ObjectId> = (0..misses).map(|_| object_id(n + (splitmix64(&mut seed) % (1 << 62)))).collect();

    let store = backend.open(&dir, false)?;
    let objects = store.objects();
    if objects != n {
        return Err(format!("the store has {objects} objects, the marker says {n}").into());
    }

    // One pass: the hits, the misses, then the dedup-hit path of put (its
    // time includes the hash of the data). Seconds of each.
    let pass = || -> Result<(f64, f64, f64), Fail> {
        let t0 = Instant::now();
        let found = hit_ids.iter().filter(|id| store.contains(id)).count() as u64;
        let hit_s = t0.elapsed().as_secs_f64();
        if found != hits {
            return Err(format!("contains found {found} of {hits} objects that are in the store").into());
        }
        let t0 = Instant::now();
        let found = miss_ids.iter().filter(|id| store.contains(id)).count() as u64;
        let miss_s = t0.elapsed().as_secs_f64();
        if found != 0 {
            return Err(format!("contains found {found} of {misses} objects that are not in the store").into());
        }
        let t0 = Instant::now();
        for &i in &hit_vals {
            store.put(&object(i))?;
        }
        let put_s = t0.elapsed().as_secs_f64();
        if store.objects() != n {
            return Err(format!("put of objects in the store changed the count from {n} to {}", store.objects()).into());
        }
        Ok((hit_s, miss_s, put_s))
    };
    let split_before = rss_split_bytes();
    let (hit_s, miss_s, put_s) = pass()?;
    let split_after = rss_split_bytes();
    // The second pass: the same sample, the pages now mapped.
    let (hit2_s, miss2_s, put2_s) = pass()?;

    let ns = |s: f64, k: u64| if k > 0 { fmt_f(s * 1e9 / k as f64) } else { "n/a".into() };
    let per_s = |s: f64, k: u64| if k > 0 && s > 0.0 { fmt_f(k as f64 / s) } else { "n/a".into() };
    let mut kv = vec![
        ("step", "lookup".into()),
        ("backend", backend.name().into()),
        ("objects", objects.to_string()),
        ("hits", hits.to_string()),
        ("misses", misses.to_string()),
        ("contains_hit_ns", ns(hit_s, hits)),
        ("contains_hit_per_s", per_s(hit_s, hits)),
        ("contains_miss_ns", ns(miss_s, misses)),
        ("contains_miss_per_s", per_s(miss_s, misses)),
        ("put_hit_ns", ns(put_s, hits)),
        ("put_hit_per_s", per_s(put_s, hits)),
    ];
    kv.extend(split_keys(LOOKUP_SPLIT, split_before, split_after, objects));
    let runs = store.index_keys().into_iter().find(|(k, _)| *k == "index_runs").map_or("n/a".into(), |(_, v)| v);
    kv.extend([
        ("contains_hit_ns_2", ns(hit2_s, hits)),
        ("contains_miss_ns_2", ns(miss2_s, misses)),
        ("put_hit_ns_2", ns(put2_s, hits)),
        ("index_runs", runs),
    ]);
    finish(&kv_text(&kv));
}

/// The default time limit of the settle step.
const SETTLE_TIMEOUT_S: u64 = 900;

fn step_settle(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let n = marker_objects(&read_marker(&dir, &*backend)?)?;
    let timeout = f.u64("timeout-s")?.unwrap_or(SETTLE_TIMEOUT_S);
    let index_before = dir_bytes(&dir.join("index"), false)?;
    let t0 = Instant::now();
    let store = backend.open_default(&dir)?;
    let key = |kv: &[(&'static str, String)], k: &str| kv.iter().find(|(n, _)| *n == k).map_or("n/a".into(), |(_, v)| v.clone());
    let before = store.index_keys();
    let settled = store.settle(std::time::Duration::from_secs(timeout))?;
    let wait = t0.elapsed().as_secs_f64();
    let after = store.index_keys();
    store.close()?;
    if store.objects() != n {
        return Err(format!("the store has {} objects after settle, not {n}", store.objects()).into());
    }
    let index_after = dir_bytes(&dir.join("index"), false)?;
    let merge_bytes = key(&after, "index_merge_bytes");
    let per = |b: u64| match merge_bytes.parse::<f64>() {
        Ok(m) if b > 0 => fmt_f(m / b as f64),
        _ => "n/a".into(),
    };
    let kv = vec![
        ("step", "settle".into()),
        ("backend", backend.name().into()),
        ("objects", n.to_string()),
        ("settled", u8::from(settled).to_string()),
        ("wait_s", fmt_f(wait)),
        ("index_runs_before", key(&before, "index_runs")),
        ("index_runs_after", key(&after, "index_runs")),
        ("index_bytes_before", index_before.to_string()),
        ("index_bytes_after", index_after.to_string()),
        ("merge_bytes", merge_bytes.clone()),
        ("merge_bytes_per_index_byte", per(index_before)),
    ];
    if !settled {
        return Err(format!("the index did not settle in {timeout} s").into());
    }
    finish(&kv_text(&kv));
}

/// `rewrite` (#112): the dedup hits of `put` when data that is in the
/// store is written again in the order of its packs (a copy of a tree that
/// is already in the store), against random dedup hits (no locality).
///
/// One process makes the store (N objects; it needs a new or empty
/// directory and writes no marker, so no other step uses it), waits for the
/// background merges, then puts K objects again: first K random objects
/// (the control), then the K objects from N/4 on in the order of their
/// first put. The locality cache uses only packs that its own process
/// sealed, so the store must be made in the same process.
fn step_rewrite(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let n = f.required_u64("objects")?;
    let k = f.u64("rewrite")?.unwrap_or(n.min(1_000_000) / 2).min(n / 2);
    let cache = match f.get("cache").unwrap_or("on") {
        "on" => true,
        "off" => false,
        v => return Err(Fail::Usage(format!("--cache needs on or off, not {v}"))),
    };
    let mut seed = f.u64("seed")?.unwrap_or(LOOKUP_SEED);
    if let Ok(mut rd) = std::fs::read_dir(&dir) {
        if rd.next().is_some() {
            return Err("the directory is not empty; rewrite needs a new or empty directory".into());
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make the directory: {e}"))?;
    let opts = CreateOpts { pack_limit: f.u64("pack-limit")?, memtable_limit: None, locality_cache: Some(cache) };
    let store = backend.create(&dir, opts)?;
    for i in 0..n {
        store.put(&object(i))?;
        if i > 0 && i % 10_000_000 == 0 {
            eprintln!("rewrite: {i} objects");
        }
    }
    store.wait_idle();
    let packs = store.packs();
    let random: Vec<u64> = (0..k).map(|_| splitmix64(&mut seed) % n).collect();
    let timed = |vals: &mut dyn Iterator<Item = u64>, cnt: u64| -> Result<(String, Keys), Fail> {
        let before = store.locality_keys();
        let t0 = Instant::now();
        for i in vals {
            store.put(&object(i))?;
        }
        let ns = if cnt > 0 { fmt_f(t0.elapsed().as_secs_f64() * 1e9 / cnt as f64) } else { "n/a".into() };
        let after = store.locality_keys();
        // The counters of this phase only (the gauges as they are after it).
        let diff = after
            .iter()
            .map(|(key, v)| {
                let old = before.iter().find(|(b, _)| b == key).map(|(_, o)| o.parse::<u64>().unwrap_or(0)).unwrap_or(0);
                let gauge = matches!(*key, "cache_entries" | "cache_packs");
                (*key, if gauge { v.clone() } else { (v.parse::<u64>().unwrap_or(0) - old).to_string() })
            })
            .collect();
        Ok((ns, diff))
    };
    let (random_ns, random_keys) = timed(&mut random.iter().copied(), k)?;
    let (ordered_ns, ordered_keys) = timed(&mut (n / 4..n / 4 + k), k)?;
    if store.objects() != n {
        return Err(format!("put of objects in the store changed the count from {n} to {}", store.objects()).into());
    }
    let mut kv = vec![
        ("step", "rewrite".into()),
        ("backend", backend.name().into()),
        ("objects", n.to_string()),
        ("packs", packs.to_string()),
        ("rewrite", k.to_string()),
        ("cache", if cache { "on" } else { "off" }.into()),
        ("put_random_hit_ns", random_ns),
        ("put_ordered_hit_ns", ordered_ns),
    ];
    let index_per_put = |keys: &[(&'static str, String)]| {
        let misses = keys.iter().find(|(key, _)| *key == "cache_misses").map(|(_, v)| v.parse::<f64>().unwrap_or(0.0));
        match misses {
            Some(m) if cache && k > 0 => format!("{:.4}", m / k as f64),
            _ => "1".into(),
        }
    };
    kv.push(("random_index_lookups_per_put", index_per_put(&random_keys)));
    kv.push(("ordered_index_lookups_per_put", index_per_put(&ordered_keys)));
    kv.extend(random_keys.into_iter().map(|(key, v)| (prefixed("random_", key), v)));
    kv.extend(ordered_keys.into_iter().map(|(key, v)| (prefixed("ordered_", key), v)));
    kv.push(("max_rss_bytes", max_rss_bytes().to_string()));
    store.close()?;
    finish(&kv_text(&kv));
}

/// `key=value` pairs of a step.
type Keys = Vec<(&'static str, String)>;

/// `prefix` + `key` as a static key (a handful per process).
fn prefixed(prefix: &str, key: &str) -> &'static str {
    Box::leak(format!("{prefix}{key}").into_boxed_str())
}

fn step_recover(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let (scope, scope_name) = match f.get("packs").unwrap_or("1") {
        "1" => (Scope::One, "one"),
        "all" => (Scope::All, "all"),
        other => return Err(Fail::Usage(format!("--packs is 1 or all, not {other}"))),
    };
    let read_only = f.has("read-only");
    let n = marker_objects(&read_marker(&dir, &*backend)?)?;

    // The high-water mark of an on-disk index, before any .idx moves (a
    // read-only open, not timed, writes nothing): the packs below it are in
    // the index runs, so the recovery open does not scan them. Read after
    // the timed open it could be later than the mark that the open used.
    let hwm_pack: Option<u64> = {
        let store = backend.open(&dir, true)?;
        let keys = store.index_keys();
        keys.iter().find(|(k, _)| *k == "hwm_pack").and_then(|(_, v)| v.parse().ok())
    };

    let dropped = backend.drop_index(&dir, scope)?;
    let t0 = Instant::now();
    let c0 = cpu_secs();
    let opened = backend.open(&dir, read_only);
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - c0;
    let checked = opened.and_then(|store| match store.objects() {
        o if o == n => Ok(store),
        o => Err(format!("the store has {o} objects after the recovery open, the marker says {n}")),
    });
    // Restore also when the open failed, so the store keeps its index files.
    // A store that was opened stays alive: it keeps no index file open.
    let restored = backend.restore_index(&dir, &dropped);
    let (store, restored) = match (checked, restored) {
        (Ok(store), Ok(r)) => (store, r),
        (checked, Err(e)) => {
            eprintln!("index-bench: the saved index files are in {}", dir.join(SAVED_DIR).display());
            return Err(match checked {
                Ok(_) => e.into(),
                Err(open_err) => format!("{open_err}; restore: {e}").into(),
            });
        }
        (Err(e), Ok(_)) => return Err(e.into()),
    };

    let t = &dropped.targets;
    let active = t.iter().filter(|t| t.active).count();
    // Only the packs at or above the mark are scanned.
    let scanned: Vec<_> = t.iter().filter(|t| hwm_pack.is_none_or(|h| t.num >= h)).collect();
    let covered = t.len() - scanned.len();
    finish(&kv_text(&[
        ("step", "recover".into()),
        ("backend", backend.name().into()),
        ("scope", scope_name.into()),
        ("mode", if read_only { "ro" } else { "rw" }.into()),
        ("packs_dropped", t.len().to_string()),
        ("sealed_packs", (t.len() - active).to_string()),
        ("active_packs", active.to_string()),
        ("scanned_bytes", scanned.iter().map(|t| t.pack_bytes).sum::<u64>().to_string()),
        ("scanned_objects", scanned.iter().map(|t| t.entries).sum::<u64>().to_string()),
        ("wall_s", fmt_f(wall)),
        ("cpu_s", fmt_f(cpu)),
        ("objects", store.objects().to_string()),
        ("idx_identical", if restored.rebuilt > 0 { "yes" } else { "n/a" }.into()),
        ("covered_packs", covered.to_string()),
        ("hwm_pack", hwm_pack.map_or("n/a".into(), |h| h.to_string())),
    ]));
}

/// The default length of a dead run of the gc step, in objects: every K-th
/// object is dead.
const DEAD_RUN: u64 = 1;

/// One GC of the store: object i is dead when (i / R) mod K is 0 (R =
/// `--dead-run`, K = `--live-every`), so (K-1)/K of the objects stay live
/// and the dead ones lie in runs of R objects. A full-size pack where at
/// least half of the bytes stay live gets holes only over whole 4 KiB
/// blocks of dead records, so in such a pack dead objects of 8 bytes that
/// lie one by one (R = 1) are not removed at all; use R = 1024 to measure
/// a GC that punches. (A small pack is compacted instead.)
///
/// The step changes the store: it adds `gc_done=1` to the marker before the
/// GC, and every step refuses such a store after that.
fn step_gc(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let k = f.u64("live-every")?.unwrap_or(4);
    let r = f.u64("dead-run")?.unwrap_or(DEAD_RUN);
    if k == 0 || r == 0 {
        return Err(Fail::Usage("--live-every and --dead-run must be at least 1".into()));
    }
    let n = marker_objects(&read_marker(&dir, &*backend)?)?;

    // The live set (the mark of a real GC), before the open.
    let t0 = Instant::now();
    let dead = |i: u64| (i / r).is_multiple_of(k);
    // A Bloom filter of the live IDs, as `metadata::gc::live_objects` makes
    // it (#113): sized for every object of the store.
    let mut live = LiveFilter::new(n);
    let mut dead_objects = 0;
    for i in 0..n {
        if dead(i) {
            dead_objects += 1;
        } else {
            live.insert(&object_id(i));
        }
    }
    let live_set_s = t0.elapsed().as_secs_f64();
    // The dead objects that the filter keeps (its false positives).
    let false_positives = (0..n).filter(|&i| dead(i) && live.may_contain(&object_id(i))).count();

    let store = backend.open(&dir, false)?;
    let marker = std::fs::read_to_string(dir.join(MARKER)).map_err(|e| format!("cannot read the marker file: {e}"))?;
    std::fs::write(dir.join(MARKER), format!("{marker}{GC_DONE}=1\n"))
        .map_err(|e| format!("cannot write the marker file: {e}"))?;
    let gc_keys = store.gc(&live)?;
    let objects_after = store.objects();
    let packs_after = store.packs();
    store.close()?;

    let mut kv = vec![
        ("step", "gc".into()),
        ("backend", backend.name().into()),
        ("objects", n.to_string()),
        ("live_every", k.to_string()),
        ("dead_run", r.to_string()),
        ("dead_objects", dead_objects.to_string()),
        ("live_set_s", fmt_f(live_set_s)),
    ];
    kv.extend(gc_keys);
    kv.extend([
        ("objects_after", objects_after.to_string()),
        ("packs_after", packs_after.to_string()),
        ("max_rss_bytes", max_rss_bytes().to_string()),
        ("live_set_bytes", live.bytes().to_string()),
        ("live_false_positives", false_positives.to_string()),
        ("dead_left", objects_after.saturating_sub(n - dead_objects).to_string()),
    ]);
    finish(&kv_text(&kv));
}

// ── run driver ───────────────────────────────────────────────────────────────

/// Start this binary for one step and return its stdout. Its stderr goes
/// through.
fn child(args: &[String]) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let out = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot start the {} step: {e}", args[0]))?;
    if !out.status.success() {
        return Err(format!("the {} step failed ({})", args[0], out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("the {} step printed bytes that are not UTF-8", args[0]))
}

/// The median of the values that parse as numbers; NaN when there are none.
fn median(mut v: Vec<f64>) -> f64 {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

/// One number from each step output.
fn column(runs: &[HashMap<String, String>], key: &str) -> Vec<f64> {
    runs.iter().map(|r| r.get(key).and_then(|v| v.parse().ok()).unwrap_or(f64::NAN)).collect()
}

fn os_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// 1M, 10M, 100M for whole millions, else the plain number.
fn objects_label(n: u64) -> String {
    if n > 0 && n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else {
        n.to_string()
    }
}

fn table(head: &[&str], rows: &[Vec<String>]) -> String {
    let mut s = format!("| {} |\n", head.join(" | "));
    s += &format!("|{}\n", "---|".repeat(head.len()));
    for r in rows {
        s += &format!("| {} |\n", r.join(" | "));
    }
    s
}

fn step_run(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let n = f.required_u64("objects")?;
    let reps = f.u64("reps")?.unwrap_or(3);
    if reps == 0 {
        return Err(Fail::Usage("--reps must be at least 1".into()));
    }
    let cold = f.has("drop-caches");
    if cold && !cfg!(target_os = "linux") {
        return Err(drop_caches_unsupported().into());
    }
    let dir_s = dir.to_str().ok_or("the --dir path is not UTF-8")?.to_string();
    let common = |step: &str| -> Vec<String> {
        vec![step.into(), "--dir".into(), dir_s.clone(), "--backend".into(), backend.name().into()]
    };

    let marker = if dir.join(MARKER).exists() {
        let m = read_marker(&dir, &*backend)?;
        let have = marker_objects(&m)?;
        if have != n {
            return Err(format!("the store has {have} objects, not --objects {n}; use another --dir").into());
        }
        println!("# create (the store exists; its marker)");
        print!("{}", std::fs::read_to_string(dir.join(MARKER)).map_err(|e| e.to_string())?);
        m
    } else {
        let mut args = common("create");
        args.extend(["--objects".into(), n.to_string()]);
        for k in ["pack-limit", "memtable-limit"] {
            if let Some(v) = f.get(k) {
                args.extend([format!("--{k}"), v.into()]);
            }
        }
        let out = child(&args)?;
        println!("# create");
        print!("{out}");
        parse_kv(&out)
    };

    println!("# open warm-up (not in the summary)");
    print!("{}", child(&common("open"))?);
    let mut opens = Vec::new();
    for rep in 1..=reps {
        let mut args = common("open");
        if cold {
            drop_caches()?;
            args.extend(["--cache".into(), "cold".into()]);
        }
        let out = child(&args)?;
        println!("# open rep {rep}");
        print!("{out}");
        opens.push(parse_kv(&out));
    }

    let mut args = common("lookup");
    for k in ["hits", "misses"] {
        if let Some(v) = f.get(k) {
            args.extend([format!("--{k}"), v.into()]);
        }
    }
    let out = child(&args)?;
    println!("# lookup");
    print!("{out}");
    let lookup = parse_kv(&out);

    // Recovery: R runs of each scope and mode.
    let mut recovers = Vec::new();
    for (packs, scope) in [("1", "one"), ("all", "all")] {
        for read_only in [false, true] {
            let mode = if read_only { "ro" } else { "rw" };
            let mut runs = Vec::new();
            for rep in 1..=reps {
                let mut args = common("recover");
                args.extend(["--packs".into(), packs.into()]);
                if read_only {
                    args.push("--read-only".into());
                }
                if cold {
                    drop_caches()?;
                }
                let out = child(&args)?;
                println!("# recover {scope} {mode} rep {rep}");
                print!("{out}");
                runs.push(parse_kv(&out));
            }
            recovers.push((scope, mode, runs));
        }
    }
    let out = child(&common("open"))?;
    println!("# open check (not in the summary)");
    print!("{out}");
    let have = parse_kv(&out).get("objects").cloned().unwrap_or_default();
    if have != n.to_string() {
        return Err(format!("the open check after the recovery runs found {have} objects, not {n}").into());
    }

    let os = os_label().to_string();
    let label = objects_label(n);
    let get = |k: &str| marker.get(k).cloned().unwrap_or_else(|| "n/a".into());
    let creator_mb = marker.get("max_rss_bytes").and_then(|v| v.parse::<f64>().ok()).map_or(f64::NAN, |b| b / 1e6);
    println!();
    println!("## Store");
    print!(
        "{}",
        table(
            &["OS", "Objects", "Packs", "Disk B/object", "Create puts/s", "Creator peak RSS MB", "Index B/object"],
            &[vec![
                os.clone(),
                label.clone(),
                get("packs"),
                get("disk_bytes_per_object"),
                get("puts_per_s"),
                fmt_f(creator_mb),
                get("index_bytes_per_object"),
            ]],
        )
    );
    let walls = column(&opens, "wall_s");
    let lo = walls.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = walls.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let open_median = median(walls.clone());
    let open_cpu = median(column(&opens, "cpu_s"));
    println!();
    println!("## Open");
    print!(
        "{}",
        table(
            &[
                "OS",
                "Objects",
                "Open s (median)",
                "Open s (min-max)",
                "Open CPU s (median)",
                "Peak RSS B/object (median)",
                "Steady RSS B/object (median)",
                "Cache",
                "Steady anon B/object (median)",
                "Steady file B/object (median)",
                "Index runs",
            ],
            &[vec![
                os.clone(),
                label.clone(),
                fmt_f(open_median),
                format!("{}-{}", fmt_f(lo), fmt_f(hi)),
                fmt_f(open_cpu),
                fmt_f(median(column(&opens, "peak_bytes_per_object"))),
                fmt_f(median(column(&opens, "steady_bytes_per_object"))),
                if cold { "cold".into() } else { "warm".into() },
                fmt_f(median(column(&opens, "steady_anon_bytes_per_object"))),
                fmt_f(median(column(&opens, "steady_file_bytes_per_object"))),
                opens.last().and_then(|o| o.get("index_runs")).cloned().unwrap_or_else(|| "n/a".into()),
            ]],
        )
    );
    let lk = |k: &str| lookup.get(k).cloned().unwrap_or_else(|| "n/a".into());
    println!();
    println!("## Lookups");
    print!(
        "{}",
        table(
            &[
                "OS",
                "Objects",
                "contains hit ns",
                "contains miss ns",
                "put dedup-hit ns",
                "contains hit/s",
                "contains miss/s",
                "Index runs",
                "Warm hit ns",
                "Warm miss ns",
                "Warm put dedup-hit ns",
            ],
            &[vec![
                os.clone(),
                label.clone(),
                lk("contains_hit_ns"),
                lk("contains_miss_ns"),
                lk("put_hit_ns"),
                lk("contains_hit_per_s"),
                lk("contains_miss_per_s"),
                lk("index_runs"),
                lk("contains_hit_ns_2"),
                lk("contains_miss_ns_2"),
                lk("put_hit_ns_2"),
            ]],
        )
    );
    let mut rows = Vec::new();
    for (scope, mode, runs) in &recovers {
        let first = |k: &str| runs[0].get(k).cloned().unwrap_or_else(|| "n/a".into());
        let num = |k: &str| runs[0].get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(f64::NAN);
        let rec = median(column(runs, "wall_s"));
        let extra = rec - open_median;
        let rate = |amount: f64| if extra > 0.0 { fmt_f(amount / extra) } else { "n/a".into() };
        // The same from CPU time, which a loaded host disturbs less.
        let extra_cpu = median(column(runs, "cpu_s")) - open_cpu;
        let cpu_rate = if extra_cpu > 0.0 && num("scanned_bytes") > 0.0 {
            fmt_f(num("scanned_bytes") / 1e6 / extra_cpu)
        } else {
            "n/a".into()
        };
        rows.push(vec![
            os.clone(),
            label.clone(),
            scope.to_string(),
            mode.to_string(),
            format!("{}/{}", first("sealed_packs"), first("active_packs")),
            fmt_f(num("scanned_bytes") / 1e6),
            first("scanned_objects"),
            fmt_f(rec),
            fmt_f(extra),
            rate(num("scanned_bytes") / 1e6),
            rate(num("scanned_objects")),
            first("covered_packs"),
            fmt_f(extra_cpu),
            cpu_rate,
        ]);
    }
    println!();
    println!("## Recovery");
    print!(
        "{}",
        table(
            &[
                "OS",
                "Objects",
                "Scope",
                "Mode",
                "Sealed/active packs",
                "Scanned MB",
                "Scanned objects",
                "Recovery open s (median)",
                "Extra s",
                "Scan MB/s",
                "Scan objects/s",
                "Covered packs",
                "Extra CPU s",
                "Scan MB/s (CPU)",
            ],
            &rows,
        )
    );
    let _ = std::io::stdout().flush();
    Ok(())
}

fn real_main(args: &[String]) -> Result<(), Fail> {
    let (cmd, flags) = parse(args)?;
    match cmd.as_str() {
        "create" => step_create(&flags),
        "open" => step_open(&flags),
        "lookup" => step_lookup(&flags),
        "settle" => step_settle(&flags),
        "recover" => step_recover(&flags),
        "gc" => step_gc(&flags),
        "rewrite" => step_rewrite(&flags),
        "run" => step_run(&flags),
        _ => Err(Fail::Usage(format!("unknown subcommand {cmd}"))),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match real_main(&args) {
        Ok(()) => 0,
        Err(Fail::Usage(m)) => {
            eprintln!("index-bench: {m}");
            eprintln!("{USAGE}");
            2
        }
        Err(Fail::Run(m)) => {
            eprintln!("index-bench: {m}");
            1
        }
    };
    std::process::exit(code);
}
