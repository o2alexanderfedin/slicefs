//! Measure the chunk store index (#66): open time, RAM per object, lookups
//! and recovery, on stores of many small synthetic objects.
//!
//! ```text
//! index-bench create --dir D --objects N [--pack-limit BYTES]
//! index-bench open   --dir D [--cache warm|cold]
//! index-bench run    --dir D --objects N [--reps R] [--pack-limit BYTES] [--drop-caches]
//!   every step also takes --backend NAME (default chunk-store)
//! ```
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

use backend::{backend_by_name, Backend};

const USAGE: &str = "usage: index-bench create|open|run --dir D [--backend NAME] [step flags] (see the doc comment of src/main.rs)";

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
        "create" => (&["dir", "backend", "objects", "pack-limit"], &[]),
        "open" => (&["dir", "backend", "cache"], &[]),
        "run" => (&["dir", "backend", "objects", "reps", "pack-limit"], &["drop-caches"]),
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
        Some(b) if b == backend.name() => Ok(kv),
        Some(b) => Err(format!(
            "the directory is not an index-bench store of backend {} (its marker says backend={b})",
            backend.name()
        )),
        None => Err(not_a_store()),
    }
}

/// The object count N of a marker.
fn marker_objects(kv: &HashMap<String, String>) -> Result<u64, String> {
    kv.get("objects").and_then(|v| v.parse().ok()).ok_or_else(|| "the marker has no objects= line".to_string())
}

/// Sum of the file sizes in `dir`.
fn dir_bytes(dir: &Path) -> Result<u64, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot list the store files: {e}"))?;
    let mut sum = 0;
    for e in rd {
        let e = e.map_err(|e| format!("cannot list the store files: {e}"))?;
        sum += e.metadata().map_err(|e| format!("cannot read a file size: {e}"))?.len();
    }
    Ok(sum)
}

// ── Steps ────────────────────────────────────────────────────────────────────

fn step_create(f: &Flags) -> Result<(), Fail> {
    let dir = f.dir()?;
    let backend = f.backend()?;
    let n = f.required_u64("objects")?;
    let pack_limit = f.u64("pack-limit")?;
    if let Ok(mut rd) = std::fs::read_dir(&dir) {
        if rd.next().is_some() {
            return Err("the directory is not empty; create needs a new or empty directory".into());
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make the directory: {e}"))?;

    let t0 = Instant::now();
    let c0 = cpu_secs();
    let store = backend.create(&dir, pack_limit)?;
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
    let disk = dir_bytes(&dir.join("packs"))?;
    let per_obj = if n > 0 { disk as f64 / n as f64 } else { f64::NAN };
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

    let rss_before = current_rss_bytes();
    let max_before = max_rss_bytes();
    let t0 = Instant::now();
    let c0 = cpu_secs();
    let store = backend.open(&dir, false)?;
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - c0;
    let max_after = max_rss_bytes();
    let rss_after = current_rss_bytes();

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
    finish(&kv_text(&[
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
    ]));
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
        if let Some(limit) = f.get("pack-limit") {
            args.extend(["--pack-limit".into(), limit.into()]);
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

    let os = os_label().to_string();
    let label = objects_label(n);
    let get = |k: &str| marker.get(k).cloned().unwrap_or_else(|| "n/a".into());
    let creator_mb = marker.get("max_rss_bytes").and_then(|v| v.parse::<f64>().ok()).map_or(f64::NAN, |b| b / 1e6);
    println!();
    println!("## Store");
    print!(
        "{}",
        table(
            &["OS", "Objects", "Packs", "Disk B/object", "Create puts/s", "Creator peak RSS MB"],
            &[vec![
                os.clone(),
                label.clone(),
                get("packs"),
                get("disk_bytes_per_object"),
                get("puts_per_s"),
                fmt_f(creator_mb),
            ]],
        )
    );
    let walls = column(&opens, "wall_s");
    let lo = walls.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = walls.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let open_median = median(walls.clone());
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
            ],
            &[vec![
                os.clone(),
                label.clone(),
                fmt_f(open_median),
                format!("{}-{}", fmt_f(lo), fmt_f(hi)),
                fmt_f(median(column(&opens, "cpu_s"))),
                fmt_f(median(column(&opens, "peak_bytes_per_object"))),
                fmt_f(median(column(&opens, "steady_bytes_per_object"))),
                if cold { "cold".into() } else { "warm".into() },
            ]],
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
