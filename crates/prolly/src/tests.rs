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

/// The table bytes (EXP_INT then EXP_FRAC, each u64 little endian).
fn table_bytes() -> Vec<u8> {
    use crate::rule_table::{EXP_FRAC, EXP_INT};
    EXP_INT.iter().chain(EXP_FRAC.iter()).flat_map(|v| v.to_le_bytes()).collect()
}

/// Entry counts of the leaves, in leaf order.
fn leaf_counts(s: &ChunkStore, root: &chunk_store::ObjectId) -> Vec<usize> {
    nodes(s, root).iter().filter(|n| n.level == 0).map(|n| n.entries).collect()
}

/// `PROLLY_PRINT_GOLDENS=1 cargo test -p prolly --lib -- --exact tests::golden_root --nocapture`
/// prints every pinned value in the form of its `pin!` line (and asserts
/// nothing), to re-pin.
#[test]
fn golden_root() {
    let print = std::env::var_os("PROLLY_PRINT_GOLDENS").is_some();
    macro_rules! pin {
        ($got:expr, $want:expr) => {{
            let got = $got;
            if print {
                println!("pin!({}, {:?});", stringify!($got), got);
            } else {
                assert_eq!(got, $want, "{}", stringify!($got));
            }
        }};
    }
    // (a) the table.
    pin!(blake3::hash(&table_bytes()).to_hex().to_string(), "6a72017ef6d3d7daf2ea9385afe01ac20fd4bf85610a5179c9bc7384bcc369e4");
    // (b) integer thresholds.
    let d = Config::DEFAULT;
    let table = [(d, 9, 54), (d, 467, 512), (d, 3000, 3045), (d, 4096, 4141), (d, 8000, 8045), (d, 14000, 14045)]
        .into_iter()
        .chain([(Config::SMALL, 9, 30), (Config::SMALL, 200, 230)]);
    let ths: Vec<u64> = table.map(|(c, b, a)| threshold(&c, b, a)).collect();
    pin!(ths, [7, 20166, 4737341, 11975333, 87726437, 448166577, 803046, 1045739182]);
    // (c) the cut points of a build, (d) its root.
    let s = mem_store();
    let (root, st) = build(&s, Config::DEFAULT, u64_entries(5_000, 7)).unwrap();
    pin!(leaf_counts(&s, &root), [258, 103, 205, 132, 161, 210, 123, 185, 103, 235, 232, 177, 155, 154, 156, 251, 155, 155, 105, 162, 287, 209, 170, 139, 165, 45, 200, 215, 128, 25]);
    pin!(root.to_hex(), "1e74a7eb632708b048b3d53a40d134148232c48802088f47a84250e92c192ded4f");
    pin!((st.depth, st.leaves), (2, 30));
    // (f) a salted root, and thresholds of a wide config.
    let salted = Config::DEFAULT.with_salt(Some([7u8; 32]));
    let (salted_root, _) = build(&mem_store(), salted, u64_entries(5_000, 7)).unwrap();
    pin!(salted_root.to_hex(), "1e018bcf4bdb6d818cb17bdf929bab475b6ce59a9f64fc565b3bdb85645c485e89");
    let wide = Config::new(8192, 32768).unwrap();
    pin!((threshold(&wide, 20_000, 20_045), threshold(&wide, 30_000, 30_045)), (1_178_752_456, 2_838_699_595));
    // (e) the unsalted boundary hash.
    pin!((boundary_hash(0, b""), boundary_hash(1, b"key")), (527629384, 94351618));
}

fn table_float(x: f64) -> f64 {
    (-x).exp()
}

#[test]
fn rule_table_matches_float_exp() {
    use crate::rule_table::{EXP_FRAC, EXP_INT};
    let one = 1u64 << 32;
    assert_eq!((EXP_INT[0], EXP_FRAC[0]), (one, one));
    assert!(EXP_INT.windows(2).all(|w| w[0] > w[1]), "EXP_INT strictly decreases");
    assert!(EXP_FRAC.windows(2).all(|w| w[0] > w[1]), "EXP_FRAC strictly decreases");
    assert_eq!(EXP_FRAC[1024], EXP_INT[1]);
    for (i, v) in EXP_INT.iter().enumerate() {
        let want = (one as f64 * table_float(i as f64)).round();
        assert!((*v as f64 - want).abs() <= 1.0, "EXP_INT[{i}]");
    }
    for (j, v) in EXP_FRAC.iter().enumerate() {
        let want = (one as f64 * table_float(j as f64 / 1024.0)).round();
        assert!((*v as f64 - want).abs() <= 1.0, "EXP_FRAC[{j}]");
    }
}

/// The float rule, as a reference only (never a source of the table).
fn float_p(cfg: &Config, before: usize, after: usize) -> f64 {
    let l = cfg.weibull_l() as f64;
    let (b, a) = (before as f64, after as f64);
    let delta = (a.powi(4) - b.powi(4)) / l.powi(4);
    1.0 - (-delta).exp()
}

#[test]
fn fixed_point_matches_float() {
    let two32 = (1u64 << 32) as f64;
    let tol = 2f64.powi(-22);
    let mut max_err = 0f64;
    let mut check = |cfg: &Config, b: usize, a: usize| {
        let err = (threshold(cfg, b, a) as f64 / two32 - float_p(cfg, b, a)).abs();
        assert!(err <= tol, "{cfg:?} {b}->{a}: error {err}");
        max_err = max_err.max(err);
    };
    let cfgs = [
        Config::DEFAULT,
        Config::SMALL,
        Config::new(8192, 16384).unwrap(),
        Config::new(8192, 32768).unwrap(),
    ];
    let mut seed = 0x1234_5678_9abc_def1u64;
    for cfg in &cfgs {
        for before in (0..cfg.node_max()).step_by(64) {
            for step in [1, 4, 45, 200, 2048] {
                check(cfg, before, (before + step).min(cfg.node_max()));
            }
        }
        for _ in 0..50_000 {
            let b = (xorshift(&mut seed) % cfg.node_max() as u64) as usize;
            let a = b + (xorshift(&mut seed) % (cfg.node_max() - b + 1) as u64) as usize;
            check(cfg, b, a);
        }
    }
    println!("max error {max_err}");
    // Decisions: the integer test against the float test.
    let mut diffs = 0;
    for cfg in &cfgs {
        for _ in 0..250_000 {
            let b = cfg.node_min() + (xorshift(&mut seed) % (cfg.node_max() - cfg.node_min()) as u64) as usize;
            let a = (b + 1 + (xorshift(&mut seed) % 2048) as usize).min(cfg.node_max());
            let h = xorshift(&mut seed) as u32;
            let int_cut = (h as u64) < threshold(cfg, b, a);
            let float_cut = (h as f64 / 4_294_967_295.0) < float_p(cfg, b, a);
            diffs += (int_cut != float_cut) as u32;
        }
    }
    println!("decisions that differ: {diffs} of 1000000");
    assert!(diffs <= 4, "{diffs} decisions differ");
}

