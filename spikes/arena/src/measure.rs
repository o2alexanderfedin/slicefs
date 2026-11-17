//! Measurement protocol (plan section 8): one process per run, getrusage
//! deltas, percentiles, load average, round-robin interleave of variants,
//! one flat JSON line per run.

use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub user_us: u64,
    pub sys_us: u64,
    pub minflt: u64,
    pub majflt: u64,
}

impl Usage {
    /// `getrusage(RUSAGE_SELF)` (macOS has no RUSAGE_THREAD).
    pub fn now() -> Usage {
        // SAFETY: zeroed rusage is a valid out buffer.
        let mut r: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
        let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
        Usage { user_us: us(r.ru_utime), sys_us: us(r.ru_stime), minflt: r.ru_minflt as u64, majflt: r.ru_majflt as u64 }
    }
}

/// A measured region: wall clock and rusage.
pub struct Span {
    t0: Instant,
    u0: Usage,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Measured {
    pub wall_s: f64,
    pub user_s: f64,
    pub sys_s: f64,
    pub minflt: u64,
    pub majflt: u64,
}

impl Measured {
    pub fn cpu_s(&self) -> f64 {
        self.user_s + self.sys_s
    }
}

impl Span {
    pub fn start() -> Span {
        let u0 = Usage::now();
        Span { t0: Instant::now(), u0 }
    }

