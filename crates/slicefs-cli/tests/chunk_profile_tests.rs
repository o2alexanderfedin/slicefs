//! The chunk profile of a store (Phase 22): a new store records a non-default
//! FastCDC profile in its header, every open of the content store uses it,
//! and another profile on an existing store is refused with no change.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chunk_store::{hasher_by_name, ChunkProfile, DEFAULT_HASHER};
use slicefs_cli::header::{self, Access, CHUNK_PROFILE_FLAG, CHUNK_PROFILE_RELEASE, FORMAT_FILE};
use slicefs_cli::mount::{start, StartArgs};
use metadata::store::DictMetadataStore;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::{content, seed};
use slicefs_traits::metadata::MetadataStore;
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

/// Names, sizes and blake3 hashes of all files and directories under
/// `root`, by relative name (as in store_header_tests.rs).
fn tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![root.to_path_buf()];
    let mut buf = vec![0u8; 64 << 10];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            if std::fs::symlink_metadata(&p).unwrap().is_dir() {
                out.insert(format!("{rel}/"), String::new());
                todo.push(p);
                continue;
            }
            let mut f = std::fs::File::open(&p).unwrap();
            let (mut h, mut len) = (blake3::Hasher::new(), 0);
            loop {
                let n = f.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                len += n;
            }
            out.insert(rel, format!("{len} {}", h.finalize().to_hex()));
        }
    }
    out
}

/// A source dir with one 1 MiB random file, at `<dir>/src`.
fn source(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("one"), random_bytes(1 << 20, 0x5eed_0202)).unwrap();
    src
}

/// A store at `<dir>/store` seeded with the 16/64/256 KiB profile.
fn seeded_large(dir: &Path) -> PathBuf {
    let store = dir.join("store");
    seed::run_seed_with(&store, &source(dir), None, Some(large())).unwrap();
    store
}

/// Assert that `err` names both profiles.
fn assert_names_both(err: &str) {
    for part in ["16/64/256 KiB", "4/8/32 KiB", "--chunk-profile"] {
        assert!(err.contains(part), "{err}");
    }
}

#[test]
fn seed_with_a_profile_makes_the_store_with_it() {
    let dir = TempDir::new().unwrap();
    let (src, p) = (source(dir.path()), large());
    let new = dir.path().join("new");
    seed::run_seed_with(&new, &src, None, Some(p)).unwrap();
    assert_eq!(format(&new), header::marker_with(DEFAULT_HASHER, p));
    assert_eq!(content::open_read_only(&new).unwrap().profile(), p);

    let new2 = dir.path().join("new2");
    seed::run_seed_with(&new2, &src, None, None).unwrap();
    assert_eq!(format(&new2), header::marker(DEFAULT_HASHER));
    assert_eq!(content::open_read_only(&new2).unwrap().profile(), ChunkProfile::DEFAULT);
}

#[test]
fn seed_refuses_another_profile_and_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let store = seeded_large(dir.path());
    let before = tree(&store);
    let err = seed::run_seed_with(&store, &dir.path().join("src"), None, Some(ChunkProfile::DEFAULT)).unwrap_err().to_string();
    assert_names_both(&err);
    assert_eq!(tree(&store), before, "the store tree does not change");
    seed::run_seed_with(&store, &dir.path().join("src"), None, Some(large())).unwrap();
}

/// A `--hash` change rewrites the header: the `chunks` line and its flag
/// are written back, and the store keeps its profile.
#[test]
fn a_hash_change_keeps_the_chunk_profile() {
    let dir = TempDir::new().unwrap();
    let (store, p) = (seeded_large(dir.path()), large());
    let keyed = hasher_by_name("test-keyed-blake3").unwrap();
    seed::run_seed_with(&store, &dir.path().join("src"), Some("test-keyed-blake3"), None).unwrap();
    assert_eq!(format(&store), header::marker_with(keyed, p));
    let cs = content::open_read_only(&store).unwrap();
    assert_eq!((cs.profile(), cs.hasher().name()), (p, "test-keyed-blake3"));
    drop(cs);

    let args = StartArgs { hash: Some("blake3-256".to_string()), chunk_profile: Some(p), ..Default::default() };
    let started = start(&store, &args, None).unwrap();
    assert_eq!(started.fs.content().profile(), p);
    started.stop();
    assert_eq!(format(&store), header::marker_with(DEFAULT_HASHER, p));
}

