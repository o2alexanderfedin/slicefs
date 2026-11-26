use crate::rule::exp_neg;
use crate::*;
use chunk_store::mem::MemIo;
use chunk_store::{ChunkStore, Options};
use std::sync::Arc;

fn mem_store() -> ChunkStore {
    ChunkStore::open_with(Arc::new(MemIo::new()), Options { zstd_level: 0, ..Options::default() }).unwrap()
}

/// `n` entries shaped like `inode_data`: a u64 key (big endian, so byte
/// order is numeric order) and a 33-byte value (tag 0x1e, then a digest).
fn u64_entries(n: usize, stride: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n)
        .map(|i| {
            let k = ((i * stride + 3) as u64).to_be_bytes().to_vec();
            let mut v = vec![0x1e];
            v.extend_from_slice(&blake3::hash(&k).as_bytes()[..32]);
            (k, v)
        })
        .collect()
}

#[test]
fn build_then_get_roundtrip() {
    for (cfg, min_depth) in [(Config::DEFAULT, 2), (Config::SMALL, 3)] {
        let s = mem_store();
        let es = u64_entries(3_000, 5);
        let (root, st) = build(&s, cfg, es.iter().map(|(k, v)| (k, v))).unwrap();
        for (k, v) in &es {
            assert_eq!(get(&s, &root, k).unwrap().as_deref(), Some(v.as_slice()));
        }
        for absent in [0u64, 4, 6, 3 + 5 * 3_000, u64::MAX] {
            assert_eq!(get(&s, &root, &absent.to_be_bytes()).unwrap(), None, "{absent}");
        }
        assert!(st.depth >= min_depth, "{cfg:?}: depth {}", st.depth);
        assert_eq!(st.entries, 3_000);
        assert_eq!(s.stats().objects, st.nodes_written);
    }
}

#[test]
fn golden_root() {
    let s = mem_store();
    let (root, st) = build(&s, Config::DEFAULT, u64_entries(5_000, 7)).unwrap();
    assert_eq!(root.to_hex(), "1ee1cf56844ec96d06f8dce27d4e0de09b87e7ce5926e397ff4e4c387e271f81d6");
    assert_eq!((st.depth, st.leaves), (3, 58));
    let d = Config::DEFAULT;
    let table = [(d, 9, 54), (d, 467, 512), (d, 3000, 3045), (d, 4096, 4141), (d, 8000, 8045), (d, 14000, 14045)]
        .into_iter()
        .chain([(Config::SMALL, 9, 30), (Config::SMALL, 200, 230)]);
    let bits: Vec<u64> = table.map(|(c, b, a)| cut_probability(&c, b, a).to_bits()).collect();
    assert_eq!(
        bits,
        [
            4494650349440729088,
            4545174308703870976,
            4580702284337380896,
            4586457444719755424,
            4598737513935175060,
            4605637781295192346,
            4551032867001516032,
            4597940318959880728
        ]
    );
    assert_eq!((boundary_hash(0, b""), boundary_hash(1, b"key")), (527_629_384, 94_351_618));
    // The basic-operation exp agrees with the std one (a reference here only).
    for i in 0..200 {
        let x = i as f64 * 0.2;
        assert!((exp_neg(x) - (-x).exp()).abs() <= 1e-13, "x={x}");
    }
}
