//! Spike question a (Phase 17 plan 01): can a second process open a store
//! while a writer keeps it open? The real store needs this for `slicefs
//! stats` and `scrub` while a mount writes (content.rs open_read_only).
//!
//! Each test makes a small store with the bench binary, keeps it open in this
//! process as the writer, then starts the bench `open` step on the same
//! directory and prints what happened.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_index-bench");

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(BIN).args(args).output().expect("start index-bench");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn p(d: &Path) -> &str {
    d.to_str().unwrap()
}

#[test]
fn fjall_second_process_open_while_a_writer_holds_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("s");
    let (ok, _, err) = run(&["create", "--dir", p(&d), "--backend", "spike-fjall", "--objects", "1000"]);
    assert!(ok, "create failed: {err}");

    // The writer: the same database, open in this process.
    let db = fjall::Database::builder(d.join("fjall")).open().expect("writer open");
    let ks = db.keyspace("idx", fjall::KeyspaceCreateOptions::default).expect("writer keyspace");
    ks.insert([7u8; 33], [0u8; 25]).expect("writer insert");

    let (ok, out, err) = run(&["open", "--dir", p(&d), "--backend", "spike-fjall"]);
    println!("second open while the writer holds the store: success={ok}\nstdout:\n{out}stderr:\n{err}");
    // Observed with fjall 3.1.13: the open fails because the writer holds
    // the lock file; fjall has no read-only open mode.
    assert!(!ok, "the second open was expected to fail");
    assert!(err.contains("Locked"), "unexpected error: {err}");

    drop(ks);
    drop(db);
    let (ok, _, err) = run(&["open", "--dir", p(&d), "--backend", "spike-fjall"]);
    assert!(ok, "open after the writer closed failed: {err}");
}
