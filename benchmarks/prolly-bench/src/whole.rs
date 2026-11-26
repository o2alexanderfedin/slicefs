//! The `whole` mode: the real `DictMetadataStore` on a copy of a seeded
//! store. It runs a seeded edit script of small commits and times
//! `begin_commit` and `finish_commit` apart (public API only, D-22).

use std::fs;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chunk_store::ChunkStore;
use metadata::store::DictMetadataStore;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

use crate::{
    hex, map_len, ms, print_disk, root_record, run_fail, rusage, segment_root, serialized_bytes, stats_delta,
    usage, usage_fields, Args, Res, Rng, S_IFDIR, S_IFMT, S_IFREG,
};

/// Most draws of one scatter pick before the bench gives up.
const MAX_DRAWS: u32 = 10_000;

/// The state of one `whole` run.
struct Whole {
    meta: DictMetadataStore,
    cs: Arc<ChunkStore>,
    rng: Rng,
    /// Inode numbers below this exist or existed.
    next_ino: u64,
    roots: fs::File,
}

pub(crate) fn run(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["store", "roots", "seed", "ks", "commits", "patterns", "next-tree"])?;
    let store = a.path("store")?;
    let roots_path = a.path("roots")?;
    let seed = a.num("seed", Some(24))?;
    let ks = a.nums("ks", "1 10 100 1000")?;
    let commits = a.num("commits", Some(10))?;
    let patterns = a.words("patterns", "scatter cluster");
    if let Some(p) = patterns.iter().find(|p| !matches!(p.as_str(), "scatter" | "cluster")) {
        return usage(format!("--patterns: unknown pattern {p}"));
    }

    let root = segment_root(&store)?;
    let io = slicefs_cli::content::store_io(&store)?;
    let cs = Arc::clone(io.store());
    let rr = root_record(&cs, &root)?;
    let meta = DictMetadataStore::load_from_root(Arc::new(Mutex::new(io)), &root)?;
    let sync = Arc::clone(&cs);
    meta.set_pre_root_sync(Arc::new(move || sync.sync().map_err(std::io::Error::other)));
    let entries = {
        let mut r = chunk_store::file::FileReader::open(&cs, metadata::store_io::to_id(&rr.inode_data()))?;
        let b = r.read(&cs, 0, 8)?;
        u64::from_le_bytes(b.as_slice().try_into().map_err(|_| crate::Fail::Run("short inode_data node".into()))?)
    };
    let mut map_bytes = 0;
    for k in &rr.maps {
        map_bytes += map_len(&cs, k)?;
    }
    println!(
        "info inode_data_entries={entries} inode_count_field={} next_ino={} map_bytes={map_bytes} maxrss_after_load={}",
        rr.inode_count,
        rr.next_ino,
        rusage().maxrss_bytes
    );
    print_disk("whole", "start", &store)?;

    let roots = fs::File::create(&roots_path)?;
    let mut w = Whole { meta, cs, rng: Rng::new(seed), next_ino: rr.next_ino, roots };

    // Setup commit (seq 0): the directory of the created files.
    let dir_meta = InodeMeta::new_directory(0, 0, 0, S_IFDIR | 0o755);
    w.meta.create_directory(1, "bench", &dir_meta)?;
    w.next_ino = w.next_ino.max(w.meta.lookup(1, "bench")? + 1);
    w.commit("setup", 0, 0, [0; 4])?;

    let mut seq = 1;
    for pattern in &patterns {
        for &k in &ks {
            for _ in 0..commits {
                let mut kinds = [0u64; 4];
                for _ in 0..k {
                    let ino = w.pick_scatter()?;
                    w.meta.modify_inode(ino, |m| {
                        m.mtime_sec += 1;
                        m.ctime_sec += 1;
                    })?;
                    kinds[0] += 1;
                }
                w.commit(pattern, k, seq, kinds)?;
                seq += 1;
            }
        }
    }

    w.roots.sync_all()?;
    w.cs.close()?;
    print_disk("whole", "end", &store)?;
    Ok(())
}

impl Whole {
    /// A random existing regular file.
    fn pick_scatter(&mut self) -> Res<u64> {
        if self.next_ino <= 2 {
            return run_fail("the store has no file inodes");
        }
        for _ in 0..MAX_DRAWS {
            let ino = 2 + self.rng.next() % (self.next_ino - 2);
            if self.is_file(ino) {
                return Ok(ino);
            }
        }
        run_fail(format!("no regular file found in {MAX_DRAWS} draws"))
    }

    fn is_file(&self, ino: u64) -> bool {
        self.meta.get_inode(ino).is_ok_and(|m| m.mode & S_IFMT == S_IFREG)
    }

    /// One timed commit: `begin_commit` and `finish_commit` apart, with the
    /// CPU, RSS and store stats around both. Appends the root to the roots
    /// file and prints a `commit` line.
    fn commit(&mut self, pattern: &str, k: u64, seq: u64, kinds: [u64; 4]) -> Res<()> {
        let s0 = self.cs.stats();
        let u0 = rusage();
        let t = Instant::now();
        let p = self.meta.begin_commit()?;
        let begin_ms = ms(t);
        let t2 = Instant::now();
        let root = self.meta.finish_commit(p)?;
        let finish_ms = ms(t2);
        let total_ms = ms(t);
        let u1 = rusage();
        let s1 = self.cs.stats();
        let rr = root_record(&self.cs, &root)?;
        let ser = serialized_bytes(&self.cs, &rr)?;
        println!(
            "commit mode=whole pattern={pattern} k={k} seq={seq} edits_meta={} edits_content={} edits_create={} edits_delete={} serialized_bytes={ser} begin_ms={begin_ms:.3} finish_ms={finish_ms:.3} total_ms={total_ms:.3} {} {}",
            kinds[0],
            kinds[1],
            kinds[2],
            kinds[3],
            usage_fields(&u0, &u1),
            stats_delta(&s0, &s1)
        );
        writeln!(self.roots, "{seq} {pattern} {k} {}", hex(&root.0))?;
        Ok(())
    }
}
