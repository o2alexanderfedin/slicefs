//! The store under test.
//!
//! Only the chunk store exists now. Phase 17 (#85) adds other index backends
//! here (for example a spike on fjall). The steps in `main.rs` call only the
//! traits in this file, so a new backend needs no change to the steps.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chunk_store::pack_io::{pack_name, Kind};
use chunk_store::{ChunkStore, ObjectId, Options};

/// Where `drop_index` keeps the moved index files: next to `packs/`, so the
/// store never sees them.
pub const SAVED_DIR: &str = "bench-saved";

/// Which packs lose their index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One pack: the lowest-numbered pack that is not the last one, or the
    /// only pack of a one-pack store.
    One,
    /// Every pack.
    All,
}

/// A pack whose index `drop_index` moved away.
#[derive(Clone, Debug)]
pub struct Target {
    pub num: u64,
    /// The last pack: the open takes the active-pack path (CRC check only,
    /// no index write). Else the sealed path (CRC and hash check of every
    /// record, then a new index).
    pub active: bool,
    /// Size of the pack file.
    pub pack_bytes: u64,
    /// Entries of the moved index.
    pub entries: u64,
}

/// The result of `drop_index`.
#[derive(Clone, Debug)]
pub struct Dropped {
    pub targets: Vec<Target>,
}

/// The result of `restore_index`.
#[derive(Clone, Copy, Debug)]
pub struct Restored {
    /// Index files that the open wrote again. Each one was byte-identical to
    /// its saved copy (`restore_index` fails otherwise).
    pub rebuilt: u64,
}

/// An open store.
pub trait Store {
    /// Store `data` (a new object, or a dedup hit).
    fn put(&self, data: &[u8]) -> Result<(), String>;
    /// True if the store has `id`.
    fn contains(&self, id: &ObjectId) -> bool;
    /// Distinct objects in the store.
    fn objects(&self) -> u64;
    /// Pack files of the store.
    fn packs(&self) -> u64;
    /// Seal the store: every pack gets its index. A backend with background
    /// work waits for it first, so the next open finds a settled store.
    fn close(&self) -> Result<(), String>;
    /// Extra `key=value` pairs that describe the index after an open (for
    /// example its run count). Empty by default.
    fn index_keys(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
    /// Remove every object that is not in `live` (one complete GC) and
    /// return `key=value` pairs that describe it. Not supported by default.
    fn gc(&self, _live: &HashSet<ObjectId>) -> Result<Vec<(&'static str, String)>, String> {
        Err("gc is not supported by this backend".into())
    }
}

/// Options of a new store. `None` keeps the default of the backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateOpts {
    /// The pack size limit in bytes (for tests).
    pub pack_limit: Option<u64>,
    /// The entries of the index memtable before a flush to a run (for
    /// tests).
    pub memtable_limit: Option<usize>,
}

/// A kind of store: how to make one and how to open one.
pub trait Backend {
    /// The name given with `--backend`.
    fn name(&self) -> &'static str;
    /// Make a new store in `dir`. Each value of `opts` that is given replaces
    /// the default.
    fn create(&self, dir: &Path, opts: CreateOpts) -> Result<Box<dyn Store>, String>;
    /// Open the store in `dir`.
    fn open(&self, dir: &Path, read_only: bool) -> Result<Box<dyn Store>, String>;

    /// Move the index of the `scope` packs away, so the next open must scan
    /// those packs.
    fn drop_index(&self, _dir: &Path, _scope: Scope) -> Result<Dropped, String> {
        Err(format!("recover is not supported by backend {}", self.name()))
    }

    /// Put the store back as it was before `drop_index`: keep each index the
    /// open wrote again (if it equals the saved copy), else move the saved
    /// copy back.
    fn restore_index(&self, _dir: &Path, _dropped: &Dropped) -> Result<Restored, String> {
        Err(format!("recover is not supported by backend {}", self.name()))
    }
}

/// The chunk store of `crates/chunk-store`, opened with `ChunkStore::open`.
pub struct ChunkStoreBackend;

struct ChunkStoreHandle(ChunkStore);

