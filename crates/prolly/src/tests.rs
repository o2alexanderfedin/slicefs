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

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// About `n` directory-style entries (D-10): key = parent u64 (big endian)
/// then a name of 1 to 40 bytes; value of 0 to 200 bytes. Sorted, no
/// duplicate keys.
fn var_entries(n: usize, seed: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut m = std::collections::BTreeMap::new();
    for _ in 0..n {
        let mut k = (xorshift(&mut s) % 64).to_be_bytes().to_vec();
        let name_len = 1 + (xorshift(&mut s) % 40) as usize;
        k.extend((0..name_len).map(|_| b'a' + (xorshift(&mut s) % 26) as u8));
        let v: Vec<u8> = (0..xorshift(&mut s) % 201).map(|_| xorshift(&mut s) as u8).collect();
        m.insert(k, v);
    }
    m.into_iter().collect()
}

fn nodes(s: &ChunkStore, root: &chunk_store::ObjectId) -> Vec<NodeInfo> {
    let mut out = Vec::new();
    walk(s, root, |n| out.push(n.clone())).unwrap();
    out
}

fn entries_of(s: &ChunkStore, root: &chunk_store::ObjectId) -> Vec<(Vec<u8>, Vec<u8>)> {
    iter(s, root).unwrap().collect::<Result<Vec<_>>>().unwrap()
}

#[test]
fn cursor_iterates_in_order() {
    let s = mem_store();
    let es = u64_entries(2_000, 3);
    let (root, _) = build(&s, Config::SMALL, es.clone()).unwrap();
    assert_eq!(entries_of(&s, &root), es);
    let empty = empty_root(&s).unwrap();
    assert!(entries_of(&s, &empty).is_empty());
    let (built_empty, _) = build(&s, Config::SMALL, Vec::<(Vec<u8>, Vec<u8>)>::new()).unwrap();
    assert_eq!(built_empty, empty);
    let (one, _) = build(&s, Config::SMALL, [(b"k", b"v")]).unwrap();
    assert_eq!(entries_of(&s, &one), vec![(b"k".to_vec(), b"v".to_vec())]);
}

#[test]
fn node_size_limits() {
    let trees = [
        ("default u64", Config::DEFAULT, u64_entries(20_000, 3)),
        ("small u64", Config::SMALL, u64_entries(20_000, 3)),
        ("small var", Config::SMALL, var_entries(5_000, 9)),
    ];
    for (what, cfg, es) in trees {
        let s = mem_store();
        let (root, _) = build(&s, cfg, es.iter().map(|(k, v)| (k, v))).unwrap();
        assert_eq!(entries_of(&s, &root), es, "{what}");
        let ns = nodes(&s, &root);
        assert!(ns[0].level == 0 || ns[0].entries >= 2, "{what}: root {:?}", ns[0]);
        for n in &ns {
            assert!(n.bytes <= cfg.node_max(), "{what}: {n:?}");
            if !n.last_of_level {
                assert!(n.bytes >= cfg.node_min(), "{what}: {n:?}");
                assert!(n.level == 0 || n.entries >= 2, "{what}: {n:?}");
            }
        }
        if cfg == Config::DEFAULT {
            let leaves: Vec<_> = ns.iter().filter(|n| n.level == 0).collect();
            let mean = leaves.iter().map(|n| n.bytes).sum::<usize>() as f64 / leaves.len() as f64;
            let l = cfg.weibull_l() as f64;
            assert!((0.75 * l..=l).contains(&mean), "{what}: mean leaf {mean}");
        }
    }
}