/// A key with a boundary hash below (`below`) or at or above the threshold.
fn key_with_hash(level: u8, thr: u64, below: bool) -> Vec<u8> {
    (0u32..)
        .map(|i| i.to_be_bytes().to_vec())
        .find(|k| ((boundary_hash(level, k) as u64) < thr) == below)
        .unwrap()
}

#[test]
fn threshold_edges() {
    let d = Config::DEFAULT;
    for b in [0, 9, 512, 16_000] {
        assert_eq!(threshold(&d, b, b), 0, "b={b}");
    }
    // At node_max the rule is not 2^32 (e^-16 is about 483 units of 2^-32); the forced cut ends the node.
    assert_eq!(threshold(&d, 0, 16_384), (1 << 32) - 483);
    assert_eq!(threshold(&d, 0, 32_768), 1 << 32);
    assert!(threshold(&d, 0, 4_096) < 1 << 32);
    let (min, force) = (d.node_min(), d.force_at());
    let thr_min = threshold(&d, NODE_HEADER, min);
    let cut_key = key_with_hash(0, thr_min, true);
    let keep_key = key_with_hash(0, thr_min, false);
    // Size node_min - 1: never ends, even for a key whose hash is below the threshold.
    assert!(!Splitter::new().push(&d, 0, &cut_key, min - 1 - NODE_HEADER));
    // Size node_min: the hash test decides.
    assert!(Splitter::new().push(&d, 0, &cut_key, min - NODE_HEADER));
    assert!(!Splitter::new().push(&d, 0, &keep_key, min - NODE_HEADER));
    // Size force_at is not forced (the hash test decides); force_at + 1 is.
    let thr_f = threshold(&d, force - 10, force);
    assert!(thr_f > 0 && thr_f < 1 << 32);
    let keep = key_with_hash(0, thr_f, false);
    assert!(!Splitter::at(force - 10, 5).push(&d, 0, &keep, 10));
    let thr_f1 = threshold(&d, force - 10, force + 1);
    let keep1 = key_with_hash(0, thr_f1, false);
    assert!(Splitter::at(force - 10, 5).push(&d, 0, &keep1, 11));
    assert!(!Splitter::at(force - 10, 5).forced(&d));
    assert!(Splitter::at(force + 1, 5).forced(&d));
    // The largest sizes: fourth powers, the shift and the product stay in
    // u128 (this test runs with overflow checks on).
    let (small_l, big_l) = (Config::new(256, 32_768).unwrap(), Config::new(16_384, 32_768).unwrap());
    // delta = 2^16 for L = 256: past the table, always cut.
    assert_eq!(threshold(&small_l, 0, 32_768), 1 << 32);
    // delta = 16 for L = 16,384: e^-16 is about 483 / 2^32, so just below 2^32.
    let t = threshold(&big_l, 0, 32_768);
    assert!(t < 1 << 32 && t > (1 << 32) - 1_000, "{t}");
    for cfg in [small_l, big_l] {
        assert_eq!(threshold(&cfg, 32_768, 32_768), 0);
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]
    #[test]
    fn threshold_is_monotone(small in proptest::prelude::any::<bool>(), b in 0usize..100_000, a_off in 0usize..100_000) {
        let cfg = if small { Config::SMALL } else { Config::DEFAULT };
        let m = cfg.node_max() - 1;
        let b = b % m;
        let a = b + a_off % (m - b + 1);
        // Later end: a higher threshold. Later start: a lower one.
        proptest::prop_assert!(threshold(&cfg, b, a) <= threshold(&cfg, b, a + 1));
        if b < a {
            proptest::prop_assert!(threshold(&cfg, b, a) >= threshold(&cfg, b + 1, a));
        }
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
    let (root, _) = build(&s, Config::DEFAULT, u64_entries(500_000, 3)).unwrap();
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
    raw_v(RULE_VERSION, level, count, body)
}

/// Like `raw`, with any rule version byte.
fn raw_v(version: u8, level: u8, count: u32, body: &[u8]) -> Vec<u8> {
    let mut b = NODE_MAGIC.to_vec();
    b.push(version);
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
    let long_entry = raw(0, 1, &raw_leaf_entry(b"a", &[0; MAX_ENTRY]));
    // 24 entries of 1,500 bytes: each within MAX_ENTRY, the node above MAX_NODE_MAX.
    let big_node = raw(0, 24, &(0..24u8).map(|i| raw_leaf_entry(&[b'a', i], &[0; 1_494])).collect::<Vec<_>>().concat());
    let long_child_key = encode_internal(1, &[Child { last_key: vec![b'a'; MAX_KEY + 1], id: leaf_id, count: 1 }]);
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
        ("entry above MAX_ENTRY", long_entry),
        ("node above MAX_NODE_MAX", big_node),
        ("child key above MAX_KEY", long_child_key),
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
    // The good leaf under a parent entry that fits it (last key, count) still reads.
    let ok = s.put(&encode_internal(1, &[Child { last_key: b"a".to_vec(), id: leaf_id, count: 1 }])).unwrap();
    assert_eq!(get(&s, &ok, b"a").unwrap(), Some(b"1".to_vec()));
}

/// Trees whose nodes are each valid but do not fit their parent entries:
/// each read and apply that loads the bad node gives `Error::Corrupt`.
#[test]
fn inconsistent_tree_is_an_error() {
    use crate::node::{encode_internal, encode_leaf, Child};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let s = mem_store();
    let leaf = |es: &[&[u8]]| s.put(&encode_leaf(&es.iter().map(|k| (k.to_vec(), b"v".to_vec())).collect::<Vec<_>>())).unwrap();
    let entry = |k: &[u8], id, count| Child { last_key: k.to_vec(), id, count };
    let a = leaf(&[b"a", b"m"]);
    let b = leaf(&[b"b", b"z"]);
    let good = leaf(&[b"n", b"z"]);
    let level1 = |cs: &[Child]| s.put(&encode_internal(1, cs)).unwrap();
    // (what, root, a key whose path holds the bad node)
    let cases = [
        // The example of the review: B holds "b", which is below A's last key "m".
        ("first key not above the previous sibling", level1(&[entry(b"m", a, 2), entry(b"z", b, 2)]), b"y" as &[u8]),
        ("last key is not the entry's key", level1(&[entry(b"q", a, 2), entry(b"z", good, 2)]), b"a"),
        ("leaf count is not the entry's count", level1(&[entry(b"m", a, 5), entry(b"z", good, 2)]), b"a"),
        (
            "subtree count is not the entry's count",
            s.put(&encode_internal(2, &[entry(b"z", level1(&[entry(b"m", a, 2), entry(b"z", good, 2)]), 3)])).unwrap(),
            b"a",
        ),
    ];
    for (what, root, key) in cases {
        let got = [
            catch_unwind(AssertUnwindSafe(|| get(&s, &root, key).map(|_| ()))),
            catch_unwind(AssertUnwindSafe(|| iter(&s, &root).and_then(|c| c.collect::<Result<Vec<_>>>()).map(|_| ()))),
            catch_unwind(AssertUnwindSafe(|| walk(&s, &root, |_| {}))),
            catch_unwind(AssertUnwindSafe(|| apply(&s, Config::SMALL, &root, &[put(key, b"new")]).map(|_| ()))),
        ];
        for (i, g) in got.into_iter().enumerate() {
            assert!(matches!(g, Ok(Err(Error::Corrupt(_)))), "{what}, check {i}: {g:?}");
        }
    }
    // The case of the review in apply: a put into A, whose rewrite runs on
    // into B (A is below NODE_MIN, so no cut at its end), is an error, and
    // no root that reads back wrong is returned.
    let root = level1(&[entry(b"m", a, 2), entry(b"z", b, 2)]);
    let got = catch_unwind(AssertUnwindSafe(|| apply(&s, Config::SMALL, &root, &[put(b"c", b"new")])));
    assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{got:?}");
    // A get whose path holds only good nodes does not see B (the check is per path).
    assert_eq!(get(&s, &root, b"a").unwrap(), Some(b"v".to_vec()));
}

/// A node of a larger config is refused by an apply of a smaller one: its
/// entries could not be written again within the smaller size bound.
#[test]
fn foreign_config_node_is_an_error_in_apply() {
    let s = mem_store();
    let (root, _) = build(&s, Config::DEFAULT, [(b"k".to_vec(), vec![7; 300])]).unwrap();
    let before = s.stats();
    let got = apply(&s, Config::SMALL, &root, &[put(b"a", b"1")]);
    assert!(matches!(got, Err(Error::Corrupt(_))), "{got:?}");
    assert_eq!(s.stats(), before);
    // The readers use the largest limits, so they read it.
    assert_eq!(get(&s, &root, b"k").unwrap(), Some(vec![7; 300]));
    // An apply with the config of the tree works.
    let (r, _) = apply(&s, Config::DEFAULT, &root, &[put(b"a", b"1")]).unwrap();
    assert_eq!(get(&s, &r, b"a").unwrap(), Some(b"1".to_vec()));
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

// ---- apply (plan 24-02) ----

type Model = std::collections::BTreeMap<Vec<u8>, Vec<u8>>;

fn put(k: &[u8], v: &[u8]) -> Edit {
    Edit::Put { key: k.to_vec(), value: v.to_vec() }
}

fn edit_model(m: &mut Model, batch: &[Edit]) {
    for e in batch {
        match e {
            Edit::Put { key, value } => m.insert(key.clone(), value.clone()),
            Edit::Delete { key } => m.remove(key),
        };
    }
}

/// Applies `batch` to `root` (the tree of `m`), updates `m`, and checks
/// that the new root is the bulk root of the new set.
fn apply_checked(
    s: &ChunkStore,
    cfg: Config,
    root: &chunk_store::ObjectId,
    m: &mut Model,
    batch: &[Edit],
) -> (chunk_store::ObjectId, ApplyStats) {
    let (r, st) = apply(s, cfg, root, batch).unwrap();
    edit_model(m, batch);
    let (bulk, _) = build(s, cfg, m.iter()).unwrap();
    assert_eq!(r, bulk, "apply root is not the bulk root ({} entries, {} edits)", m.len(), batch.len());
    (r, st)
}

#[test]
fn apply_one_put_then_get() {
    let s = mem_store();
    let es = u64_entries(3_000, 3);
    let (root, _) = build(&s, Config::DEFAULT, es.clone()).unwrap();
    let mut m: Model = es.iter().cloned().collect();
    let (old_k, new_k) = (es[1_500].0.clone(), 4u64.to_be_bytes().to_vec());
    let batch = [put(&new_k, b"new"), put(&old_k, &[7; 33])];
    let (r, _) = apply_checked(&s, Config::DEFAULT, &root, &mut m, &batch);
    assert_eq!(get(&s, &r, &old_k).unwrap(), Some(vec![7; 33]));
    assert_eq!(get(&s, &r, &new_k).unwrap(), Some(b"new".to_vec()));
    assert_eq!(get(&s, &r, &es[10].0).unwrap(), Some(es[10].1.clone()));
    assert_eq!(entries_of(&s, &r), m.into_iter().collect::<Vec<_>>());
}

#[test]
fn one_change_writes_one_path() {
    for (cfg, n) in [(Config::DEFAULT, 20_000), (Config::SMALL, 5_000)] {
        let s = mem_store();
        let es = u64_entries(n, 3);
        let (root, bst) = build(&s, cfg, es.clone()).unwrap();
        let before = s.stats().objects;
        let (r, st) = apply(&s, cfg, &root, &[put(&es[n / 2].0, &[0xab; 33])]).unwrap();
        assert_ne!(r, root);
        assert_eq!(s.stats().objects - before, bst.depth as u64, "{cfg:?}");
        assert_eq!(st.nodes_written, bst.depth as u64, "{cfg:?}: {st:?}");
    }
}

#[test]
fn apply_inserts_equal_bulk() {
    let cfg = Config::SMALL;
    let s = mem_store();
    let es = u64_entries(2_000, 3);
    let (root, _) = build(&s, cfg, es.clone()).unwrap();
    let base: Model = es.iter().cloned().collect();
    let k = |x: u64| x.to_be_bytes().to_vec();
    let mut seed = 77u64;
    let mut random: Vec<u64> = (0..300).map(|_| xorshift(&mut seed) % 7_000).collect();
    random.sort_unstable();
    random.dedup();
    let batches: Vec<Vec<Edit>> = vec![
        vec![put(&k(0), b"first"), put(&k(1), b"")],
        vec![put(&k(3 + 3 * 2_000), b"last"), put(&k(u64::MAX), &[1; 40])],
        vec![put(&k(3 * 1_000 + 4), b"middle")],
        random.iter().map(|&x| put(&k(x), &x.to_le_bytes())).collect(),
    ];
    for b in &batches {
        let mut m = base.clone();
        apply_checked(&s, cfg, &root, &mut m, b);
    }
    // A small tree grows several levels in one batch.
    let small = u64_entries(50, 401);
    let (r50, st50) = build(&s, cfg, small.clone()).unwrap();
    let mut m: Model = small.into_iter().collect();
    let grow: Vec<Edit> = u64_entries(20_000, 1)
        .into_iter()
        .filter(|(k, _)| !m.contains_key(k))
        .map(|(key, value)| Edit::Put { key, value })
        .collect();
    let (r, _) = apply_checked(&s, cfg, &r50, &mut m, &grow);
    assert!(depth_of(&s, &r) >= st50.depth + 2, "depth {} from {}", depth_of(&s, &r), st50.depth);
}

fn depth_of(s: &ChunkStore, root: &chunk_store::ObjectId) -> u8 {
    nodes(s, root).iter().map(|n| n.level).max().unwrap_or(0) + 1
}

fn del(k: &[u8]) -> Edit {
    Edit::Delete { key: k.to_vec() }
}

fn dels<'a>(keys: impl IntoIterator<Item = &'a Vec<u8>>) -> Vec<Edit> {
    keys.into_iter().map(|k| del(k)).collect()
}

fn puts(es: &[(Vec<u8>, Vec<u8>)]) -> Vec<Edit> {
    es.iter().map(|(k, v)| put(k, v)).collect()
}

#[test]
fn canonical_root() {
    let cfg = Config::SMALL;
    let s = mem_store();
    let es = u64_entries(2_000, 3);
    let (root, bst) = build(&s, cfg, es.clone()).unwrap();
    let empty = empty_root(&s).unwrap();
    // (a) no keys; (b) one key; (c) two keys.
    for keep in [0usize, 1, 2] {
        let mut m: Model = es.iter().cloned().collect();
        let (r, st) = apply_checked(&s, cfg, &root, &mut m, &dels(es[keep..].iter().map(|(k, _)| k)));
        assert_eq!(m.len(), keep);
        // The batch itself (8-byte keys) counts in the high water.
        assert!(st.high_water_bytes >= 8 * (es.len() - keep), "{st:?}");
        if keep == 0 {
            assert_eq!(r, empty);
        }
    }
    // (d) a whole range of leaves goes, and the height goes down.
    let mut m: Model = es.iter().cloned().collect();
    apply_checked(&s, cfg, &root, &mut m, &dels(es[500..1_500].iter().map(|(k, _)| k)));
    let mut m: Model = es.iter().cloned().collect();
    let (r, _) = apply_checked(&s, cfg, &root, &mut m, &dels(es[30..].iter().map(|(k, _)| k)));
    assert!(depth_of(&s, &r) < bst.depth, "depth {} from {}", depth_of(&s, &r), bst.depth);
    // (e) a delete of an absent key from the empty map writes nothing.
    let before = s.stats();
    assert_eq!(apply(&s, cfg, &empty, &[del(b"x")]).unwrap().0, empty);
    assert_eq!(s.stats(), before);
}

#[test]
fn order_independence() {
    let cfg = Config::SMALL;
    let es = u64_entries(3_000, 3);
    let n = es.len();
    let mut roots = Vec::new();
    // (1) bulk.
    roots.push(build(&mem_store(), cfg, es.clone()).unwrap().0);
    // (2) one put per batch, in a shuffled order.
    let s = mem_store();
    let mut order: Vec<usize> = (0..n).collect();
    let mut seed = 2_024u64;
    for i in (1..n).rev() {
        order.swap(i, (xorshift(&mut seed) % (i as u64 + 1)) as usize);
    }
    let mut r = empty_root(&s).unwrap();
    for i in order {
        r = apply(&s, cfg, &r, &puts(&es[i..=i])).unwrap().0;
    }
    roots.push(r);
    // (3) 7 batches in key order; (4) 13 batches in reverse key order.
    for (parts, reverse) in [(7, false), (13, true)] {
        let s = mem_store();
        let mut ranges: Vec<_> = (0..parts).map(|p| p * n / parts..(p + 1) * n / parts).collect();
        if reverse {
            ranges.reverse();
        }
        let mut r = empty_root(&s).unwrap();
        for range in ranges {
            r = apply(&s, cfg, &r, &puts(&es[range])).unwrap().0;
        }
        roots.push(r);
    }
    // (5) a superset, then the extra keys deleted in 3 batches.
    let s = mem_store();
    let extra: Vec<Vec<u8>> = (0..1_000u64).map(|i| (i * 9 + 4).to_be_bytes().to_vec()).collect();
    let mut sup: Model = es.iter().cloned().collect();
    sup.extend(extra.iter().map(|k| (k.clone(), b"extra".to_vec())));
    let (mut r, _) = build(&s, cfg, sup.iter()).unwrap();
    for part in extra.chunks(334) {
        r = apply(&s, cfg, &r, &dels(part)).unwrap().0;
    }
    roots.push(r);
    assert!(roots.iter().all(|r| *r == roots[0]), "{:?}", roots.iter().map(|r| r.to_hex()).collect::<Vec<_>>());
}

#[test]
fn same_batch_twice_writes_nothing() {
    let cfg = Config::DEFAULT;
    let s = mem_store();
    let es = u64_entries(3_000, 3);
    let (root, _) = build(&s, cfg, es.clone()).unwrap();
    // 50 mixed edits: puts of new and old keys, deletes of old and absent keys.
    let mut b: Vec<Edit> = (0..50u64)
        .map(|i| {
            let k = (i * 179 + 3 + i % 3).to_be_bytes().to_vec();
            if i % 2 == 0 {
                put(&k, &i.to_le_bytes())
            } else {
                del(&k)
            }
        })
        .collect();
    b.sort_by(|x, y| x.key().cmp(y.key()));
    let (r1, _) = apply(&s, cfg, &root, &b).unwrap();
    assert_ne!(r1, root);
    let before = s.stats();
    let (r2, _) = apply(&s, cfg, &r1, &b).unwrap();
    assert_eq!(r1, r2);
    let after = s.stats();
    assert_eq!((after.objects, after.raw_bytes), (before.objects, before.raw_bytes));
    // A put of the current value changes nothing.
    let (r3, st) = apply(&s, cfg, &root, &puts(&es[700..701])).unwrap();
    assert_eq!(r3, root);
    assert_eq!(s.stats(), after);
    assert!(st.nodes_read > 0, "{st:?}");
}

#[test]
fn apply_rejects_unsorted_batch() {
    let cfg = Config::SMALL;
    let s = mem_store();
    let (root, _) = build(&s, cfg, u64_entries(500, 3)).unwrap();
    // A root that is not in the store: an error from a read would be
    // `Error::Store`, so these errors come before any read.
    let missing = chunk_store::ObjectId([0x1e; chunk_store::ID_LEN]);
    let before = s.stats();
    for r in [root, missing] {
        assert!(matches!(apply(&s, cfg, &r, &[put(b"b", b""), put(b"a", b"")]), Err(Error::Unsorted)));
        assert!(matches!(apply(&s, cfg, &r, &[put(b"a", b""), del(b"a")]), Err(Error::Unsorted)));
        assert!(matches!(apply(&s, cfg, &r, &[put(b"a", &[0; 300])]), Err(Error::EntryTooLarge { len: 305, max: 256 })));
        assert!(matches!(apply(&s, cfg, &r, &[del(&[b'k'; 129])]), Err(Error::KeyTooLarge { len: 129, max: 128 })));
        assert_eq!(apply(&s, cfg, &r, &[]).unwrap(), (r, ApplyStats::default()));
    }
    assert_eq!(s.stats(), before);
}

/// A directory-style key (D-10) made from `x`: parent u64 (big endian),
/// then a name of 1 to 40 bytes.
fn var_key(x: u64) -> Vec<u8> {
    let mut s = x | 1;
    let mut k = (x % 64).to_be_bytes().to_vec();
    let name_len = 1 + (x >> 8) as usize % 40;
    k.extend((0..name_len).map(|_| b'a' + (xorshift(&mut s) % 26) as u8));
    k
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        failure_persistence: None,
        ..proptest::prelude::ProptestConfig::with_cases(48)
    })]

    #[test]
    fn variable_length_model(
        n in 0usize..400,
        seed in proptest::prelude::any::<u64>(),
        batches in proptest::collection::vec(
            proptest::collection::vec((0u8..4, proptest::prelude::any::<u64>(), 0usize..201), 1..40),
            1..5,
        ),
    ) {
        let cfg = Config::SMALL;
        let s = mem_store();
        let mut m: Model = var_entries(n, seed).into_iter().collect();
        let (mut root, _) = build(&s, cfg, m.iter()).unwrap();
        for ops in batches {
            let mut b: Vec<Edit> = ops
                .into_iter()
                .map(|(kind, x, vlen)| {
                    let old = (!m.is_empty()).then(|| m.keys().nth(x as usize % m.len()).cloned()).flatten();
                    let key = match (kind, old) {
                        (0 | 2, Some(k)) => k,
                        _ => var_key(x),
                    };
                    let value: Vec<u8> = (0..vlen).map(|i| (x >> (i % 8)) as u8).collect();
                    if kind < 2 { Edit::Put { key, value } } else { Edit::Delete { key } }
                })
                .collect();
            b.sort_by(|x, y| x.key().cmp(y.key()));
            b.dedup_by(|x, y| x.key() == y.key());
            root = apply_checked(&s, cfg, &root, &mut m, &b).0;
            assert_eq!(entries_of(&s, &root), m.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
            let mut sx = seed | 1;
            for i in 0..20u64 {
                let k = match m.keys().nth(xorshift(&mut sx) as usize % m.len().max(1)) {
                    Some(k) if i % 2 == 0 => k.clone(),
                    _ => var_key(xorshift(&mut sx)),
                };
                assert_eq!(get(&s, &root, &k).unwrap(), m.get(&k).cloned());
            }
        }
    }
}