fn err(e: chunk_store::Error) -> String {
    e.to_string()
}

impl Store for ChunkStoreHandle {
    fn put(&self, data: &[u8]) -> Result<(), String> {
        self.0.put(data).map(|_| ()).map_err(err)
    }
    fn contains(&self, id: &ObjectId) -> bool {
        self.0.contains(id)
    }
    fn objects(&self) -> u64 {
        self.0.stats().objects
    }
    fn packs(&self) -> u64 {
        self.0.stats().packs
    }
    fn close(&self) -> Result<(), String> {
        // The background merges first: an open must not measure a store
        // whose runs the creator left unmerged.
        self.0.wait_index_idle();
        self.0.close().map_err(err)
    }
    fn index_keys(&self) -> Vec<(&'static str, String)> {
        let i = self.0.index_info();
        vec![
            ("index_runs", i.runs.to_string()),
            ("index_filter_bytes", i.filter_bytes.to_string()),
            ("index_fence_bytes", i.fence_bytes.to_string()),
            ("index_memtable_entries", i.memtable_entries.to_string()),
            ("hwm_pack", i.hwm_pack.to_string()),
        ]
    }
    fn gc(&self, live: &HashSet<ObjectId>) -> Result<Vec<(&'static str, String)>, String> {
        let boundary = self.0.gc_begin().map_err(err)?;
        let st = self.0.gc_finish(live, boundary).map_err(err)?;
        let t = self.0.last_gc_timing();
        let ms = |d: std::time::Duration| format!("{:.3}", d.as_secs_f64() * 1e3);
        Ok(vec![
            ("removed_objects", st.removed_objects.to_string()),
            ("kept_objects", st.kept_objects.to_string()),
            ("removed_bytes", st.removed_bytes.to_string()),
            ("packs_punched", st.packs_punched.to_string()),
            ("packs_compacted", st.packs_compacted.to_string()),
            ("packs_deleted", st.packs_deleted.to_string()),
            ("wall_s", format!("{:.3}", t.wall.as_secs_f64())),
            ("lock_hold_max_ms", ms(t.lock_hold_max)),
            ("lock_hold_total_ms", ms(t.lock_hold_total)),
        ])
    }
}

/// The numbers of the `.pack` files in `packs`, sorted.
fn pack_numbers(packs: &Path) -> Result<Vec<u64>, String> {
    let rd = std::fs::read_dir(packs).map_err(|e| format!("cannot list the packs: {e}"))?;
    let mut nums: Vec<u64> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            u64::from_str_radix(name.strip_suffix(".pack")?, 16).ok()
        })
        .collect();
    nums.sort_unstable();
    Ok(nums)
}

/// The entry count of an index file: a u64 LE at bytes 6..14 of its header.
fn idx_entries(path: &Path) -> Result<u64, String> {
    use std::io::Read;
    let mut head = [0u8; 14];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .map_err(|e| format!("cannot read an index header: {e}"))?;
    Ok(u64::from_le_bytes(head[6..14].try_into().unwrap()))
}

fn not_clean(saved: &Path, what: &str) -> String {
    format!(
        "the store is not clean ({what}): a previous recover did not finish; the saved index files are in {}",
        saved.display()
    )
}

/// Live and saved path of the index of pack `num`.
fn idx_paths(dir: &Path, num: u64) -> (PathBuf, PathBuf) {
    let name = pack_name(num, Kind::Idx);
    (dir.join("packs").join(&name), dir.join(SAVED_DIR).join(name))
}