#[test]
fn boundaries_ignore_value_bytes() {
    for cfg in [Config::SMALL, Config::DEFAULT] {
        let s = mem_store();
        let a = u64_entries(5_000, 3);
        let b: Vec<_> = a.iter().map(|(k, v)| (k.clone(), v.iter().map(|x| x ^ 0xff).collect::<Vec<u8>>())).collect();
        let (ra, _) = build(&s, cfg, a).unwrap();
        let (rb, _) = build(&s, cfg, b).unwrap();
        assert_ne!(ra, rb);
        let shape = |r| nodes(&s, &r).into_iter().map(|n| (n.level, n.last_key)).collect::<Vec<_>>();
        assert_eq!(shape(ra), shape(rb), "{cfg:?}");
    }
}

#[test]
fn levels_use_their_own_salt() {
    let s = mem_store();
    let (root, _) = build(&s, Config::DEFAULT, u64_entries(200_000, 3)).unwrap();
    let ns = nodes(&s, &root);
    let at = |level| ns.iter().filter(move |n| n.level == level);
    let mean = |level| at(level).map(|n| n.bytes).sum::<usize>() as f64 / at(level).count() as f64;
    assert!(at(1).count() >= 20, "level 1 has {} nodes", at(1).count());
    let (m0, m1) = (mean(0), mean(1));
    assert!(m1 >= m0 / 2.0 && m1 <= m0 * 2.0, "leaf mean {m0}, level-1 mean {m1}");
    // The level-0 hash of a leaf-ending key is low (that is why the leaf
    // ended there); the level-1 salt makes it uniform again.
    let ends: Vec<_> = at(0).filter(|n| !n.last_of_level).map(|n| n.last_key.clone()).collect();
    let h = |level| ends.iter().map(|k| boundary_hash(level, k) as f64 / 4_294_967_295.0).sum::<f64>() / ends.len() as f64;
    assert!(h(0) < 0.1, "level-0 mean {}", h(0));
    assert!((0.4..=0.6).contains(&h(1)), "level-1 mean {}", h(1));
}

#[test]
fn same_set_twice_writes_nothing() {
    let s = mem_store();
    let (r1, _) = build(&s, Config::DEFAULT, u64_entries(3_000, 3)).unwrap();
    let before = s.stats();
    let (r2, _) = build(&s, Config::DEFAULT, u64_entries(3_000, 3)).unwrap();
    assert_eq!(r1, r2);
    let after = s.stats();
    assert_eq!((after.objects, after.raw_bytes), (before.objects, before.raw_bytes));
}

#[test]
fn memory_is_bounded() {
    let mut high = Vec::new();
    for n in [2_000, 40_000] {
        let s = mem_store();
        let (root, st) = build(&s, Config::SMALL, u64_entries(n, 3)).unwrap();
        assert!(st.high_water_bytes <= st.depth as usize * Config::SMALL.node_max(), "{n}: {st:?}");
        high.push(st.high_water_bytes);
        if n == 40_000 {
            let mut c = iter(&s, &root).unwrap();
            let mut seen = 0;
            for e in c.by_ref() {
                e.unwrap();
                seen += 1;
            }
            assert_eq!(seen, n);
            assert_eq!(c.max_nodes_held(), st.depth as usize);
        }
    }
    assert!(high[1] <= 3 * high[0], "high water {high:?}");
}

/// Node bytes with any header and body (no checks).
fn raw(level: u8, count: u32, body: &[u8]) -> Vec<u8> {
    let mut b = NODE_MAGIC.to_vec();
    b.push(level);
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(body);
    b
}

fn raw_leaf_entry(k: &[u8], v: &[u8]) -> Vec<u8> {
    let mut b = (k.len() as u16).to_le_bytes().to_vec();
    b.extend_from_slice(&(v.len() as u16).to_le_bytes());
    b.extend_from_slice(k);
    b.extend_from_slice(v);
    b
}

/// `get` (of a key routed to the first child), a full cursor run and
/// `walk`, each caught for panics.
fn read_all(s: &ChunkStore, root: &chunk_store::ObjectId) -> Vec<std::thread::Result<Result<()>>> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    vec![
        catch_unwind(AssertUnwindSafe(|| get(s, root, b"a").map(|_| ()))),
        catch_unwind(AssertUnwindSafe(|| iter(s, root).and_then(|c| c.collect::<Result<Vec<_>>>()).map(|_| ()))),
        catch_unwind(AssertUnwindSafe(|| walk(s, root, |_| {}))),
    ]
}

