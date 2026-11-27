//! The `whole` mode: the real `DictMetadataStore` on a copy of a seeded
//! store. It runs a seeded edit script of small commits, then optionally
//! one version commit, then the `insdel` commits, and times `begin_commit`
//! and `finish_commit` apart (public API only, D-22).

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chunk_store::ChunkStore;
use metadata::store::DictMetadataStore;
use slicefs_cli::content::put_reader;
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

use crate::{
    hex, map_len, ms, print_disk, root_record, run_fail, rusage, segment_root, serialized_bytes, stats_delta,
    usage, usage_fields, Args, Fail, Res, Rng, CONTENT_BYTES, S_IFDIR, S_IFMT, S_IFREG,
};

/// Most draws of one scatter pick before the bench gives up.
const MAX_DRAWS: u32 = 10_000;

/// The directory under the root that holds the created files.
const BENCH_DIR: &str = "bench";

/// The pattern of real inserts and deletes in the middle of the map: each
/// edit deletes a random file of the tree or inserts a file at an inode
/// number that an earlier commit freed. It runs after the version step, so
/// the commits before it are the same as without it.
const INSDEL: &str = "insdel";

/// A file that an `insdel` delete removed: enough to insert it again at
/// the same inode number.
struct Freed {
    ino: u64,
    dir: u64,
    name: String,
    meta: InodeMeta,
    manifest: Vec<ObjectKey>,
}

/// The state of one `whole` run.
struct Whole {
    meta: DictMetadataStore,
    cs: Arc<ChunkStore>,
    rng: Rng,
    /// Inode numbers below this exist or existed.
    next_ino: u64,
    roots: fs::File,
    /// The ino of the `bench` directory.
    bench: u64,
    /// The created files (name, ino), oldest first; a delete takes the oldest.
    created: VecDeque<(String, u64)>,
    /// Files created so far (for the names).
    creates: u64,
    /// The last cluster pick of this commit.
    cluster_at: Option<u64>,
    /// The files that `insdel` deleted and did not insert again, oldest first.
    freed: VecDeque<Freed>,
}

/// The counts of the version step.
#[derive(Default)]
struct Version {
    files_changed: u64,
    files_created: u64,
    files_deleted: u64,
    dirs_created: u64,
    dirs_deleted: u64,
}