#[test]
fn apply_memory_is_bounded() {
    let cfg = Config::SMALL;
    let s = mem_store();
    let es = u64_entries(40_000, 3);
    let (root, bst) = build(&s, cfg, es.clone()).unwrap();
    let total = nodes(&s, &root).len() as u64;
    let (depth, max) = (bst.depth as usize, cfg.node_max());
    let mut m: Model = es.iter().cloned().collect();
    let ten: Vec<Edit> = (0..10).map(|i| put(&es[i * 4_000 + 17].0, &[0x5a; 33])).collect();
    let (_, st) = apply_checked(&s, cfg, &root, &mut m, &ten);
    assert!(st.nodes_read <= (10 * depth * (depth + 2)) as u64, "{st:?}, depth {depth}");
    assert!(st.nodes_read < total / 4, "{st:?}, {total} nodes");
    assert!(st.high_water_bytes <= 4 * (depth + 1) * max, "{st:?}, depth {depth}");
    let mut m: Model = es.iter().cloned().collect();
    let many: Vec<Edit> = (0..1_000u64).map(|i| put(&(i * 120 + 4).to_be_bytes(), &[0x33; 33])).collect();
    let batch_bytes: usize = many.len() * (8 + 33);
    let (_, st) = apply_checked(&s, cfg, &root, &mut m, &many);
    let bound = 4 * (depth + 1) * max + 2 * (depth + 1) * batch_bytes;
    assert!(st.high_water_bytes <= bound, "{st:?}, bound {bound}");
}