    pub fn end(&self) -> Measured {
        let wall = self.t0.elapsed().as_secs_f64();
        let u = Usage::now();
        Measured {
            wall_s: wall,
            user_s: (u.user_us - self.u0.user_us) as f64 / 1e6,
            sys_s: (u.sys_us - self.u0.sys_us) as f64 / 1e6,
            minflt: u.minflt - self.u0.minflt,
            majflt: u.majflt - self.u0.majflt,
        }
    }
}

/// Percentile `p` (0..=100) of `v` (sorted in place), nearest rank.
pub fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

pub fn loadavg() -> f64 {
    let mut l = [0f64; 3];
    // SAFETY: valid buffer of 3 doubles.
    unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    l[0]
}

pub fn git_sha() -> String {
    if let Ok(s) = std::env::var("ARENA_GIT_SHA") {
        return s;
    }
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A flat JSON object, written as one line.
#[derive(Clone, Debug, Default)]
pub struct Rec {
    fields: Vec<(String, Val)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    S(String),
    N(f64),
}

impl Rec {
    pub fn new(bench: &str, variant: &str) -> Rec {
        let mut r = Rec::default();
        r.s("bench", bench).s("variant", variant).s("git", &git_sha()).n("load1", loadavg());
        r
    }

    pub fn s(&mut self, k: &str, v: &str) -> &mut Self {
        self.fields.push((k.into(), Val::S(v.into())));
        self
    }

    pub fn n(&mut self, k: &str, v: f64) -> &mut Self {
        self.fields.push((k.into(), Val::N(v)));
        self
    }

    pub fn m(&mut self, m: &Measured) -> &mut Self {
        self.n("wall_s", m.wall_s)
            .n("user_s", m.user_s)
            .n("sys_s", m.sys_s)
            .n("cpu_s", m.cpu_s())
            .n("minflt", m.minflt as f64)
            .n("majflt", m.majflt as f64)
    }

    pub fn get(&self, k: &str) -> Option<&Val> {
        self.fields.iter().find(|(n, _)| n == k).map(|(_, v)| v)
    }

    pub fn num(&self, k: &str) -> Option<f64> {
        match self.get(k) {
            Some(Val::N(x)) => Some(*x),
            _ => None,
        }
    }

    pub fn str(&self, k: &str) -> Option<&str> {
        match self.get(k) {
            Some(Val::S(x)) => Some(x),
            _ => None,
        }
    }

    pub fn fields(&self) -> &[(String, Val)] {
        &self.fields
    }

    pub fn to_json(&self) -> String {
        let mut o = String::from("{");
        for (i, (k, v)) in self.fields.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            let _ = write!(o, "\"{}\":", esc(k));
            match v {
                Val::S(s) => {
                    let _ = write!(o, "\"{}\"", esc(s));
                }
                Val::N(x) if x.is_finite() => {
                    let _ = write!(o, "{x}");
                }
                Val::N(_) => o.push_str("null"),
            }
        }
        o.push('}');
        o
    }

    /// Print as one JSON line on stdout (the driver collects it).
    pub fn emit(&self) {
        println!("{}", self.to_json());
    }

    /// Parse a line written by `to_json`.
    pub fn parse(line: &str) -> Option<Rec> {
        let b = line.trim().as_bytes();
        if b.first() != Some(&b'{') || b.last() != Some(&b'}') {
            return None;
        }
        let mut i = 1;
        let mut r = Rec::default();
        let s = |i: &mut usize| -> Option<String> {
            if b.get(*i) != Some(&b'"') {
                return None;
            }
            *i += 1;
            let mut out = String::new();
            while *i < b.len() && b[*i] != b'"' {
                if b[*i] == b'\\' {
                    *i += 1;
                }
                out.push(b[*i] as char);
                *i += 1;
            }
            *i += 1;
            Some(out)
        };
        while i < b.len() - 1 {
            let k = s(&mut i)?;
            if b.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            let v = if b.get(i) == Some(&b'"') {
                Val::S(s(&mut i)?)
            } else {
                let st = i;
                while i < b.len() - 1 && b[i] != b',' {
                    i += 1;
                }
                let t = std::str::from_utf8(&b[st..i]).ok()?;
                Val::N(if t == "null" { f64::NAN } else { t.parse().ok()? })
            };
            r.fields.push((k, v));
            if b.get(i) == Some(&b',') {
                i += 1;
            }
        }
        Some(r)
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Run `bench` once per (repeat, variant) in a child process of this
/// executable (`<exe> one <bench> --variant V --rep R <extra>`), variants
/// round-robin with the start variant rotated per repeat. Every JSON line a
/// child prints is appended to `out` and echoed. Returns the records.
pub fn interleave(bench: &str, variants: &[String], reps: usize, extra: &[String], out: &Path) -> io::Result<Vec<Rec>> {
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p)?;
    }
    let exe = std::env::current_exe()?;
    let sha = git_sha();
    let mut all = Vec::new();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(out)?;
    for rep in 0..reps {
        for k in 0..variants.len() {
            let v = &variants[(k + rep) % variants.len()];
            let mut cmd = Command::new(&exe);
            cmd.arg("one").arg(bench).arg("--variant").arg(v).arg("--rep").arg(rep.to_string()).args(extra);
            cmd.env("ARENA_GIT_SHA", &sha);
            let o = cmd.output()?;
            if !o.status.success() {
                eprintln!("run failed: {bench} {v} rep {rep}: {}", String::from_utf8_lossy(&o.stderr));
                continue;
            }
            for line in o.stdout.lines() {
                let line = line?;
                if let Some(r) = Rec::parse(&line) {
                    writeln!(file, "{line}")?;
                    println!("{line}");
                    all.push(r);
                }
            }
            io::stderr().write_all(&o.stderr)?;
        }
    }
    Ok(all)
}

/// Read every record of a JSONL file.
pub fn load(path: &Path) -> io::Result<Vec<Rec>> {
    let f = std::fs::File::open(path)?;
    Ok(io::BufReader::new(f).lines().map_while(|l| l.ok()).filter_map(|l| Rec::parse(&l)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip() {
        let mut r = Rec::default();
        r.s("a", "x\"y").n("b", 1.5).n("c", 3.0);
        let p = Rec::parse(&r.to_json()).unwrap();
        assert_eq!(p.str("a"), Some("x\"y"));
        assert_eq!(p.num("b"), Some(1.5));
        assert_eq!(p.num("c"), Some(3.0));
    }
}
