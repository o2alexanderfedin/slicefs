//! The chunk profile of a store (Phase 22): a new store records a non-default
//! FastCDC profile in its header, every open of the content store uses it,
//! and another profile on an existing store is refused with no change.

use std::path::Path;

use chunk_store::{ChunkProfile, DEFAULT_HASHER};
use slicefs_cli::header::{self, CHUNK_PROFILE_FLAG, CHUNK_PROFILE_RELEASE, FORMAT_FILE};
use slicefs_cli::{content, seed};
use tempfile::TempDir;

/// `n` xorshift64 bytes from `seed`.
fn random_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn large() -> ChunkProfile {
    ChunkProfile::new(16384, 65536, 262144).unwrap()
}

fn format(store: &Path) -> String {
    std::fs::read_to_string(store.join(FORMAT_FILE)).unwrap()
}

#[test]
fn a_store_made_with_a_profile_seeds_with_it() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let p = large();
    assert_eq!(content::check_write_with(&store, None, Some(p)).unwrap(), None);
    let text = format(&store);
    assert_eq!(text, header::marker_with(DEFAULT_HASHER, p));
    assert!(text.lines().any(|l| l == "chunks 16384 65536 262144"), "{text}");
    let flag = format!("ro_compat {CHUNK_PROFILE_FLAG} {CHUNK_PROFILE_RELEASE}");
    assert!(text.lines().any(|l| l == flag), "{text}");

    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let data = random_bytes(3 << 20, 0x5eed_0022);
    std::fs::write(src.join("big"), &data).unwrap();
    std::fs::write(src.join("small"), b"ten bytes!").unwrap();
    seed::run_seed(&store, &src, None).unwrap();

    let cs = content::open_read_only(&store).unwrap();
    assert_eq!(cs.profile(), p);
    let ranges = p.ranges(&data);
    assert!(ranges.iter().any(|r| r.len() > 32 << 10));
    for r in &ranges {
        assert!(cs.contains(&cs.hasher().id(&data[r.clone()])), "chunk {r:?} of the store profile");
    }
    let default = ChunkProfile::DEFAULT.ranges(&data);
    assert!(default.iter().any(|r| !cs.contains(&cs.hasher().id(&data[r.clone()]))));
}

#[test]
fn a_default_store_keeps_the_header_of_today() {
    let dir = TempDir::new().unwrap();
    let new = dir.path().join("a");
    let new2 = dir.path().join("b");
    content::check_write_with(&new, None, None).unwrap();
    content::check_write_with(&new2, None, Some(ChunkProfile::DEFAULT)).unwrap();
    assert_eq!(format(&new), header::marker(DEFAULT_HASHER));
    assert_eq!(format(&new2), header::marker(DEFAULT_HASHER));
    assert_eq!(content::open(&new).unwrap().profile(), ChunkProfile::DEFAULT);
}

#[test]
fn another_profile_on_an_existing_store_is_refused() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let p = large();
    content::check_write_with(&store, None, Some(p)).unwrap();
    let before = format(&store);
    for hash in [None, Some("test-keyed-blake3")] {
        let e = content::check_write_with(&store, hash, Some(ChunkProfile::DEFAULT)).unwrap_err().to_string();
        for part in ["16/64/256 KiB", "4/8/32 KiB", "--chunk-profile"] {
            assert!(e.contains(part), "{e}");
        }
        assert_eq!(format(&store), before);
    }
    assert_eq!(content::check_write_with(&store, None, Some(p)).unwrap(), None);
    assert_eq!(format(&store), before);

    let plain = dir.path().join("plain");
    header::create(&plain).unwrap();
    let before = format(&plain);
    let e = content::check_write_with(&plain, None, Some(p)).unwrap_err().to_string();
    for part in ["16/64/256 KiB", "4/8/32 KiB", "--chunk-profile"] {
        assert!(e.contains(part), "{e}");
    }
    assert_eq!(format(&plain), before);
}