#[test]
fn malformed_node_is_an_error_in_apply() {
    use crate::node::{encode_internal, encode_leaf, Child};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let s = mem_store();
    let good_leaf = encode_leaf(&[(b"a".to_vec(), b"1".to_vec())]);
    let leaf_id = s.put(&good_leaf).unwrap();
    let mut wrong_magic = good_leaf.clone();
    wrong_magic[0] = b'X';
    let out_of_order = raw(0, 2, &[raw_leaf_entry(b"b", b""), raw_leaf_entry(b"a", b"")].concat());
    let child = |id, count| Child { last_key: b"zz".to_vec(), id, count };
    let wrong_level = encode_internal(2, &[child(leaf_id, 1)]);
    // Two subtree counts whose sum is above u64::MAX.
    let leaf_b = s.put(&encode_leaf(&[(b"zzz".to_vec(), b"2".to_vec())])).unwrap();
    let big = |k: &[u8], id| Child { last_key: k.to_vec(), id, count: u64::MAX };
    let leaf_m = s.put(&encode_leaf(&[(b"m".to_vec(), b"3".to_vec())])).unwrap();
    let last = Child { last_key: b"zzz".to_vec(), id: leaf_b, count: 1 };
    let huge_counts = encode_internal(1, &[big(b"a", leaf_id), big(b"m", leaf_m), last]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("wrong magic", wrong_magic),
        ("child at the wrong level", wrong_level),
        ("keys out of order", out_of_order),
        ("count above the bytes", raw(0, u32::MAX, &[0; 11])),
        ("subtree counts above u64", huge_counts),
    ];
    // The key goes to the last node of each level.
    let batch = [put(b"zzzz", b"new")];
    for (what, bytes) in cases {
        let bad = s.put(&bytes).unwrap();
        let parent = s.put(&encode_internal(if bytes[5] == 0 { 1 } else { bytes[5] + 1 }, &[child(bad, 1)])).unwrap();
        for (place, root) in [("root", bad), ("child", parent)] {
            let got = catch_unwind(AssertUnwindSafe(|| apply(&s, Config::SMALL, &root, &batch)));
            assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{what} as {place}: {got:?}");
        }
    }
}

