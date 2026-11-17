//! Arena spike: fixed-size arena files for the chunk store (spike A), the
//! base for mmap reads (spike B) and free-space reuse (spike C). See the
//! shared plan; `chunk-store` itself is not changed, its private parts are
//! forked here (`codec`, `sys`, `file_fork`) and the real `ChunkStore` stays
//! the baseline ("RS").
//!
//! Where the code differs from the plan's interface sketch (section 2), and
//! how spikes B and C extend it:
//! - `Arena::open(dir, num, cfg, idx, Recovery, ro, active)` returns
//!   `Option<Opened>` (arena, entries, sealed, bytes walked): the store
//!   passes the idx it loaded and says which arena is the active one, so the
//!   cut runs only for that arena, inside `open`, before any map exists.
//!   `None` = bad header and no idx (the file was deleted).
//! - `ReadPath` is object-safe (`read_record(&Loc, &mut dyn FnMut(&[u8]))`)
//!   so the store holds a `Box<dyn ReadPath>`; `read::with_record` is the
//!   generic closure form of the plan. Spike B adds `map.rs` with an
//!   `MmapPath` and one arm in `read::make`. The store tells the read path
//!   the length of every arena (`arena_ready`, for each arena at open and
//!   for each new one; `fixed` = it has its full fixed length) and calls
//!   `forget` before it unlinks one.
//! - Spike C: reuse mode is its own store type, `reuse::ReuseStore` (same
//!   surface, same arena files and formats); `SpikeStore::open` refuses
//!   `Options::reuse`. It uses `Recovery::Reuse` (granule scan with BLAKE3),
//!   `Arena::write_at`, `GcToken::Seq`, `Loc::seq`, the allocators in
//!   `alloc.rs` and the fault points `CheckpointWrite` / `DirtyWrite`. The
//!   read path is the same `ReadPath` (spike B's map plugs in unchanged).
//!   `SpikeStore::io` / `ReuseStore::io` count the bytes written and
//!   `sys::full_syncs` the `full_sync` calls (write amplification and sync
//!   cost of C-B1).
//! - Extra options for spike A's measurements: `gc_seal` (always / size/8),
//!   `close_seals` (today's close, or a resumable `open` idx), `walk` (the
//!   recovery walk of A-B4).

pub mod alloc;
pub mod arena;
pub mod bench;
pub mod codec;
pub mod file_fork;
pub mod measure;
pub mod read;
pub mod reuse;
pub mod store;
pub mod sys;
pub mod workload;

pub use chunk_store::{chunk_ranges, Error, GcStats, ObjectId, Result, Stats};
pub use reuse::ReuseStore;
pub use store::{GcToken, SpikeStore};

/// How an arena file gets its fixed length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prealloc {
    /// V0: today's behaviour. The file grows with each record; the arena is
    /// sealed after it reaches `size` (it may pass it by one record).
    Grow,
    /// V1: `ftruncate(size)` at create. A sparse file.
    Sparse,
    /// V2: `F_PREALLOCATE` (macOS) or `fallocate` (Linux), then `ftruncate`.
    Reserve,
    /// V3: the whole file written with zeros at create. A lower bound only.
    Zeroed,
}

