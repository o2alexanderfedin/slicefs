//! JSONL -> markdown tables: median [min..max] per (bench, cfg, variant)
//! for each numeric field, and the delta of the median against a baseline
//! variant. `summarize noise <file>`: noise floor N (plan section 8) from
//! runs of one variant, split into two halves in run order.
//!
//! `summarize table <file> [--base V0] [--fields a,b,c]`

use arena_spike::measure::{load, median, Rec, Val};
use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = arena_spike::bench::Args::parse(&argv);
    match argv.first().map(String::as_str) {
        Some("noise") => noise(Path::new(&argv[1])),
        Some("table") => table(Path::new(&argv[1]), &a.str("base", "V0"), &a.str("fields", "")),
        _ => eprintln!("usage: summarize table <file> [--base V0] [--fields a,b] | summarize noise <file>"),
    }
}

const SKIP: &[&str] = &["load1", "rep", "k", "n", "seed", "fill_pct", "trim"];

fn groups(recs: &[Rec]) -> BTreeMap<(String, String), BTreeMap<String, Vec<&Rec>>> {
    let mut g: BTreeMap<(String, String), BTreeMap<String, Vec<&Rec>>> = BTreeMap::new();
    for r in recs {
        let key = (
            r.str("bench").unwrap_or("").to_string(),
            format!("{} {}", r.str("part").unwrap_or(""), r.str("cfg").unwrap_or("")).trim().to_string(),
        );
        g.entry(key).or_default().entry(r.str("variant").unwrap_or("").to_string()).or_default().push(r);
    }
    g
}

fn table(path: &Path, base: &str, fields: &str) {
    let recs = load(path).unwrap();
    for ((bench, cfg), by_v) in groups(&recs) {
        let mut names: Vec<String> = Vec::new();
        for rs in by_v.values() {
            for (k, v) in rs[0].fields() {
                if matches!(v, Val::N(_)) && !SKIP.contains(&k.as_str()) && !names.contains(k) {
                    names.push(k.clone());
                }
            }
        }
        if !fields.is_empty() {
            let want: Vec<&str> = fields.split(',').collect();
            names.retain(|n| want.contains(&n.as_str()));
        }
        println!("\n### {bench} {cfg}\n");
        println!("| metric | {} |", by_v.iter().map(|(v, rs)| format!("{v} (n={})", rs.len())).collect::<Vec<_>>().join(" | "));
        println!("|---|{}", "---|".repeat(by_v.len()));
        for name in &names {
            let med = |rs: &Vec<&Rec>| median(&rs.iter().filter_map(|r| r.num(name)).collect::<Vec<_>>());
            let b = by_v.get(base).map(med);
            let cells: Vec<String> = by_v
                .iter()
                .map(|(v, rs)| {
                    let xs: Vec<f64> = rs.iter().filter_map(|r| r.num(name)).collect();
                    let m = median(&xs);
                    let lo = xs.iter().cloned().fold(f64::INFINITY, f64::min);
                    let hi = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let mut c = if xs.len() > 1 { format!("{} [{}..{}]", f(m), f(lo), f(hi)) } else { f(m) };
                    if let Some(b) = b {
                        if v != base && b != 0.0 && b.is_finite() {
                            c.push_str(&format!(" ({:+.1}%)", (m - b) / b * 100.0));
                        }
                    }
                    c
                })
                .collect();
            println!("| {name} | {} |", cells.join(" | "));
        }
        let loads: Vec<f64> = by_v.values().flatten().filter_map(|r| r.num("load1")).collect();
        println!("\nload1 median {:.1} (min {:.1}, max {:.1}); delta vs {base}", median(&loads),
            loads.iter().cloned().fold(f64::INFINITY, f64::min), loads.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
    }
}

fn f(x: f64) -> String {
    if !x.is_finite() {
        "-".into()
    } else if x.abs() >= 100.0 {
        format!("{x:.0}")
    } else if x.abs() >= 1.0 {
        format!("{x:.2}")
    } else {
        format!("{x:.4}")
    }
}

/// N = |median(first half) - median(second half)| / median(all), for each
/// numeric field, per (bench, cfg, variant).
fn noise(path: &Path) {
    let recs = load(path).unwrap();
    for ((bench, cfg), by_v) in groups(&recs) {
        for (v, rs) in by_v {
            println!("\n### noise floor: {bench} {cfg} {v} (runs: {})\n", rs.len());
            println!("| metric | median 1st half | median 2nd half | N |");
            println!("|---|---|---|---|");
            let h = rs.len() / 2;
            for (k, val) in rs[0].fields() {
                if !matches!(val, Val::N(_)) || SKIP.contains(&k.as_str()) {
                    continue;
                }
                let xs: Vec<f64> = rs.iter().filter_map(|r| r.num(k)).collect();
                let (m1, m2, m) = (median(&xs[..h]), median(&xs[h..]), median(&xs));
                println!("| {k} | {} | {} | {:.1}% |", f(m1), f(m2), ((m1 - m2).abs() / m * 100.0));
            }
        }
    }
}
