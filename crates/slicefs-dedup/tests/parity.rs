use cas_local::MemDedupIndex;
use proptest::prelude::*;
use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

#[derive(Debug, Clone)]
enum Op {
    Insert(u8),
    Lookup(u8),
    Remove(u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::Insert),
        any::<u8>().prop_map(Op::Lookup),
        any::<u8>().prop_map(Op::Remove),
    ]
}

fn make_hash(seed: u8) -> ChunkHash {
    let mut v = vec![0u8; 28];
    v[0] = seed;
    ChunkHash::from_bytes(v)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]
    #[test]
    fn parity_redb_vs_mem(ops in proptest::collection::vec(op_strategy(), 0..200)) {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let redb_idx = RedbDedupIndex::create(cfg).unwrap();
        let mem_idx = MemDedupIndex::new(1000, 0.01);

        for op in ops {
            match op {
                Op::Insert(s) => {
                    redb_idx.insert(&make_hash(s)).unwrap();
                    mem_idx.insert(&make_hash(s)).unwrap();
                }
                Op::Remove(s) => {
                    redb_idx.remove(&make_hash(s)).unwrap();
                    mem_idx.remove(&make_hash(s)).unwrap();
                }
                Op::Lookup(s) => {
                    let r_red = redb_idx.lookup(&make_hash(s)).unwrap();
                    let r_mem = mem_idx.lookup(&make_hash(s)).unwrap();
                    // Permitted demotion only: redb may say Absent where Mem says DefinitelyAbsent.
                    // Forbidden: redb says Present where Mem says Absent/DefinitelyAbsent.
                    match (r_red, r_mem) {
                        (DedupResult::Present, DedupResult::Present) => {}
                        (DedupResult::Absent, DedupResult::Absent) => {}
                        (DedupResult::Absent, DedupResult::DefinitelyAbsent) => {} // permitted demotion
                        (DedupResult::DefinitelyAbsent, DedupResult::DefinitelyAbsent) => {}
                        (a, b) => prop_assert!(false, "parity break: redb={a:?}, mem={b:?}"),
                    }
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]
    #[test]
    fn prop_open_close_open_preserves_set(seeds in proptest::collection::vec(any::<u8>(), 0..100)) {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        {
            let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
            for s in &seeds { idx.insert(&make_hash(*s)).unwrap(); }
            idx.flush().unwrap();
        }
        let idx = RedbDedupIndex::open(cfg).unwrap();
        for s in &seeds {
            let r = idx.lookup(&make_hash(*s)).unwrap();
            prop_assert!(matches!(r, DedupResult::Present));
        }
    }
}