#[test]
fn malformed_node_is_an_error() {
    use crate::node::{encode_internal, encode_leaf, Child};
    let s = mem_store();
    let good_leaf = encode_leaf(&[(b"a".to_vec(), b"1".to_vec())]);
    let leaf_id = s.put(&good_leaf).unwrap();
    let mut wrong_magic = good_leaf.clone();
    wrong_magic[0] = b'X';
    let mut trailing = good_leaf.clone();
    trailing.push(0);
    let past_end = raw(0, 1, &[10, 0, 0, 0, b'a', b'b', b'c']);
    let out_of_order = raw(0, 2, &[raw_leaf_entry(b"b", b""), raw_leaf_entry(b"a", b"")].concat());
    let equal = raw(0, 2, &[raw_leaf_entry(b"a", b""), raw_leaf_entry(b"a", b"")].concat());
    let long_key = raw(0, 1, &raw_leaf_entry(&[b'a'; MAX_KEY + 1], b""));
    let child = |id| Child { last_key: b"zz".to_vec(), id, count: 1 };
    let wrong_level = encode_internal(2, &[child(leaf_id)]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("wrong magic", wrong_magic),
        ("5-byte node", good_leaf[..5].to_vec()),
        ("count above the bytes", raw(0, u32::MAX, &[0; 11])),
        ("entry past the end", past_end),
        ("trailing bytes", trailing),
        ("keys out of order", out_of_order),
        ("equal keys", equal),
        ("internal with count 0", raw(1, 0, &[])),
        ("child at the wrong level", wrong_level),
        ("key above MAX_KEY", long_key),
    ];
    for (what, bytes) in cases {
        let bad = s.put(&bytes).unwrap();
        // As the root, and as the child of a good level-1 node.
        let parent = s.put(&encode_internal(1, &[child(bad)])).unwrap();
        for (place, root) in [("root", bad), ("child", parent)] {
            for got in read_all(&s, &root) {
                assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{what} as {place}: {got:?}");
            }
        }
    }
    // A child that is not in the store.
    let missing = s.put(&encode_internal(1, &[child(chunk_store::ObjectId([0x1e; chunk_store::ID_LEN]))])).unwrap();
    for got in read_all(&s, &missing) {
        assert!(matches!(got, Ok(Err(Error::Store(_)))), "missing child: {got:?}");
    }
    // The good leaf under a good parent still reads.
    let ok = s.put(&encode_internal(1, &[child(leaf_id)])).unwrap();
    assert_eq!(get(&s, &ok, b"a").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn entry_too_large_is_an_error() {
    let s = mem_store();
    let before = s.stats();
    let mut b = Builder::new(&s, Config::SMALL);
    assert!(matches!(b.push(b"k", &[0; 300]), Err(Error::EntryTooLarge { len: 305, max: 256 })));
    assert!(matches!(b.push(&[b'k'; 129], b""), Err(Error::KeyTooLarge { len: 129, max: 128 })));
    assert_eq!(s.stats(), before);
}

#[test]
fn unsorted_input_is_an_error() {
    let s = mem_store();
    let mut b = Builder::new(&s, Config::SMALL);
    b.push(b"b", b"").unwrap();
    assert!(matches!(b.push(b"a", b""), Err(Error::Unsorted)));
    let mut b = Builder::new(&s, Config::SMALL);
    b.push(b"a", b"").unwrap();
    assert!(matches!(b.push(b"a", b""), Err(Error::Unsorted)));
    assert!(matches!(build(&s, Config::SMALL, [(b"b", b""), (b"a", b"")]), Err(Error::Unsorted)));
}