#[test]
fn mount_checks_the_profile() {
    let dir = TempDir::new().unwrap();
    let (store, p) = (seeded_large(dir.path()), large());
    let before = tree(&store);
    let other = StartArgs { chunk_profile: Some(ChunkProfile::DEFAULT), ..Default::default() };
    let err = start(&store, &other, None).err().expect("another profile is refused").to_string();
    assert_names_both(&err);
    assert_eq!(tree(&store), before, "the store tree does not change");
    for want in [Some(p), None] {
        let args = StartArgs { chunk_profile: want, ..Default::default() };
        let started = start(&store, &args, None).unwrap();
        assert_eq!(started.fs.content().profile(), p, "{want:?}");
        started.stop();
    }
}

#[test]
fn a_read_only_mount_refuses_another_profile() {
    let dir = TempDir::new().unwrap();
    let (store, p) = (seeded_large(dir.path()), large());
    let mut f = std::fs::OpenOptions::new().append(true).open(store.join(FORMAT_FILE)).unwrap();
    std::io::Write::write_all(&mut f, b"ro_compat test-ro-feature r-test\n").unwrap();
    drop(f);
    let before = tree(&store);
    let other = StartArgs { chunk_profile: Some(ChunkProfile::DEFAULT), ..Default::default() };
    let err = start(&store, &other, None).err().expect("another profile is refused").to_string();
    assert_names_both(&err);
    assert!(!err.contains("--hash"), "{err}");
    assert_eq!(tree(&store), before, "the store tree does not change");
    let started = start(&store, &StartArgs { chunk_profile: Some(p), ..Default::default() }, None).unwrap();
    assert!(matches!(started.access, Access::ReadOnly(_)), "{:?}", started.access);
    assert_eq!(started.fs.content().profile(), p);
    started.stop();
}

const S_IFREG: u32 = 0o100_000;
const MIB: usize = 1 << 20;

/// Check that file `ino` reads back as `model` and that its key is the key
/// of a fresh write of the same bytes in this store.
fn check_file(fs: &SliceFsFilesystem, ino: u64, model: &[u8], what: &str) -> slicefs_traits::key::ObjectKey {
    let root = fs.meta().get_manifest(ino).unwrap()[0];
    assert!(fs.read_content(&root).unwrap() == model, "{what}: bytes");
    assert_eq!(root, content::put_all(fs.content(), model).unwrap(), "{what}: key of a fresh write");
    root
}

/// D-22: a default store and a 16/64/256 KiB store, both open in one
/// process through the file-system layer. In each: a streamed file, a file
/// extended with zeros by a size change, and a small in-place edit (a
/// splice at release). Each reads back and has the key of a fresh write.
#[test]
fn two_profiles_through_the_file_system() {
    let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    content::check_write_with(b.path(), None, Some(large())).unwrap();
    let open = |dir: &Path| {
        let io = Arc::new(Mutex::new(content::store_io(dir).unwrap()));
        SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None)
    };
    let both = [(open(a.path()), ChunkProfile::DEFAULT), (open(b.path()), large())];
    let mut big_keys = Vec::new();
    for (fs, profile) in &both {
        assert_eq!(fs.content().profile(), *profile);
        let what = |step: &str| format!("{profile}: {step}");

        // (1) A 4 MiB file in four 1 MiB writes.
        let (big, fh) = fs.test_create(1, "big", S_IFREG | 0o644, 0o022, 0, 0).unwrap();
        let mut model = Vec::with_capacity(4 * MIB);
        for i in 0..4u64 {
            let piece = random_bytes(MIB, 0x22_0200 + i);
            fs.test_write(fh, i * MIB as u64, &piece).unwrap();
            model.extend_from_slice(&piece);
        }
        fs.test_release(big, fh).unwrap();
        big_keys.push(check_file(fs, big, &model, &what("stream")));

        // (2) 5000 bytes, then a size change adds a 3 MiB zero run.
        let (small, fh) = fs.test_create(1, "zeros", S_IFREG | 0o644, 0o022, 0, 0).unwrap();
        let mut zeros = random_bytes(5000, 0x22_0210);
        fs.test_write(fh, 0, &zeros).unwrap();
        fs.test_release(small, fh).unwrap();
        fs.test_setattr_size(small, None, (5000 + 3 * MIB) as u64).unwrap();
        zeros.resize(5000 + 3 * MIB, 0);
        check_file(fs, small, &zeros, &what("zero run"));

        // (3) A 4 KiB in-place edit of file (1), stored at release.
        let (fh, _) = fs.test_open(big, libc::O_RDWR).unwrap();
        let at = MIB + 77;
        let new: Vec<u8> = model[at..at + 4096].iter().map(|x| !x).collect();
        fs.test_write(fh, at as u64, &new).unwrap();
        model[at..at + 4096].copy_from_slice(&new);
        fs.test_release(big, fh).unwrap();
        check_file(fs, big, &model, &what("splice"));
    }
    assert_ne!(big_keys[0], big_keys[1], "the two profiles cut the same bytes into other chunks");
}