impl Prealloc {
    pub fn name(self) -> &'static str {
        match self {
            Prealloc::Grow => "V0",
            Prealloc::Sparse => "V1",
            Prealloc::Reserve => "V2",
            Prealloc::Zeroed => "V3",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "V0" => Prealloc::Grow,
            "V1" => Prealloc::Sparse,
            "V2" => Prealloc::Reserve,
            "V3" => Prealloc::Zeroed,
            _ => return None,
        })
    }

    /// The file length is fixed (V1-V3).
    pub fn fixed(self) -> bool {
        self != Prealloc::Grow
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advice {
    Default,
    Random,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Pread,
    /// Spike B.
    Mmap { advice: Advice },
}

#[derive(Clone, Copy, Debug)]
pub struct ArenaCfg {
    /// Arena file length (bytes), header page included.
    pub size: u64,
    pub prealloc: Prealloc,
    /// Records are padded to a multiple of this (1 = no padding). Divides 4096.
    pub granule: u32,
    /// Cut a sealed arena to its high-water mark (A-B3 measurement only;
    /// refused with `Mmap` or `reuse`).
    pub trim_on_seal: bool,
}

impl Default for ArenaCfg {
    fn default() -> Self {
        ArenaCfg { size: 64 << 20, prealloc: Prealloc::Sparse, granule: 1, trim_on_seal: false }
    }
}

/// When `gc_begin` seals the active arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealRule {
    /// Today's rule: whenever it holds a record.
    Always,
    /// Only when it holds at least `size / 8` bytes.
    Eighth,
}

/// How a reopened active arena finds its end (A-B4 compares them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walk {
    /// `fs::read` of the whole file, then decode (today's `scan_pack`).
    FullRead,
    /// `pread` in 1 MiB windows up to the first record that fails.
    Window,
    /// As `Window`, but stop at the first hole (`SEEK_HOLE`).
    HoleBounded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocKind {
    NextFit,
    SizeClasses,
    Hybrid,
    /// Spike C addition (not in the plan's grid): best fit inside one target
    /// arena; when it has no fit, the arena with the most free bytes.
    ArenaBest,
}

impl AllocKind {
    pub fn name(self) -> &'static str {
        match self {
            AllocKind::NextFit => "V1",
            AllocKind::SizeClasses => "V2",
            AllocKind::Hybrid => "V3",
            AllocKind::ArenaBest => "V4",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "V1" => AllocKind::NextFit,
            "V2" => AllocKind::SizeClasses,
            "V3" => AllocKind::Hybrid,
            "V4" => AllocKind::ArenaBest,
            _ => return None,
        })
    }
}

/// Spike C.
#[derive(Clone, Copy, Debug)]
pub struct ReuseCfg {
    pub alloc: AllocKind,
    pub evacuate_below: Option<f32>,
    pub relax_during_gc: bool,
    /// Recovery scan skips holes of a sparse arena (`SEEK_DATA`).
    pub seek_data: bool,
    /// 0 = checkpoint each arena the allocator left at the next sync (the
    /// plan). K > 0 = keep up to K arenas dirty.
    pub max_dirty: usize,
}

impl Default for ReuseCfg {
    fn default() -> Self {
        ReuseCfg { alloc: AllocKind::NextFit, evacuate_below: None, relax_during_gc: false, seek_data: false, max_dirty: 0 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub arena: ArenaCfg,
    pub read: ReadMode,
    /// Spike C; `None` = append mode.
    pub reuse: Option<ReuseCfg>,
    /// zstd level; 0 = no compression.
    pub zstd_level: i32,
    /// No changes on disk: no cut, zero, trim, preallocate or index write;
    /// always `pread`.
    pub read_only: bool,
    /// Make the next arena in the background (only if A-B5 asks for it).
    pub prepare_next: bool,
    pub gc_seal: SealRule,
    /// `close` seals the active arena (today's rule), instead of writing a
    /// resumable idx. For A-B3.
    pub close_seals: bool,
    pub walk: Walk,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            arena: ArenaCfg::default(),
            read: ReadMode::Pread,
            reuse: None,
            zstd_level: 3,
            read_only: false,
            prepare_next: false,
            gc_seal: SealRule::Eighth,
            close_seals: false,
            walk: Walk::Window,
        }
    }
}

impl Options {
    pub fn with_prealloc(p: Prealloc) -> Self {
        let mut o = Options::default();
        o.arena.prealloc = p;
        o
    }
}

/// Where a record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub arena: u64,
    pub off: u64,
    pub stored: u32,
    pub raw: u32,
    pub flags: u8,
    /// Order of `add` in this process (not stored; spike C's GC token).
    pub seq: u64,
}
