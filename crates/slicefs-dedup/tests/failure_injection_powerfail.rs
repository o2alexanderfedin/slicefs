//! FI test #9 — power-fail (loop device that drops writes after T_drop).
//!
//! Manual setup (Linux only):
//!   1. truncate -s 1G /tmp/sfi9.img
//!   2. modprobe nbd
//!   3. start nbd-server-drop --drop-after 200ms /tmp/sfi9.img &
//!   4. nbd-client localhost /dev/nbd0
//!   5. mkfs.ext4 -F /dev/nbd0
//!   6. mount /dev/nbd0 /mnt/sfi9
//!   7. SLICEFS_FI_POWERFAIL=1 SLICEFS_FI_MOUNT=/mnt/sfi9 cargo test --test failure_injection_powerfail
//!
//! `nbd-server-drop` is a small wrapper (out of scope here) that ignores
//! writes after the configured T_drop window has elapsed.

#[test]
fn t9_powerfail_no_fp() {
    if std::env::var("SLICEFS_FI_POWERFAIL").is_err() {
        eprintln!("SKIP: t9 requires SLICEFS_FI_POWERFAIL=1 + Linux nbd setup");
        return;
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("SKIP: t9 is Linux-only");
        #[allow(clippy::needless_return)]
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
        use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

        let mount = std::env::var("SLICEFS_FI_MOUNT").expect("SLICEFS_FI_MOUNT");
        let cas = std::path::PathBuf::from(&mount).join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();

        // Insert burst longer than T_drop window so the tail gets dropped.
        {
            let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
            for i in 0u64..50_000 {
                let mut h = [0u8; 28];
                h[..8].copy_from_slice(&i.to_le_bytes());
                let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
                let shard = cas.join(&hex[..2]);
                let _ = std::fs::create_dir_all(&shard);
                let _ = std::fs::write(shard.join(&hex[2..]), b"x");
                let _ = idx.insert(&ChunkHash::from_bytes(h.to_vec()));
            }
            // No flush — writes after T_drop are dropped by the nbd-server shim.
        }

        // Re-mount: lookup any-Present hash → corresponding CAS block must exist (I1).
        let idx = RedbDedupIndex::open(cfg).expect("must mount after power-fail");
        for i in 0u64..50_000 {
            let mut h = [0u8; 28];
            h[..8].copy_from_slice(&i.to_le_bytes());
            let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            if matches!(r, DedupResult::Present) {
                let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
                let p = cas.join(&hex[..2]).join(&hex[2..]);
                assert!(p.exists(), "I1 violated at i={}", i);
            }
        }
    }
}