#[test]
fn node_header_layout() {
    use crate::node::encode_leaf;
    let s = mem_store();
    let empty = s.read(&empty_root(&s).unwrap()).unwrap();
    assert_eq!(empty.as_slice(), [b'S', b'P', b'T', b'1', 1, 0, 0, 0, 0, 0]);
    assert_eq!(empty.len(), NODE_HEADER);
    let one = encode_leaf(&[(b"k".to_vec(), b"v".to_vec())]);
    assert_eq!(&one[..10], [b'S', b'P', b'T', b'1', 1, 0, 1, 0, 0, 0]);
}

#[test]
fn rule_version_is_refused() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let s = mem_store();
    for version in [0u8, 2] {
        let bytes = raw_v(version, 0, 1, &raw_leaf_entry(b"a", b"1"));
        let root = s.put(&bytes).unwrap();
        let is_refused = |what: &str, got: std::thread::Result<Result<()>>| match got {
            Ok(Err(Error::RuleVersion { found })) => assert_eq!(found, version, "{what}"),
            other => panic!("{what} with version {version}: {other:?}"),
        };
        is_refused("get", catch_unwind(AssertUnwindSafe(|| get(&s, &root, b"a").map(|_| ()))));
        is_refused("iter", catch_unwind(AssertUnwindSafe(|| iter(&s, &root).map(|_| ()))));
        is_refused("walk", catch_unwind(AssertUnwindSafe(|| walk(&s, &root, |_| {}))));
        let before = s.stats().objects;
        is_refused("apply", catch_unwind(AssertUnwindSafe(|| apply(&s, Config::DEFAULT, &root, &[put(b"b", b"2")]).map(|_| ()))));
        assert_eq!(s.stats().objects, before, "apply wrote an object");
    }
}