pub(crate) fn run(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["store", "roots", "seed", "ks", "commits", "patterns", "next-tree"])?;
    let store = a.path("store")?;
    let roots_path = a.path("roots")?;
    let seed = a.num("seed", Some(24))?;
    let ks = a.nums("ks", "1 10 100 1000")?;
    let commits = a.num("commits", Some(10))?;
    let patterns = a.words("patterns", "scatter cluster insdel");
    if let Some(p) = patterns.iter().find(|p| !matches!(p.as_str(), "scatter" | "cluster" | INSDEL)) {
        return usage(format!("--patterns: unknown pattern {p}"));
    }
    let next_tree = a.opt("next-tree").map(Path::new);
    if let Some(t) = next_tree {
        if !t.is_dir() {
            return usage("--next-tree: not a directory");
        }
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
        u64::from_le_bytes(b.as_slice().try_into().map_err(|_| Fail::Run("short inode_data node".into()))?)
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

    // Setup commit (seq 0): the directory of the created files.
    let roots = fs::File::create(&roots_path)?;
    let bench = meta.create_directory(1, BENCH_DIR, &InodeMeta::new_directory(0, 0, 0, S_IFDIR | 0o755))?;
    let mut w = Whole {
        meta,
        cs,
        rng: Rng::new(seed),
        next_ino: rr.next_ino.max(bench + 1),
        roots,
        bench,
        created: VecDeque::new(),
        creates: 0,
        cluster_at: None,
        freed: VecDeque::new(),
    };
    w.commit("setup", 0, 0, [0; 4])?;

    // The small commits. Edit i of commit seq has kind (i + seq) mod 4, so
    // K = 1 rotates the kinds over the commits and a larger K splits them
    // about equally.
    let mut seq = 1;
    for pattern in patterns.iter().filter(|p| p.as_str() != INSDEL) {
        let cluster = pattern == "cluster";
        for &k in &ks {
            for _ in 0..commits {
                w.cluster_at = None;
                let mut kinds = [0u64; 4];
                for i in 0..k {
                    let done = w.edit(((i + seq) % 4) as usize, cluster)?;
                    kinds[done] += 1;
                }
                w.commit(pattern, k, seq, kinds)?;
                seq += 1;
            }
        }
    }

    // The version step: one commit that syncs the store tree to the next
    // tree (`run_seed_with` cannot add a version to a store).
    if let Some(next) = next_tree {
        let mut v = Version::default();
        w.sync_dir(1, next, &mut v)?;
        println!(
            "version files_changed={} files_created={} files_deleted={} dirs_created={} dirs_deleted={}",
            v.files_changed, v.files_created, v.files_deleted, v.dirs_created, v.dirs_deleted
        );
        w.commit("version", 0, seq, [0; 4])?;
        seq += 1;
    }

    // The insdel commits: edit i of commit seq is a delete when (i + seq)
    // mod 2 is 0, else an insert of a file that an earlier commit deleted
    // (a delete when there is none). So no key is both inserted and deleted
    // in one commit, and each edit changes one key of the committed map.
    if patterns.iter().any(|p| p == INSDEL) {
        let dirs = w.tree_dirs()?;
        for &k in &ks {
            for _ in 0..commits {
                let mut avail = w.freed.len();
                let mut inserted = Vec::new();
                let mut kinds = [0u64; 4];
                for i in 0..k {
                    if (i + seq) % 2 == 1 && avail > 0 {
                        avail -= 1;
                        inserted.push(w.insert_freed()?);
                        kinds[2] += 1;
                    } else {
                        w.delete_random(&dirs, &inserted)?;
                        kinds[3] += 1;
                    }
                }
                w.commit(INSDEL, k, seq, kinds)?;
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
    /// Runs one edit of `kind` (0 metadata change, 1 content change, 2
    /// create, 3 delete) and returns the kind that ran: a delete with no
    /// created file left is a create.
    fn edit(&mut self, kind: usize, cluster: bool) -> Res<usize> {
        match kind {
            0 => {
                let ino = self.pick(cluster)?;
                self.meta.modify_inode(ino, |m| {
                    m.mtime_sec += 1;
                    m.ctime_sec += 1;
                })?;
                Ok(0)
            }
            1 => {
                let ino = self.pick(cluster)?;
                let (key, size) = self.new_content()?;
                self.meta.set_manifest(ino, &[key])?;
                self.meta.modify_inode(ino, |m| {
                    m.size = size;
                    m.mtime_sec += 1;
                })?;
                Ok(1)
            }
            3 if !self.created.is_empty() => {
                let (name, ino) = self.created.pop_front().expect("not empty");
                self.meta.unlink(self.bench, &name)?;
                self.meta.delete_inode(ino)?;
                Ok(3)
            }
            _ => {
                let (key, size) = self.new_content()?;
                let mut m = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
                m.size = size;
                let ino = self.meta.create_inode(&m)?;
                let name = format!("c{:08}", self.creates);
                self.creates += 1;
                self.meta.link(self.bench, &name, ino)?;
                self.meta.set_manifest(ino, &[key])?;
                self.created.push_back((name, ino));
                self.next_ino = self.next_ino.max(ino + 1);
                Ok(2)
            }
        }
    }

    /// `CONTENT_BYTES` new bytes from the RNG, streamed into the store.
    fn new_content(&mut self) -> Res<(ObjectKey, u64)> {
        let mut buf = vec![0u8; CONTENT_BYTES];
        self.rng.fill(&mut buf);
        Ok(put_reader(&self.cs, &mut buf.as_slice())?)
    }

    /// The file of the next metadata or content change. `scatter`: a
    /// random file per pick. `cluster`: a random start per commit, then the
    /// next files in ino order (wrapping from `next_ino` to 2).
    fn pick(&mut self, cluster: bool) -> Res<u64> {
        let ino = match (cluster, self.cluster_at) {
            (false, _) | (true, None) => self.pick_scatter()?,
            (true, Some(last)) => {
                let mut ino = last;
                let mut found = None;
                for _ in 0..self.next_ino - 2 {
                    ino = if ino + 1 >= self.next_ino { 2 } else { ino + 1 };
                    if self.is_file(ino) {
                        found = Some(ino);
                        break;
                    }
                }
                found.ok_or_else(|| Fail::Run("no regular file for a cluster pick".into()))?
            }
        };
        if cluster {
            self.cluster_at = Some(ino);
        }
        Ok(ino)
    }

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

    /// Every directory of the tree, root included, `bench` and its files
    /// not: the places of the `insdel` deletes. One listing at a time.
    fn tree_dirs(&self) -> Res<Vec<u64>> {
        let mut dirs = Vec::new();
        let mut todo = vec![1];
        while let Some(dir) = todo.pop() {
            dirs.push(dir);
            for e in self.meta.list_directory(dir)? {
                if !is_dot(&e.name)
                    && !(dir == 1 && e.name == BENCH_DIR)
                    && self.meta.get_inode(e.ino)?.mode & S_IFMT == S_IFDIR
                {
                    todo.push(e.ino);
                }
            }
        }
        dirs.sort_unstable();
        Ok(dirs)
    }

    /// Deletes a random regular file of a random directory of `dirs`, not
    /// one of `skip` (the inserts of this commit), and keeps it in `freed`.
    fn delete_random(&mut self, dirs: &[u64], skip: &[u64]) -> Res<()> {
        for _ in 0..MAX_DRAWS {
            let dir = dirs[(self.rng.next() % dirs.len() as u64) as usize];
            let mut files = Vec::new();
            for e in self.meta.list_directory(dir)? {
                if !is_dot(&e.name) && !skip.contains(&e.ino) && self.is_file(e.ino) {
                    files.push((e.name, e.ino));
                }
            }
            if files.is_empty() {
                continue;
            }
            let (name, ino) = files.swap_remove((self.rng.next() % files.len() as u64) as usize);
            let meta = self.meta.get_inode(ino)?;
            let manifest = self.meta.get_manifest(ino)?;
            self.meta.unlink(dir, &name)?;
            self.meta.delete_inode(ino)?;
            self.freed.push_back(Freed { ino, dir, name, meta, manifest });
            return Ok(());
        }
        run_fail(format!("no regular file for a delete in {MAX_DRAWS} draws"))
    }

    /// Inserts the oldest freed file again at its inode number, name and
    /// content, with the mtime and ctime one second later (so its
    /// `inode_data` value is new, not a value that the store already has).
    fn insert_freed(&mut self) -> Res<u64> {
        let f = self.freed.pop_front().ok_or_else(|| Fail::Run("no freed file to insert".into()))?;
        let mut m = f.meta;
        m.mtime_sec += 1;
        m.ctime_sec += 1;
        self.meta.create_inode_at(f.ino, &m)?;
        self.meta.link(f.dir, &f.name, f.ino)?;
        self.meta.set_manifest(f.ino, &f.manifest)?;
        Ok(f.ino)
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

    /// Syncs the store directory `dir` to the tree directory `path`, as one
    /// version step. The content key, not the mtime, decides "changed",
    /// because release tars may stamp other mtimes; unchanged content then
    /// dedups to 0 new bytes. Tree names are walked sorted, regular files
    /// and directories only (as seed does). Memory: one listing per level
    /// and streamed file content, never a whole file (D-02).
    fn sync_dir(&mut self, dir: u64, path: &Path, v: &mut Version) -> Res<()> {
        // The store side by name: the listing order is not the name order,
        // and it holds `.` and `..`.
        let mut store_side: BTreeMap<String, u64> = BTreeMap::new();
        for e in self.meta.list_directory(dir)? {
            if !is_dot(&e.name) && !(dir == 1 && e.name == BENCH_DIR) {
                store_side.insert(e.name, e.ino);
            }
        }
        let mut names = Vec::new();
        for e in fs::read_dir(path)? {
            names.push(e?.file_name());
        }
        names.sort();
        for os_name in names {
            let p = path.join(&os_name);
            let name = os_name.to_string_lossy().into_owned();
            let md = fs::metadata(&p)?;
            if !md.is_file() && !md.is_dir() {
                continue;
            }
            let old = store_side.remove(&name);
            let old_is_dir = match old {
                Some(ino) => Some(self.meta.get_inode(ino)?.mode & S_IFMT == S_IFDIR),
                None => None,
            };
            match (old, old_is_dir, md.is_dir()) {
                (Some(ino), Some(false), false) => {
                    let (key, size) = put_reader(&self.cs, &mut fs::File::open(&p)?)?;
                    if self.meta.get_manifest(ino)? != vec![key] {
                        self.meta.set_manifest(ino, &[key])?;
                        let src = file_inode_meta(&md, size);
                        let mut m = self.meta.get_inode(ino)?;
                        m.size = src.size;
                        m.mtime_sec = src.mtime_sec;
                        m.mtime_nsec = src.mtime_nsec;
                        m.mode = src.mode;
                        self.meta.update_inode(&m)?;
                        v.files_changed += 1;
                    }
                }
                (Some(ino), Some(true), true) => self.sync_dir(ino, &p, v)?,
                (old, _, is_dir) => {
                    // New, or a file on one side and a directory on the
                    // other: remove, then create.
                    if let Some(ino) = old {
                        self.remove(dir, &name, ino, v)?;
                    }
                    if is_dir {
                        let ino = self.meta.create_directory(dir, &name, &dir_inode_meta(&md))?;
                        v.dirs_created += 1;
                        self.sync_dir(ino, &p, v)?;
                    } else {
                        let (key, size) = put_reader(&self.cs, &mut fs::File::open(&p)?)?;
                        let ino = self.meta.create_inode(&file_inode_meta(&md, size))?;
                        self.meta.link(dir, &name, ino)?;
                        self.meta.set_manifest(ino, &[key])?;
                        v.files_created += 1;
                    }
                }
            }
        }
        for (name, ino) in store_side {
            self.remove(dir, &name, ino, v)?;
        }
        Ok(())
    }

    /// Removes the entry `name` (inode `ino`) of `dir`; a directory loses
    /// its children first.
    fn remove(&mut self, dir: u64, name: &str, ino: u64, v: &mut Version) -> Res<()> {
        if self.meta.get_inode(ino)?.mode & S_IFMT == S_IFDIR {
            for e in self.meta.list_directory(ino)?.into_iter().filter(|e| !is_dot(&e.name)) {
                self.remove(ino, &e.name, e.ino, v)?;
            }
            v.dirs_deleted += 1;
        } else {
            v.files_deleted += 1;
        }
        self.meta.unlink(dir, name)?;
        self.meta.delete_inode(ino)?;
        Ok(())
    }
}

/// `.` or `..`: listed by the store, never synced or removed.
fn is_dot(name: &str) -> bool {
    name == "." || name == ".."
}

/// The `InodeMeta` of a regular file: a copy of `file_inode_meta` of
/// `crates/slicefs-cli/src/seed.rs` (private there).
fn file_inode_meta(md: &fs::Metadata, size: u64) -> InodeMeta {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = S_IFREG | (md.permissions().mode() & 0o7777);
    let mut m = InodeMeta::new_file(0, md.uid(), md.gid(), mode);
    m.size = size;
    m.mtime_sec = md.mtime();
    m.mtime_nsec = md.mtime_nsec() as u32;
    m
}

/// The `InodeMeta` of a directory: a copy of `dir_inode_meta` of seed.rs.
fn dir_inode_meta(md: &fs::Metadata) -> InodeMeta {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = S_IFDIR | (md.permissions().mode() & 0o7777);
    InodeMeta::new_directory(0, md.uid(), md.gid(), mode)
}