impl Backend for ChunkStoreBackend {
    fn name(&self) -> &'static str {
        "chunk-store"
    }

    fn create(&self, dir: &Path, c: CreateOpts) -> Result<Box<dyn Store>, String> {
        let mut opts = Options::default();
        if let Some(limit) = c.pack_limit {
            opts.pack_limit = limit;
        }
        if let Some(limit) = c.memtable_limit {
            opts.index_memtable_limit = limit;
        }
        Ok(Box::new(ChunkStoreHandle(ChunkStore::open(dir, opts).map_err(err)?)))
    }

    fn open(&self, dir: &Path, read_only: bool) -> Result<Box<dyn Store>, String> {
        let opts = Options { read_only, ..Options::default() };
        Ok(Box::new(ChunkStoreHandle(ChunkStore::open(dir, opts).map_err(err)?)))
    }

    fn drop_index(&self, dir: &Path, scope: Scope) -> Result<Dropped, String> {
        let packs = dir.join("packs");
        let saved = dir.join(SAVED_DIR);
        if let Ok(mut rd) = std::fs::read_dir(&saved) {
            if rd.next().is_some() {
                return Err(not_clean(&saved, "saved index files are left"));
            }
        }
        let nums = pack_numbers(&packs)?;
        let Some(&last) = nums.last() else { return Err("the store has no packs".into()) };
        if let Some(&num) = nums.iter().find(|&&n| !idx_paths(dir, n).0.exists()) {
            return Err(not_clean(&saved, &format!("pack {num} has no index")));
        }
        let picked: Vec<u64> = match scope {
            Scope::All => nums.clone(),
            Scope::One => vec![nums[0]], // the lowest pack; it is the last only in a one-pack store
        };
        let mut targets = Vec::new();
        for num in picked {
            let pack_bytes = std::fs::metadata(packs.join(pack_name(num, Kind::Pack)))
                .map_err(|e| format!("cannot read a pack size: {e}"))?
                .len();
            let entries = idx_entries(&idx_paths(dir, num).0)?;
            targets.push(Target { num, active: num == last, pack_bytes, entries });
        }
        std::fs::create_dir_all(&saved).map_err(|e| format!("cannot make {}: {e}", saved.display()))?;
        let mut moved = Vec::new();
        for t in &targets {
            let (live, keep) = idx_paths(dir, t.num);
            if let Err(e) = std::fs::rename(&live, &keep) {
                // Put back what moved, so the store stays whole.
                for &num in &moved {
                    let (live, keep) = idx_paths(dir, num);
                    let _ = std::fs::rename(keep, live);
                }
                let _ = std::fs::remove_dir(&saved);
                return Err(format!("cannot move an index file: {e}"));
            }
            moved.push(t.num);
        }
        Ok(Dropped { targets })
    }

    fn restore_index(&self, dir: &Path, dropped: &Dropped) -> Result<Restored, String> {
        let saved = dir.join(SAVED_DIR);
        let mut rebuilt = 0;
        let mut errors = Vec::new();
        for t in &dropped.targets {
            let (live, keep) = idx_paths(dir, t.num);
            if live.exists() {
                // The open wrote the index again: it must equal the saved copy.
                let same = match (std::fs::read(&live), std::fs::read(&keep)) {
                    (Ok(a), Ok(b)) => a == b,
                    (Err(e), _) | (_, Err(e)) => {
                        errors.push(format!("cannot compare the index of pack {}: {e}", t.num));
                        continue;
                    }
                };
                if !same {
                    errors.push(format!("the rebuilt index of pack {} differs from the saved copy", t.num));
                    continue;
                }
                if let Err(e) = std::fs::remove_file(&keep) {
                    errors.push(format!("cannot remove a saved index: {e}"));
                    continue;
                }
                rebuilt += 1;
            } else if let Err(e) = std::fs::rename(&keep, &live) {
                errors.push(format!("cannot move the index of pack {} back: {e}", t.num));
            }
        }
        if errors.is_empty() {
            if let Err(e) = std::fs::remove_dir(&saved) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    errors.push(format!("cannot remove {}: {e}", saved.display()));
                }
            }
        }
        if errors.is_empty() {
            let nums = pack_numbers(&dir.join("packs"))?;
            if let Some(num) = nums.iter().find(|&&n| !idx_paths(dir, n).0.exists()) {
                errors.push(format!("pack {num} has no index after the restore"));
            }
        }
        if errors.is_empty() {
            Ok(Restored { rebuilt })
        } else {
            Err(errors.join("; "))
        }
    }
}

/// The backend with this `--backend` name.
pub fn backend_by_name(name: &str) -> Option<Box<dyn Backend>> {
    match name {
        "chunk-store" => Some(Box::new(ChunkStoreBackend)),
        _ => None,
    }
}