#[test]
fn config_new_limits() {
    for (l, m) in [(4096, 16384), (8192, 16384), (8192, 32768), (256, 4096), (16384, 32768)] {
        assert!(Config::new(l, m).is_some(), "({l}, {m})");
    }
    for (l, m) in [(255, 16384), (16385, 16384), (4096, 4095), (4096, 32769)] {
        assert!(Config::new(l, m).is_none(), "({l}, {m})");
    }
    assert_eq!(Config::new(8192, 16384), Some(Config::DEFAULT));
    assert_eq!(Config::READ.node_max(), MAX_NODE_MAX);
}

#[test]
fn salted_tree_differs() {
    let s = mem_store();
    let es = u64_entries(5_000, 7);
    let salted = Config::DEFAULT.with_salt(Some([7u8; 32]));
    assert_eq!(salted.salt(), Some(&[7u8; 32]));
    assert_eq!(Config::DEFAULT.salt(), None);
    let (plain_root, _) = build(&s, Config::DEFAULT, es.iter().map(|(k, v)| (k, v))).unwrap();
    let (salted_root, _) = build(&s, salted, es.iter().map(|(k, v)| (k, v))).unwrap();
    assert_ne!(plain_root, salted_root);
    // Reads take no salt.
    assert_eq!(entries_of(&s, &salted_root), es);
    for (k, v) in &es {
        assert_eq!(get(&s, &salted_root, k).unwrap().as_deref(), Some(v.as_slice()));
    }
    assert_eq!((boundary_hash(0, b""), boundary_hash(1, b"key")), (527_629_384, 94_351_618));
    assert_ne!(boundary_hash_keyed(&[7u8; 32], 0, b""), boundary_hash(0, b""));
}

