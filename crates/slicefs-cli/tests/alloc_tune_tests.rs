//! `alloc_tune` (Phase 21 [Allocator]), in its own test binary: `hblkhd`
//! of glibc is process-wide, so a test beside the threshold test must not
//! make blocks of 128 KiB or more (they could be mmapped).

use slicefs_cli::alloc_tune;
use std::hint::black_box;

/// After `tune_at_start` the mmap threshold stays at 128 KiB. Without it,
/// the free of a 4 MiB mmapped block raises the threshold of glibc to about
/// 4 MiB, and a 1 MiB block then comes from an arena.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn tune_at_start_fixes_the_mmap_threshold() {
    assert_eq!(alloc_tune::tune_at_start(), Some(128 * 1024));
    drop(black_box(vec![1u8; 4 << 20]));
    let before = alloc_tune::stats().expect("glibc gives stats").mmap_bytes;
    let block = black_box(vec![1u8; 1 << 20]);
    let during = alloc_tune::stats().expect("glibc gives stats").mmap_bytes;
    drop(block);
    assert!(
        during >= before + (1 << 20),
        "a 1 MiB block after a 4 MiB free is not mmapped: hblkhd {before} -> {during}"
    );
}

/// Where the allocator gives numbers, the live heap is not empty.
#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
#[test]
fn stats_show_a_live_heap() {
    // 16 blocks of 64 KiB: below the threshold, so never mmapped.
    let keep: Vec<Vec<u8>> = (0..16).map(|_| black_box(vec![1u8; 64 << 10])).collect();
    let s = alloc_tune::stats().expect("this allocator gives stats");
    assert!(s.live_bytes >= 1 << 20, "live heap {} with 1 MiB held", s.live_bytes);
    drop(keep);
}
