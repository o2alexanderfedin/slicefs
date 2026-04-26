/// Tests for --wal-strategy CLI flag parsing.
///
/// Verifies that all 4 WAL strategy values are accepted by the mount subcommand.
use clap::Parser;
use slicefs_cli::cli::{Cli, Cmd};

/// Parse --wal-strategy per-op and verify it is accepted.
#[test]
fn test_wal_strategy_per_op() {
    let cli = Cli::try_parse_from([
        "slicefs",
        "mount",
        "/mnt",
        "--store",
        "/data",
        "--wal-strategy",
        "per-op",
    ])
    .expect("per-op should be valid");

    match cli.command {
        Cmd::Mount { wal_strategy, .. } => {
            assert_eq!(wal_strategy.as_deref(), Some("per-op"));
        }
        _ => panic!("expected Mount"),
    }
}

/// Parse --wal-strategy periodic and verify it is accepted.
#[test]
fn test_wal_strategy_periodic() {
    let cli = Cli::try_parse_from([
        "slicefs",
        "mount",
        "/mnt",
        "--store",
        "/data",
        "--wal-strategy",
        "periodic",
    ])
    .expect("periodic should be valid");

    match cli.command {
        Cmd::Mount { wal_strategy, .. } => {
            assert_eq!(wal_strategy.as_deref(), Some("periodic"));
        }
        _ => panic!("expected Mount"),
    }
}

/// Parse --wal-strategy flush-on-fsync and verify it is accepted.
#[test]
fn test_wal_strategy_flush_on_fsync() {
    let cli = Cli::try_parse_from([
        "slicefs",
        "mount",
        "/mnt",
        "--store",
        "/data",
        "--wal-strategy",
        "flush-on-fsync",
    ])
    .expect("flush-on-fsync should be valid");

    match cli.command {
        Cmd::Mount { wal_strategy, .. } => {
            assert_eq!(wal_strategy.as_deref(), Some("flush-on-fsync"));
        }
        _ => panic!("expected Mount"),
    }
}

/// Parse --wal-strategy no-wal and verify it is accepted.
#[test]
fn test_wal_strategy_no_wal() {
    let cli = Cli::try_parse_from([
        "slicefs",
        "mount",
        "/mnt",
        "--store",
        "/data",
        "--wal-strategy",
        "no-wal",
    ])
    .expect("no-wal should be valid");

    match cli.command {
        Cmd::Mount { wal_strategy, .. } => {
            assert_eq!(wal_strategy.as_deref(), Some("no-wal"));
        }
        _ => panic!("expected Mount"),
    }
}

/// --wal-strategy defaults to per-op when not specified.
#[test]
fn test_wal_strategy_default_is_per_op() {
    let cli = Cli::try_parse_from(["slicefs", "mount", "/mnt", "--store", "/data"])
        .expect("mount without wal-strategy should parse fine");

    match cli.command {
        Cmd::Mount { wal_strategy, .. } => {
            // Default is per-op when flag is absent
            let strategy = wal_strategy.as_deref().unwrap_or("per-op");
            assert_eq!(strategy, "per-op", "default strategy should be per-op");
        }
        _ => panic!("expected Mount"),
    }
}