#[test]
fn wide_config_tree_reads() {
    let wide = Config::new(8192, 32768).unwrap();
    for (what, es) in [("u64", u64_entries(40_000, 3)), ("var", var_entries(3_000, 5))] {
        let s = mem_store();
        let (root, st) = build(&s, wide, es.iter().map(|(k, v)| (k, v))).unwrap();
        assert_eq!(st.entries, es.len() as u64);
        for (k, v) in &es {
            assert_eq!(get(&s, &root, k).unwrap().as_deref(), Some(v.as_slice()), "{what}");
        }
        assert_eq!(entries_of(&s, &root), es, "{what}");
        let largest = nodes(&s, &root).iter().map(|n| n.bytes).max().unwrap();
        assert!(largest <= MAX_NODE_MAX, "{what}: largest node {largest}");
    }
    // A scale of 16 KiB makes nodes above 16 KiB, which `Config::READ` accepts.
    let wider = Config::new(16_384, 32_768).unwrap();
    let s = mem_store();
    let es = u64_entries(40_000, 3);
    let (root, _) = build(&s, wider, es.iter().map(|(k, v)| (k, v))).unwrap();
    assert_eq!(entries_of(&s, &root), es);
    let largest = nodes(&s, &root).iter().map(|n| n.bytes).max().unwrap();
    assert!(largest > NODE_MAX && largest <= MAX_NODE_MAX, "largest node {largest}");
}

#[test]
fn forced_cuts_are_counted() {
    let s = mem_store();
    let es = u64_entries(20_000, 3);
    let (_, st) = build(&s, Config::DEFAULT, es.iter().map(|(k, v)| (k, v))).unwrap();
    assert_eq!(st.forced_cuts, 0);
    // L far above node_max: nearly every cut is forced.
    let forced = Config::new(16_384, 4_096).unwrap();
    let (root, st) = build(&s, forced, es.iter().map(|(k, v)| (k, v))).unwrap();
    assert!(st.forced_cuts >= st.leaves / 2, "{} forced cuts, {} leaves", st.forced_cuts, st.leaves);
    let batch: Vec<Edit> = (0..100u64).map(|i| put(&(i * 600 + 4).to_be_bytes(), b"new")).collect();
    let (_, ast) = apply(&s, forced, &root, &batch).unwrap();
    assert!(ast.forced_cuts >= 1, "{ast:?}");
}

// ---- range, count, height, visit (plan 26-02) ----

/// The entries of `es` with `start <= key < end`.
fn filtered(es: &[(Vec<u8>, Vec<u8>)], start: &[u8], end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    es.iter().filter(|(k, _)| k.as_slice() >= start && end.is_none_or(|e| k.as_slice() < e)).cloned().collect()
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        failure_persistence: None,
        ..proptest::prelude::ProptestConfig::with_cases(40)
    })]

    #[test]
    fn range_equals_filtered_iter(
        n in 0usize..3_000,
        seed in proptest::prelude::any::<u64>(),
        a in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..12),
        b in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..12),
        sel in 0u8..9,
    ) {
        let s = mem_store();
        let es = var_entries(n, seed);
        let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
        let ctx = Ctx::uncached(&s);
        let existing = |x: u64| es.get(x as usize % es.len().max(1)).map(|(k, _)| k.clone());
        let start = match (sel % 3, existing(seed)) {
            (0, Some(k)) => k,
            (1, _) => Vec::new(),
            _ => a,
        };
        let end = match (sel / 3, existing(seed >> 17)) {
            (0, _) => None,
            (1, Some(k)) => Some(k),
            _ => Some(b),
        };
        let got = range(&ctx, &root, &start, end.as_deref()).unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(got, filtered(&es, &start, end.as_deref()));
        // Through a small cache the result is the same.
        let cache = NodeCache::new(8 * 1024);
        let cached = Ctx::new(&s, Some(&cache));
        let got = range(&cached, &root, &start, end.as_deref()).unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(got, filtered(&es, &start, end.as_deref()));
    }
}

#[test]
fn range_edges() {
    let s = mem_store();
    let ctx = Ctx::uncached(&s);
    let es = u64_entries(3_000, 5);
    let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
    let all = |start: &[u8], end: Option<&[u8]>| range(&ctx, &root, start, end).unwrap().collect::<Result<Vec<_>>>().unwrap();
    assert_eq!(all(b"", None), es);
    // Start above the last key, an empty range, and end below start.
    assert!(all(&u64::MAX.to_be_bytes(), None).is_empty());
    assert!(all(&es[10].0, Some(&es[10].0)).is_empty());
    assert!(all(&es[10].0, Some(&es[5].0)).is_empty());
    // The start key is in, the end key is out.
    assert_eq!(all(&es[10].0, Some(&es[12].0)), es[10..12].to_vec());
    // A start between keys.
    let between = 4u64.to_be_bytes();
    assert_eq!(all(&between, Some(&es[2].0))[0], es[1]);
    // The empty tree.
    let empty = empty_root(&s).unwrap();
    assert!(range(&ctx, &empty, b"", None).unwrap().next().is_none());
}

#[test]
fn range_holds_one_path() {
    let s = mem_store();
    let ctx = Ctx::uncached(&s);
    let (root, st) = build(&s, Config::SMALL, u64_entries(40_000, 3)).unwrap();
    assert!(st.depth >= 3, "depth {}", st.depth);
    let depth = st.depth as usize;
    let mut r = range(&ctx, &root, &20_000u64.to_be_bytes(), None).unwrap();
    let mut seen = 0;
    for e in r.by_ref() {
        e.unwrap();
        seen += 1;
    }
    assert!(seen > 10_000);
    assert_eq!(r.max_nodes_held(), depth);
    let mut r = range(&ctx, &root, &1_000u64.to_be_bytes(), Some(&2_000u64.to_be_bytes())).unwrap();
    assert!(r.by_ref().all(|e| e.is_ok()));
    assert!(r.max_nodes_held() <= depth);
}

#[test]
fn count_and_height_read_one_node() {
    for cfg in [Config::SMALL, Config::DEFAULT] {
        for n in [0usize, 1, 50, 3_000, 40_000] {
            let s = mem_store();
            let (root, st) = build(&s, cfg, u64_entries(n, 3)).unwrap();
            let cache = NodeCache::new(1 << 20);
            let ctx = Ctx::new(&s, Some(&cache));
            assert_eq!(count(&ctx, &root).unwrap(), n as u64, "{n}");
            assert_eq!(cache.stats().misses, 1, "count reads the root only");
            let cache = NodeCache::new(1 << 20);
            let ctx = Ctx::new(&s, Some(&cache));
            assert_eq!(height(&ctx, &root).unwrap(), st.depth, "{n}");
            assert_eq!(cache.stats().misses, 1, "height reads the root only");
            let top = nodes(&s, &root).iter().map(|x| x.level).max().unwrap();
            assert_eq!(st.depth, top + 1);
            assert_eq!(iter(&s, &root).unwrap().count() as u64, n as u64);
        }
    }
}

