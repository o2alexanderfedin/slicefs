//! Bench driver.
//!
//! `bench suite <bench> --variants RS,V0,V1 --reps 5 --out results/a/x.jsonl [--k v ...]`
//! `bench one <bench> --variant V1 [--k v ...]` (one run; prints one or more JSON lines)

use arena_spike::bench::{run_one, Args};
use arena_spike::measure::interleave;
use std::path::PathBuf;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("one") => {
            let bench = &argv[1];
            let a = Args::parse(&argv[2..]);
            let v = a.str("variant", "V1");
            if !run_one(bench, &v, &a) {
                eprintln!("unknown bench {bench}");
                std::process::exit(2);
            }
        }
        Some("suite") => {
            let bench = argv[1].clone();
            let a = Args::parse(&argv[2..]);
            let variants: Vec<String> = a.str("variants", "V0,V1,V2,V3,RS").split(',').map(String::from).collect();
            let reps = a.u64("reps", 5) as usize;
            let out = PathBuf::from(a.str("out", &format!("results/a/{bench}.jsonl")));
            // Pass every other --key value through to the runs.
            let mut extra = Vec::new();
            for (k, v) in &a.0 {
                if !["variants", "reps", "out"].contains(&k.as_str()) {
                    extra.push(format!("--{k}"));
                    extra.push(v.clone());
                }
            }
            interleave(&bench, &variants, reps, &extra, &out).unwrap();
        }
        _ => {
            eprintln!("usage: bench suite <bench> [--variants ..] [--reps N] [--out F] [--k v ..] | bench one <bench> --variant V [..]");
            std::process::exit(2);
        }
    }
}