/// The node IDs of a tree by a plain recursion over the stored bytes
/// (parent first, children left to right).
fn reference_ids(s: &ChunkStore, id: &chunk_store::ObjectId, out: &mut Vec<(chunk_store::ObjectId, u8)>) {
    let node = crate::node::decode(&s.read(id).unwrap(), &Config::READ).unwrap();
    out.push((*id, node.level()));
    if let crate::node::Node::Internal { children, .. } = node {
        for c in children {
            reference_ids(s, &c.id, out);
        }
    }
}

#[test]
fn visit_yields_each_node_once() {
    for (cfg, n) in [(Config::SMALL, 20_000), (Config::DEFAULT, 20_000), (Config::SMALL, 30)] {
        let s = mem_store();
        let ctx = Ctx::uncached(&s);
        let es = u64_entries(n, 3);
        let (root, _) = build(&s, cfg, es.iter().map(|(k, v)| (k, v))).unwrap();
        let mut got = Vec::new();
        let mut leaves = Vec::new();
        let mut counts = Vec::new();
        let done = visit(&ctx, &root, &mut |v| {
            got.push((v.id, v.level));
            counts.push((v.level, v.children, v.entries.len()));
            leaves.extend(v.entries.iter().cloned());
            true
        })
        .unwrap();
        assert!(done);
        let mut want = Vec::new();
        reference_ids(&s, &root, &mut want);
        assert_eq!(got, want, "{n}: same nodes in the same order");
        assert_eq!(got[0].0, root);
        // The levels are the levels of `walk`, in the same order.
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), nodes(&s, &root).iter().map(|x| x.level).collect::<Vec<_>>());
        assert_eq!(leaves, es);
        // A leaf has no children; an internal node has no entries.
        assert!(counts.iter().all(|&(level, c, e)| if level == 0 { c == 0 } else { e == 0 && c > 0 }));
        // Every ID once (no node of a built tree is shared).
        let mut ids: Vec<_> = got.iter().map(|g| g.0).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), got.len(), "{n}");
    }
}

#[test]
fn visit_stops_when_asked() {
    let s = mem_store();
    let ctx = Ctx::uncached(&s);
    let (root, st) = build(&s, Config::SMALL, u64_entries(5_000, 3)).unwrap();
    assert!(st.depth >= 2);
    let mut calls = 0;
    let done = visit(&ctx, &root, &mut |_| {
        calls += 1;
        calls < 3
    })
    .unwrap();
    assert!(!done);
    assert_eq!(calls, 3);
    // The empty tree is one leaf with no entries.
    let empty = empty_root(&s).unwrap();
    let mut seen = Vec::new();
    assert!(visit(&ctx, &empty, &mut |v| {
        seen.push((v.id, v.level, v.children, v.entries.len()));
        true
    })
    .unwrap());
    assert_eq!(seen, vec![(empty, 0, 0, 0)]);
}

/// Trees that are damaged or crafted give `Error::Corrupt` from the new
/// readers, with a cache or without, and never a panic.
#[test]
fn crafted_trees_are_errors_in_range_count_visit() {
    use crate::node::{encode_internal, encode_leaf, Child};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let s = mem_store();
    let leaf = |es: &[&[u8]]| s.put(&encode_leaf(&es.iter().map(|k| (k.to_vec(), b"v".to_vec())).collect::<Vec<_>>())).unwrap();
    let entry = |k: &[u8], id, count| Child { last_key: k.to_vec(), id, count };
    let a = leaf(&[b"a", b"m"]);
    let b = leaf(&[b"b", b"z"]);
    let good = leaf(&[b"n", b"z"]);
    let level1 = |cs: &[Child]| s.put(&encode_internal(1, cs)).unwrap();
    let cache = NodeCache::new(1 << 20);
    for ctx in [Ctx::uncached(&s), Ctx::new(&s, Some(&cache))] {
        // (what, root): a bad node on the first path of range and visit.
        let cases = [
            ("first key not above the previous sibling", level1(&[entry(b"m", a, 2), entry(b"z", b, 2)])),
            ("last key is not the entry's key", level1(&[entry(b"q", a, 2), entry(b"z", good, 2)])),
            ("leaf count is not the entry's count", level1(&[entry(b"m", a, 5), entry(b"z", good, 2)])),
            ("child at the wrong level", s.put(&encode_internal(2, &[entry(b"m", a, 2)])).unwrap()),
        ];
        for (what, root) in cases {
            let got = [
                catch_unwind(AssertUnwindSafe(|| range(&ctx, &root, b"", None).and_then(|r| r.collect::<Result<Vec<_>>>()).map(|_| ()))),
                catch_unwind(AssertUnwindSafe(|| visit(&ctx, &root, &mut |_| true).map(|_| ()))),
            ];
            for (i, g) in got.into_iter().enumerate() {
                assert!(matches!(g, Ok(Err(Error::Corrupt(_)))), "{what}, check {i}: {g:?}");
            }
        }
        // Counts that overflow u64: count gives Corrupt.
        let big = s.put(&encode_internal(1, &[entry(b"m", a, u64::MAX), entry(b"z", good, u64::MAX)])).unwrap();
        let got = catch_unwind(AssertUnwindSafe(|| count(&ctx, &big)));
        assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{got:?}");
        // A bad root node.
        let bad = s.put(&raw(0, 1, &[10, 0, 0, 0, b'a'])).unwrap();
        for got in [
            catch_unwind(AssertUnwindSafe(|| range(&ctx, &bad, b"", None).map(|_| ()))),
            catch_unwind(AssertUnwindSafe(|| count(&ctx, &bad).map(|_| ()))),
            catch_unwind(AssertUnwindSafe(|| height(&ctx, &bad).map(|_| ()))),
            catch_unwind(AssertUnwindSafe(|| visit(&ctx, &bad, &mut |_| true).map(|_| ()))),
        ] {
            assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{got:?}");
        }
        // A range that starts past the bad child does not see it.
        let root = level1(&[entry(b"m", a, 5), entry(b"z", good, 2)]);
        let r = range(&ctx, &root, b"n", None).unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(r.len(), 2);
    }
}
