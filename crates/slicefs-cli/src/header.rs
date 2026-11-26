//! The store header: the text file `<store>/FORMAT`.
//!
//! The first line names the store format (`slicefs-store 7 ...`). Each
//! later line is one item: the store setting `hash <name>` (the write
//! hash), the store setting `chunks <min> <avg> <max>` (the chunk profile,
//! with the ro_compat flag `chunk-profile`), or a feature flag `<class> <name> <release>`, where the class is
//! `compat`, `ro_compat` or `incompat` and the release is the lowest SliceFS
//! release that knows the flag (shown in messages, never compared).
//!
//! The open decision ([`decide`]) collects the flags first. The strongest
//! unknown flag decides: an unknown incompat flag refuses the store, an
//! unknown ro_compat flag gives read-only access (a write is refused), an
//! unknown compat flag changes nothing. A line of an unknown kind is
//! allowed only next to an unknown flag (it belongs to that feature).

use std::io::Read;
use std::path::Path;

use chunk_store::{ChunkProfile, ObjectHasher, DEFAULT_HASHER};

/// The header file in the store root.
pub const FORMAT_FILE: &str = "FORMAT";
/// The store format number that this build reads and writes.
pub const FORMAT_NUMBER: u32 = 7;
/// The first line that this build writes.
pub const FORMAT: &str = "slicefs-store 7 (store header with feature flags)\n";
/// The largest header that [`read`] accepts, in bytes.
pub const MAX_HEADER: usize = 64 * 1024;
/// The ro_compat flag of a store with a non-default chunk profile (the
/// line `chunks <min> <avg> <max>`): an older build reads such a store, but
/// would write it with another profile.
pub const CHUNK_PROFILE_FLAG: &str = "chunk-profile";
/// The release word of the `chunk-profile` flag. A placeholder until the
/// owner names the release; tests use this constant, never its text.
pub const CHUNK_PROFILE_RELEASE: &str = "v2.2";
/// The incompat flag of a store that has, or had, objects with SHA-256 IDs
/// (tag 0x12): an older build cannot verify them, so it must not open the
/// store.
pub const HASH_SHA256_FLAG: &str = "hash-sha256";
/// The release word of the `hash-sha256` flag. A placeholder until the
/// owner names the release, as [`CHUNK_PROFILE_RELEASE`]; tests use this
/// constant, never its text.
pub const HASH_SHA256_RELEASE: &str = "v2.2";
/// The feature flags that this build knows (Phase 22: `chunk-profile`;
/// Phase 23: `hash-sha256`). Add a name here with the rule of
/// docs/design/store-format.md.
pub const KNOWN_FLAGS: &[&str] = &[CHUNK_PROFILE_FLAG, HASH_SHA256_FLAG];

/// The flag that a store needs before it holds objects of `hasher`
/// (`None`: no flag). The one place that says which hash needs a flag.
pub fn hash_flag(hasher: &dyn ObjectHasher) -> Option<Flag> {
    (hasher.tag() == chunk_store::hash::Sha256.tag()).then(|| Flag {
        class: Class::Incompat,
        name: HASH_SHA256_FLAG.to_string(),
        release: HASH_SHA256_RELEASE.to_string(),
    })
}

/// The header of a new store whose new objects use `hasher` (the default
/// chunk profile).
pub fn marker(hasher: &dyn ObjectHasher) -> String {
    marker_with(hasher, ChunkProfile::DEFAULT)
}

/// The header of a new store with the write hash `hasher` and the chunk
/// profile `profile`. A non-default profile adds the line `chunks <min>
/// <avg> <max>` (bytes) and its ro_compat flag; the default profile adds
/// nothing, so the header of a default store stays as in format 7. A hash
/// that needs a flag ([`hash_flag`]: SHA-256) gets its flag line right
/// after the hash line.
pub fn marker_with(hasher: &dyn ObjectHasher, profile: ChunkProfile) -> String {
    let mut s = format!("{FORMAT}hash {}\n", hasher.name());
    if let Some(f) = hash_flag(hasher) {
        s.push_str(&format!("{} {} {}\n", f.class.word(), f.name, f.release));
    }
    if profile != ChunkProfile::DEFAULT {
        s.push_str(&format!("chunks {} {} {}\n", profile.min(), profile.avg(), profile.max()));
        s.push_str(&format!("ro_compat {CHUNK_PROFILE_FLAG} {CHUNK_PROFILE_RELEASE}\n"));
    }
    s
}

/// The class of a feature flag: what a build that does not know the flag
/// does with the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// It reads and writes the store.
    Compat,
    /// It reads the store, but does not write it.
    RoCompat,
    /// It does not open the store.
    Incompat,
}

impl Class {
    /// The first word of a flag line of this class.
    pub fn word(self) -> &'static str {
        match self {
            Class::Compat => "compat",
            Class::RoCompat => "ro_compat",
            Class::Incompat => "incompat",
        }
    }

    /// The class of the first word of a line (`None`: not a flag line).
    pub fn from_word(w: &str) -> Option<Class> {
        Class::from_word_bytes(w.as_bytes())
    }

    /// The class of the bytes of the first word of a line (`None`: not a
    /// flag line). Part of the byte-level core that Kani proves.
    pub fn from_word_bytes(w: &[u8]) -> Option<Class> {
        match w {
            b"compat" => Some(Class::Compat),
            b"ro_compat" => Some(Class::RoCompat),
            b"incompat" => Some(Class::Incompat),
            _ => None,
        }
    }
}

/// A feature flag: its class, its name and the lowest release that knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub class: Class,
    pub name: String,
    pub release: String,
}

/// One line of the header after the first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// `hash <name>`: the write hash of the store (a setting, not a flag).
    Hash(String),
    /// `chunks <min> <avg> <max>`: the FastCDC profile of the store, in
    /// bytes (a setting; it always comes with the ro_compat flag
    /// `chunk-profile`).
    Chunks(ChunkProfile),
    /// `<class> <name> <release>`.
    Flag(Flag),
    /// A line of a kind that this build does not know, as it is.
    Other(String),
}

/// A parsed header: the format number, the first line, and every later
/// line in its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub format: u32,
    pub first: String,
    pub lines: Vec<Line>,
}

impl Header {
    /// The name of the write hash (`None`: no hash line).
    pub fn hash(&self) -> Option<&str> {
        self.lines.iter().find_map(|l| match l {
            Line::Hash(name) => Some(name.as_str()),
            _ => None,
        })
    }

    /// The chunk profile (`None`: no `chunks` line, the default profile).
    pub fn chunk_profile(&self) -> Option<ChunkProfile> {
        self.lines.iter().find_map(|l| match l {
            Line::Chunks(p) => Some(*p),
            _ => None,
        })
    }

    /// The feature flags, in their order.
    pub fn flags(&self) -> impl Iterator<Item = &Flag> {
        self.lines.iter().filter_map(|l| match l {
            Line::Flag(f) => Some(f),
            _ => None,
        })
    }

    /// The header text: the first line and every later line in its order,
    /// each with a line end.
    pub fn to_text(&self) -> String {
        let mut s = format!("{}\n", self.first);
        for l in &self.lines {
            match l {
                Line::Hash(name) => s.push_str(&format!("hash {name}\n")),
                Line::Chunks(p) => s.push_str(&format!("chunks {} {} {}\n", p.min(), p.avg(), p.max())),
                Line::Flag(f) => s.push_str(&format!("{} {} {}\n", f.class.word(), f.name, f.release)),
                Line::Other(text) => s.push_str(&format!("{text}\n")),
            }
        }
        s
    }
}

/// What the caller does with the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Read only: never writes the store.
    Read,
    /// A command that writes the store.
    Write,
    /// A mount: it writes when the store allows it, else it mounts read-only.
    Mount,
}

/// The open decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Read and write.
    ReadWrite,
    /// Read only, because of this unknown ro_compat flag (the first one).
    ReadOnly(Flag),
}

impl Access {
    /// True for [`Access::ReadWrite`].
    pub fn is_read_write(&self) -> bool {
        matches!(self, Access::ReadWrite)
    }
}

/// The result of [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The header (`None`: the store has no header yet).
    pub header: Option<Header>,
    pub access: Access,
}

// ── Messages ──────────────────────────────────────────────────────────────────
//
// Each text is one `format!` literal on one line (docs/design/store-format.md
// lists them).

fn msg_no_header(store: &Path) -> String {
    let store = store.display();
    format!("{store} has data but no store header (the file FORMAT is missing). This SliceFS does not open a store with no header.")
}

fn msg_older(store: &Path, what: &str) -> String {
    let store = store.display();
    format!("{store} was made by an older SliceFS ({what}). The store format changed without a migration: copy the files out with the older version, then into a new store")
}

fn msg_newer(store: &Path, n: u32) -> String {
    let (store, known) = (store.display(), FORMAT_NUMBER);
    format!("{store} was made by a newer SliceFS (store format {n}). This SliceFS reads store format {known} only.")
}

fn msg_unknown_format(store: &Path, line: &str) -> String {
    let store = store.display();
    format!("{store}: unknown store format {line:?}")
}

fn msg_incompat(store: &Path, name: &str, release: &str) -> String {
    let store = store.display();
    format!("{store} uses the feature {name}, which needs SliceFS {release} or later. This SliceFS cannot open it.")
}

/// The line that a mount prints when it mounts the store read-only.
pub fn msg_mount_read_only(store: &Path, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("Mounting read-only: {store} uses the feature {name}, which needs SliceFS {release} or later to write.")
}

/// A write command (or a writable load) on a store with the unknown
/// ro_compat flag `flag`.
pub fn msg_ro_write(store: &Path, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("{store} uses the feature {name}, which needs SliceFS {release} or later to write. This SliceFS can only read it.")
}

/// `--hash` on a ro_compat mount.
pub fn msg_ro_hash(store: &Path, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("{store} can only be mounted read-only (feature {name} needs SliceFS {release} or later to write), so --hash cannot change its write hash.")
}

/// `--auto-snapshot` on a ro_compat mount.
pub fn msg_ro_auto_snapshot(store: &Path, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("{store} can only be mounted read-only (feature {name} needs SliceFS {release} or later to write), so --auto-snapshot cannot make a snapshot at unmount.")
}

/// A ro_compat mount of a store whose write log has `n` records to replay.
pub fn msg_log_pending(store: &Path, n: usize, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("{store} has {n} request(s) in its write log. They must be replayed, but this SliceFS can only mount the store read-only (feature {name} needs SliceFS {release} or later). Mount it once with SliceFS {release} or later.")
}

/// A ro_compat mount of a store with `mount.lock`.
pub fn msg_lock_present(store: &Path, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("{store}/mount.lock exists: the last mount did not end cleanly, or a mount runs now. This SliceFS can only mount the store read-only (feature {name} needs SliceFS {release} or later) and cannot replay the write log.")
}

/// `--chunk-profile want` on a store whose recorded profile is `have`.
pub fn msg_profile_differs(store: &Path, have: ChunkProfile, want: ChunkProfile) -> String {
    let store = store.display();
    format!("{store} has the chunk profile {have}; --chunk-profile {want} is another profile. The chunk profile of a store is fixed when the store is made.")
}

fn msg_bad_line(store: &Path, k: usize, reason: &str, line: &str) -> String {
    let store = store.display();
    format!("{store}/FORMAT line {k}: {reason}: {line:?}")
}

fn msg_too_large(store: &Path) -> String {
    let store = store.display();
    format!("{store}/FORMAT is larger than 64 KiB; it is not a SliceFS store header.")
}

fn msg_not_text(store: &Path) -> String {
    let store = store.display();
    format!("{store}/FORMAT is not UTF-8 text; it is not a SliceFS store header.")
}

fn refuse(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

// ── Read, parse, decide ───────────────────────────────────────────────────────

/// Read the header text of `store` (`None`: no header file). Reads at most
/// [`MAX_HEADER`] + 1 bytes: a larger file or one that is not UTF-8 is an
/// error.
pub fn read(store: &Path) -> std::io::Result<Option<String>> {
    let f = match std::fs::File::open(store.join(FORMAT_FILE)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    f.take(MAX_HEADER as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_HEADER {
        return Err(refuse(msg_too_large(store)));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, msg_not_text(store)))
}

/// Parse the header text of `store`. The first line must be
/// `slicefs-store <n>` with an optional free text after one space; the
/// number is parsed (the free text is not compared). A number below or
/// above [`FORMAT_NUMBER`] is refused, and the other lines are not read
/// (another format can have any layout). Every later line is kept in its
/// order: `hash <name>` (at most one), `chunks <min> <avg> <max>` (at most
/// one; see [`parse_chunks`]), a flag line `<class> <name>
/// <release>` (see [`valid_name_bytes`] and [`valid_release_bytes`]; a name once in
/// the whole header), or a line of another kind. The `chunks` line and the
/// flag [`CHUNK_PROFILE_FLAG`] come together or not at all, and that flag is
/// ro_compat (an older build that does not know it opens the store
/// read-only and keeps the `chunks` line as a line of its feature).
pub fn parse(store: &Path, text: &str) -> std::io::Result<Header> {
    let mut pieces: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        pieces.pop();
    }
    let first = pieces.first().copied().unwrap_or("");
    let format = first_line_number(first.as_bytes()).ok_or_else(|| refuse(msg_unknown_format(store, first)))?;
    match format {
        n if n < FORMAT_NUMBER => {
            let what = match n {
                0..=4 => "metadata in vt0/",
                5 => "224-bit object IDs",
                _ => "store format 6, no feature flags",
            };
            return Err(refuse(msg_older(store, what)));
        }
        n if n > FORMAT_NUMBER => return Err(refuse(msg_newer(store, n))),
        _ => {}
    }
    let mut lines: Vec<Line> = Vec::new();
    for (i, &line) in pieces.iter().enumerate().skip(1) {
        let bad = |reason: &str| refuse(msg_bad_line(store, i + 1, reason, line));
        let words: Vec<&str> = line.split(' ').collect();
        let parsed = match (words[0], Class::from_word_bytes(words[0].as_bytes())) {
            ("hash", _) if words.len() == 2 => {
                if lines.iter().any(|l| matches!(l, Line::Hash(_))) {
                    return Err(bad("two hash lines"));
                }
                Line::Hash(words[1].to_string())
            }
            ("chunks", _) => {
                if lines.iter().any(|l| matches!(l, Line::Chunks(_))) {
                    return Err(bad("two chunks lines"));
                }
                Line::Chunks(parse_chunks(&words).ok_or_else(|| bad("bad chunks line"))?)
            }
            (_, Some(class)) => {
                if words.len() != 3 || !valid_name_bytes(words[1].as_bytes()) || !valid_release_bytes(words[2].as_bytes()) {
                    return Err(bad("bad feature flag line"));
                }
                let name = words[1];
                if lines.iter().any(|l| matches!(l, Line::Flag(f) if f.name == name)) {
                    return Err(bad(&format!("the feature {name} is listed twice")));
                }
                Line::Flag(Flag { class, name: name.to_string(), release: words[2].to_string() })
            }
            _ => Line::Other(line.to_string()),
        };
        lines.push(parsed);
    }
    // The `chunks` line and the `chunk-profile` flag come as a pair, and the
    // flag is ro_compat. Index `j` of `lines` is line `j + 2` of the text.
    let bad_at = |j: usize, reason: &str| refuse(msg_bad_line(store, j + 2, reason, pieces[j + 1]));
    let chunks_at = lines.iter().position(|l| matches!(l, Line::Chunks(_)));
    let flag_at = lines.iter().position(|l| matches!(l, Line::Flag(f) if f.name == CHUNK_PROFILE_FLAG));
    match (chunks_at, flag_at) {
        (Some(j), None) => return Err(bad_at(j, "chunks line with no chunk-profile flag")),
        (None, Some(j)) => return Err(bad_at(j, "chunk-profile flag with no chunks line")),
        (Some(_), Some(j)) if !matches!(&lines[j], Line::Flag(f) if f.class == Class::RoCompat) => {
            return Err(bad_at(j, "the feature chunk-profile must be ro_compat"));
        }
        _ => {}
    }
    Ok(Header { format, first: first.to_string(), lines })
}

/// The profile of the words of a `chunks` line: exactly three sizes after
/// the word, ASCII digits only, each a u32, that [`ChunkProfile::new`]
/// accepts (so a bad profile on disk never reaches FastCDC).
fn parse_chunks(words: &[&str]) -> Option<ChunkProfile> {
    let [_, min, avg, max] = words else { return None };
    let size = |w: &str| if !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()) { w.parse::<u32>().ok() } else { None };
    ChunkProfile::new(size(min)?, size(avg)?, size(max)?).ok()
}

// The byte-level core of `parse`: `valid_name_bytes`, `valid_release_bytes`,
// `first_line_number` and `Class::from_word_bytes`. Kani proves these free of
// panic and overflow (`mod proofs`). `parse` itself (str splitting, messages
// with `format!`) is not proven end to end: str-level harnesses did not finish
// in 150 s (21.2 research). It is covered by these proofs and its unit tests.

/// A flag name: 1 to 64 bytes of `a-z`, `0-9` and `-`, a letter or digit at
/// each end, no `--`.
fn valid_name_bytes(b: &[u8]) -> bool {
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=64).contains(&b.len())
        && b.iter().all(|&c| alnum(c) || c == b'-')
        && alnum(b[0])
        && alnum(b[b.len() - 1])
        && !b.windows(2).any(|w| w == b"--")
}

/// A release: 1 to 32 visible ASCII bytes (no space). It is shown in
/// messages, never compared.
fn valid_release_bytes(b: &[u8]) -> bool {
    (1..=32).contains(&b.len()) && b.iter().all(|c| c.is_ascii_graphic())
}

/// The format number of a first line `slicefs-store <digits>[ <free text>]`:
/// the bytes after the prefix up to the first space (or the end), digits
/// only, at most `u32::MAX` (else `None`).
fn first_line_number(first: &[u8]) -> Option<u32> {
    let rest = first.strip_prefix(b"slicefs-store ")?;
    let number = rest.split(|&b| b == b' ').next().unwrap_or(rest);
    if number.is_empty() || !number.iter().all(u8::is_ascii_digit) {
        return None;
    }
    number.iter().try_fold(0u32, |v, &d| v.checked_mul(10)?.checked_add(u32::from(d - b'0')))
}

/// The open decision for a parsed header. The unknown flags are the flags
/// whose name is not in [`KNOWN_FLAGS`]. An unknown incompat flag refuses
/// (any `want`). Else the first unknown ro_compat flag gives read-only
/// access, and refuses `Want::Write`. Else, when no flag is unknown, a line
/// of an unknown kind is an error. Else read and write.
pub fn decide(store: &Path, header: &Header, want: Want) -> std::io::Result<Access> {
    let unknown: Vec<&Flag> = header.flags().filter(|f| !KNOWN_FLAGS.contains(&f.name.as_str())).collect();
    let incompat: Vec<String> = unknown
        .iter()
        .filter(|f| f.class == Class::Incompat)
        .map(|f| msg_incompat(store, &f.name, &f.release))
        .collect();
    if !incompat.is_empty() {
        return Err(refuse(incompat.join(" ")));
    }
    if let Some(flag) = unknown.iter().find(|f| f.class == Class::RoCompat) {
        if want == Want::Write {
            return Err(refuse(msg_ro_write(store, flag)));
        }
        return Ok(Access::ReadOnly((*flag).clone()));
    }
    if unknown.is_empty() {
        let other = header.lines.iter().enumerate().find_map(|(i, l)| match l {
            Line::Other(text) => Some((i + 2, text)),
            _ => None,
        });
        if let Some((k, text)) = other {
            return Err(refuse(msg_bad_line(store, k, "unknown setting", text)));
        }
    }
    Ok(Access::ReadWrite)
}

/// The one check of the store header: read it, parse it, decide.
///
/// No header: a store with data (an entry in `packs/` or `segments/`) is
/// refused, for every `want`. A store with no data is new: with
/// `Want::Write` it gets a new header ([`create`]) and the check goes on
/// with it; with `Want::Read` or `Want::Mount` nothing is written and the
/// access is read and write (`header: None`; a mount needs `segments/`, and
/// `load_store` refuses a store with no committed state).
pub fn check(store: &Path, want: Want) -> std::io::Result<Opened> {
    check_with(store, want, DEFAULT_HASHER, ChunkProfile::DEFAULT)
}

/// [`check`], where a new store gets the header of `hasher` (`--hash`) and
/// `profile` (`--chunk-profile`).
pub fn check_with(store: &Path, want: Want, hasher: &dyn ObjectHasher, profile: ChunkProfile) -> std::io::Result<Opened> {
    let text = match read(store)? {
        Some(text) => text,
        None if has_data(store)? => return Err(refuse(msg_no_header(store))),
        None if want == Want::Write => {
            match create_with(store, hasher, profile) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            read(store)?.ok_or_else(|| refuse(format!("{}/FORMAT is missing after its write", store.display())))?
        }
        None => return Ok(Opened { header: None, access: Access::ReadWrite }),
    };
    let header = parse(store, &text)?;
    let access = decide(store, &header, want)?;
    Ok(Opened { header: Some(header), access })
}

/// True when `packs/` or `segments/` of `store` has an entry (a missing or
/// empty directory is no data). Reads one entry of each, not the full list.
fn has_data(store: &Path) -> std::io::Result<bool> {
    for dir in ["packs", "segments"] {
        match std::fs::read_dir(store.join(dir)) {
            Ok(mut entries) => {
                if entries.next().transpose()?.is_some() {
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

// ── Store lock ────────────────────────────────────────────────────────────────

/// A `flock` on the store directory, held until the drop. A read-only
/// mount holds it shared for its life (it writes nothing to the store, so
/// it makes no `mount.lock`); a writable mount holds it exclusive for its
/// life (taken before `mount.lock`, released after it); a command that
/// needs no live mount (offline gc, snapshot create, snapshot switch)
/// holds it exclusive while it runs.
/// The lock is on the directory and not on FORMAT, because a header
/// rewrite replaces the FORMAT file. A file system with no `flock` gives no
/// lock (the commands run as before).
#[derive(Debug)]
pub struct StoreLock {
    _dir: Option<std::fs::File>,
}

/// Take the store lock of `store`, shared or exclusive, without blocking.
/// `Ok(None)` when another holder keeps it for more than `LOCK_RETRY`.
///
/// The short retry is for a false "held": the lock belongs to the open
/// directory handle, and a child process that another thread of this process
/// starts at that moment holds a copy of the handle for a short time (seen on
/// macOS: about 5% of immediate re-locks with 4 threads that spawn). A real
/// holder, such as a live mount, keeps it, and the caller is refused after
/// the retry.
fn try_store_lock(store: &Path, shared: bool) -> std::io::Result<Option<StoreLock>> {
    const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(250);
    const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(10);
    let dir = std::fs::File::open(store)?;
    let start = std::time::Instant::now();
    loop {
        let r = if shared { dir.try_lock_shared() } else { dir.try_lock() };
        match r {
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_RETRY => std::thread::sleep(LOCK_POLL),
            r => return lock_result(dir, r),
        }
    }
}

fn lock_result(dir: std::fs::File, r: Result<(), std::fs::TryLockError>) -> std::io::Result<Option<StoreLock>> {
    match r {
        Ok(()) => Ok(Some(StoreLock { _dir: Some(dir) })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => Ok(Some(StoreLock { _dir: None })),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// The shared store lock of a read-only mount. Refused while a writable
/// mount or a command that writes (offline gc, snapshot create or switch)
/// holds it.
pub fn lock_read_mount(store: &Path) -> std::io::Result<StoreLock> {
    try_store_lock(store, true)?.ok_or_else(|| {
        refuse(format!("{} is in use: it is mounted to write, or a command that writes it (gc, seed, snapshot create or snapshot switch) runs now. Mount it when that mount or command ends.", store.display()))
    })
}

/// The exclusive store lock of a command that needs no live mount.
/// Refused while a mount (or another such command) holds it.
pub fn lock_offline(store: &Path) -> std::io::Result<StoreLock> {
    try_store_lock(store, false)?.ok_or_else(|| {
        refuse(format!("{} is in use: it is mounted, or another command (gc, seed, snapshot create or snapshot switch) runs now. Unmount it or let that command end first.", store.display()))
    })
}

/// The exclusive store lock of a writable mount, held for the life of the
/// daemon (#201). Refused while another mount or a command that needs no
/// live mount holds it; then `mount.lock` belongs to that holder and is
/// not touched.
pub fn lock_write_mount(store: &Path) -> std::io::Result<StoreLock> {
    try_store_lock(store, false)?.ok_or_else(|| {
        refuse(format!("{} is in use: it is mounted, or a command (gc, seed, snapshot create or snapshot switch) runs now. Unmount it or let that command end first.", store.display()))
    })
}

/// True when a read-only mount (or a command that writes) holds the store
/// lock of `store` now. Takes the exclusive lock and drops it at once.
pub fn store_lock_held(store: &Path) -> bool {
    matches!(try_store_lock(store, false), Ok(None))
}

// ── Write ─────────────────────────────────────────────────────────────────────

/// Make the header of a new store: the first step of a new store, before
/// `packs/` or `segments/` exist. Makes the store directory if needed and
/// writes [`marker`]`(DEFAULT_HASHER)` durably. Refuses (`AlreadyExists`)
/// when FORMAT exists; the file stays as it is. Safe for parallel creates:
/// the header is written to a temp file of its own, synced, and installed
/// with a hard link, which never replaces a FORMAT that another create
/// installed first. Tests that build a store with `StoreIo::open` or
/// `ChunkStore::open` call it first.
pub fn create(store: &Path) -> std::io::Result<()> {
    create_with(store, DEFAULT_HASHER, ChunkProfile::DEFAULT)
}

/// [`create`] with the header [`marker_with`]`(hasher, profile)`.
fn create_with(store: &Path, hasher: &dyn ObjectHasher, profile: ChunkProfile) -> std::io::Result<()> {
    std::fs::create_dir_all(store)?;
    let format = store.join(FORMAT_FILE);
    if format.exists() {
        return Err(already_exists(store));
    }
    let tmp = write_tmp(store, &marker_with(hasher, profile))?;
    let linked = match std::fs::hard_link(&tmp, &format) {
        // A file system with no hard links: the rename of before (it can
        // replace a header that a parallel create wrote at the same time).
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists && !format.exists() => std::fs::rename(&tmp, &format),
        r => r,
    };
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(already_exists(store)),
        Err(e) => Err(e),
        Ok(()) => std::fs::File::open(store)?.sync_all(),
    }
}

fn already_exists(store: &Path) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{}/FORMAT exists", store.display()))
}

/// Write `header` as the header of `store`, durably (see [`write_text`]).
pub fn write_header(store: &Path, header: &Header) -> std::io::Result<()> {
    write_text(store, &header.to_text())
}

/// Write `text` as the header of `store` durably: a synced temp file
/// renamed over the old header, then a sync of the directory. A power loss
/// leaves the old header or the new one, never an empty or torn file.
fn write_text(store: &Path, text: &str) -> std::io::Result<()> {
    let tmp = write_tmp(store, text)?;
    if let Err(e) = std::fs::rename(&tmp, store.join(FORMAT_FILE)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::File::open(store)?.sync_all()
}

/// Write `text` to a new temp file in `store` and sync it. The name
/// (`FORMAT.tmp.<pid>.<n>`) is unique to this process and this call, so two
/// writers never write the same temp file. On an error the file is removed.
fn write_tmp(store: &Path, text: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = store.join(format!("{FORMAT_FILE}.tmp.{}.{n}", std::process::id()));
    let written = std::fs::File::options().write(true).create_new(true).open(&tmp).and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store path for the parse and decide tests (no file is read).
    const STORE: &str = "store";
    const HASH: &str = "hash blake3-256\n";
    const RO: &str = "ro_compat test-ro-feature r-test\n";
    const COMPAT: &str = "compat test-compat-feature r-test\n";
    const INCOMPAT: &str = "incompat test-new-feature r-test\n";
    const ALL: [Want; 3] = [Want::Read, Want::Write, Want::Mount];

    fn decide_text(text: &str, want: Want) -> std::io::Result<Access> {
        let store = Path::new(STORE);
        decide(store, &parse(store, text)?, want)
    }

    /// A child process that another thread starts holds a copy of the lock's
    /// directory handle for a moment, so an immediate re-lock can see a false
    /// "held". The short retry in `try_store_lock` must hide it.
    #[test]
    fn a_relock_is_not_refused_while_another_thread_spawns_processes() {
        let d = tempfile::tempdir().unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::process::Command::new("true").status();
                }
            })
        };
        let refused = (0..300).filter(|_| lock_offline(d.path()).is_err()).count();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        spawner.join().unwrap();
        assert_eq!(refused, 0, "re-locks refused while another thread spawns");
    }

    fn ro_flag() -> Flag {
        Flag { class: Class::RoCompat, name: "test-ro-feature".to_string(), release: "r-test".to_string() }
    }

    /// WR-01: two parallel creates of the same new store both succeed and
    /// agree on one full header; no temp file stays.
    #[test]
    fn two_parallel_creates_of_a_new_store_agree() {
        for _ in 0..20 {
            let d = tempfile::tempdir().unwrap();
            let store = d.path().join("store");
            let go = std::sync::Arc::new(std::sync::Barrier::new(2));
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let (store, go) = (store.clone(), std::sync::Arc::clone(&go));
                    std::thread::spawn(move || {
                        go.wait();
                        check(&store, Want::Write).map(|o| o.access).map_err(|e| e.to_string())
                    })
                })
                .collect();
            for t in threads {
                assert_eq!(t.join().unwrap(), Ok(Access::ReadWrite));
            }
            assert_eq!(read(&store).unwrap(), Some(marker(DEFAULT_HASHER)));
            let names: Vec<_> = std::fs::read_dir(&store).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(names, [FORMAT_FILE], "only FORMAT is in the store");
        }
    }

    /// FMT-01, [Durable first]: a new store gets the format-7 header; `create`
    /// makes the header only, and never writes over a header.
    #[test]
    fn a_new_store_gets_a_header() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("store");
        crate::content::open(&store).unwrap();
        let text = read(&store).unwrap().expect("FORMAT exists");
        assert_eq!(text, marker(DEFAULT_HASHER));
        assert_eq!(text, format!("slicefs-store 7 (store header with feature flags)\n{HASH}"));
        let h = parse(&store, &text).unwrap();
        assert_eq!((h.format, h.flags().count(), h.hash()), (7, 0, Some("blake3-256")));

        let other = d.path().join("other");
        create(&other).unwrap();
        assert_eq!(read(&other).unwrap(), Some(marker(DEFAULT_HASHER)));
        assert!(!other.join("packs").exists() && !other.join("segments").exists(), "create makes no packs/ and no segments/");
        let kept = format!("{}{COMPAT}", marker(DEFAULT_HASHER));
        std::fs::write(other.join(FORMAT_FILE), &kept).unwrap();
        let err = create(&other).expect_err("a second create is refused");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(read(&other).unwrap(), Some(kept), "the header is the same");
    }

    /// [Number parse]: the number of the first line is parsed; its free text
    /// is not compared.
    #[test]
    fn the_number_is_parsed_not_the_line() {
        let h = parse(Path::new(STORE), &format!("slicefs-store 7 other free text\n{HASH}")).unwrap();
        assert_eq!(h.format, 7);
        assert_eq!(decide(Path::new(STORE), &h, Want::Write).unwrap(), Access::ReadWrite);
        assert_eq!(parse(Path::new(STORE), "slicefs-store 7\n").unwrap().format, 7);
        let err = parse(Path::new(STORE), &format!("slicefs-store x\n{HASH}")).expect_err("x").to_string();
        assert!(err.contains("unknown store format") && err.contains("slicefs-store x"), "{err}");
        let err = parse(Path::new(STORE), &format!("slicefs-store 8 (x)\n{HASH}")).expect_err("8").to_string();
        assert!(err.contains("newer SliceFS (store format 8)"), "{err}");
    }

    /// [Flags]: an unknown ro_compat flag gives read-only access; a write is
    /// refused with the name and the release.
    #[test]
    fn unknown_ro_compat_is_read_only() {
        let text = format!("{FORMAT}{HASH}{RO}");
        for want in [Want::Read, Want::Mount] {
            assert_eq!(decide_text(&text, want).unwrap(), Access::ReadOnly(ro_flag()), "{want:?}");
        }
        let err = decide_text(&text, Want::Write).expect_err("a write is refused").to_string();
        assert!(err.contains("test-ro-feature") && err.contains("r-test") && err.contains("can only read"), "{err}");
    }

    /// [Flags]: an unknown compat flag changes nothing.
    #[test]
    fn unknown_compat_is_read_write() {
        let text = format!("{FORMAT}{HASH}{COMPAT}");
        for want in ALL {
            assert_eq!(decide_text(&text, want).unwrap(), Access::ReadWrite, "{want:?}");
        }
    }

    /// [Incompat], [Parse order]: an unknown incompat flag refuses every
    /// `Want`, also next to a ro_compat flag (the strongest flag decides).
    #[test]
    fn unknown_incompat_is_refused_by_the_decision() {
        for text in [format!("{FORMAT}{HASH}{INCOMPAT}"), format!("{FORMAT}{HASH}{RO}{INCOMPAT}")] {
            for want in ALL {
                let err = decide_text(&text, want).expect_err("refused").to_string();
                assert!(err.contains("test-new-feature") && err.contains("r-test") && err.contains("cannot open"), "{want:?}: {err}");
            }
        }
    }

    /// [Parse order]: a line of an unknown kind is refused when no flag is
    /// unknown, and allowed (and kept in its place) next to an unknown flag.
    #[test]
    fn an_unknown_line_needs_an_unknown_flag() {
        let line = "chunk-profile 16 64 256";
        let err = decide_text(&format!("{FORMAT}{line}\n"), Want::Read).expect_err("refused").to_string();
        assert!(err.contains("FORMAT line 2: unknown setting") && err.contains(line), "{err}");
        let text = format!("{FORMAT}{line}\n{COMPAT}");
        assert_eq!(decide_text(&text, Want::Write).unwrap(), Access::ReadWrite);
        assert_eq!(parse(Path::new(STORE), &text).unwrap().to_text(), text, "the line stays in its place");
        assert_eq!(decide_text(&format!("{FORMAT}{line}\n{RO}"), Want::Mount).unwrap(), Access::ReadOnly(ro_flag()));
    }

    /// [Flags]: a flag line has three words.
    #[test]
    fn a_flag_line_has_three_words() {
        let err = parse(Path::new(STORE), &format!("{FORMAT}{HASH}ro_compat test-ro-feature\n")).expect_err("refused").to_string();
        assert!(err.contains("line 3: bad feature flag line"), "{err}");
    }

    /// [New store]: data (an entry in `packs/` or `segments/`) and no header
    /// is refused for every `Want`; with no data, only `Want::Write` writes
    /// the header.
    #[test]
    fn data_and_no_header_is_refused() {
        for dir in ["packs", "segments"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(d.path().join(dir)).unwrap();
            std::fs::write(d.path().join(dir).join("x"), b"").unwrap();
            for want in ALL {
                let err = check(d.path(), want).expect_err("refused").to_string();
                assert!(err.contains("FORMAT is missing") && err.contains("no store header"), "{dir} {want:?}: {err}");
            }
            assert!(!d.path().join(FORMAT_FILE).exists(), "{dir}: no header is written");
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("packs")).unwrap();
        std::fs::create_dir_all(d.path().join("segments")).unwrap();
        for want in [Want::Read, Want::Mount] {
            assert_eq!(check(d.path(), want).unwrap(), Opened { header: None, access: Access::ReadWrite }, "{want:?}");
            assert!(!d.path().join(FORMAT_FILE).exists(), "{want:?} writes nothing");
        }
        assert!(check(d.path(), Want::Write).unwrap().header.is_some());
        assert_eq!(read(d.path()).unwrap(), Some(marker(DEFAULT_HASHER)));
    }

    /// [Number]: a format below 7 gets the older-SliceFS text, above 7 the
    /// newer-SliceFS text, another first line `unknown store format`. The
    /// lines after another number are not read.
    #[test]
    fn older_and_newer_formats_are_refused() {
        let cases = [
            ("slicefs-store 4 (file content in packs)", "older SliceFS (metadata in vt0/)"),
            ("slicefs-store 5 (metadata and file content in packs)", "older SliceFS (224-bit object IDs)"),
            ("slicefs-store 6 (metadata and file content in packs, tagged object IDs)", "older SliceFS (store format 6, no feature flags)"),
            ("slicefs-store 8 (x)", "newer SliceFS (store format 8)"),
            ("slicefs-store", "unknown store format"),
            ("slicefs-store +7", "unknown store format"),
            ("hello", "unknown store format"),
        ];
        for (first, want) in cases {
            let err = parse(Path::new(STORE), &format!("{first}\nany other layout\n")).expect_err(first).to_string();
            assert!(err.contains(want), "{first}: {err}");
        }
    }

    /// [Flags]: a name is 1 to 64 of `a-z`, `0-9`, `-` (a letter or digit at
    /// each end, no `--`); a release is 1 to 32 visible ASCII characters.
    #[test]
    fn flag_names_and_releases_follow_the_grammar() {
        let flag = |name: &str, release: &str| parse(Path::new(STORE), &format!("{FORMAT}{HASH}compat {name} {release}\n"));
        let long = "a".repeat(65);
        for name in ["Test-x", "-a", "a-", "a--b", "a_b", long.as_str()] {
            let err = flag(name, "r-test").expect_err(name).to_string();
            assert!(err.contains("line 3: bad feature flag line"), "{name}: {err}");
        }
        for release in ["r".repeat(33), "r\u{1}t".to_string()] {
            let err = flag("test-x", &release).expect_err("release").to_string();
            assert!(err.contains("line 3: bad feature flag line"), "{release:?}: {err}");
        }
        assert!(flag(&"a".repeat(64), &"r".repeat(32)).is_ok());
        assert!(flag("test-x2", "r-test").is_ok());
    }

    /// [Flags]: one name appears once in the whole header.
    #[test]
    fn a_flag_is_listed_once() {
        for class in ["compat", "ro_compat"] {
            let text = format!("{FORMAT}compat test-x r-test\n{class} test-x r-test\n");
            let err = parse(Path::new(STORE), &text).expect_err(class).to_string();
            assert!(err.contains("line 3: the feature test-x is listed twice"), "{class}: {err}");
        }
    }

    /// [Hash line]: at most one `hash` line.
    #[test]
    fn one_hash_line_at_most() {
        let err = parse(Path::new(STORE), &format!("{FORMAT}{HASH}{HASH}")).expect_err("refused").to_string();
        assert!(err.contains("line 3: two hash lines"), "{err}");
    }

    /// The `chunk-profile` flag line of this build.
    fn profile_flag(class: &str) -> String {
        format!("{class} {CHUNK_PROFILE_FLAG} {CHUNK_PROFILE_RELEASE}\n")
    }

    /// [Chunks line]: the `chunks` line and the ro_compat `chunk-profile`
    /// flag come together or not at all (M9 with the line number).
    #[test]
    fn a_chunks_line_needs_the_flag_and_the_flag_needs_the_line() {
        let chunks = "chunks 16384 65536 262144\n";
        let ro = profile_flag("ro_compat");
        let cases = [
            (format!("{FORMAT}{HASH}{chunks}"), "line 3: chunks line with no chunk-profile flag"),
            (format!("{FORMAT}{HASH}{ro}"), "line 3: chunk-profile flag with no chunks line"),
            (format!("{FORMAT}{HASH}{chunks}{}", profile_flag("compat")), "line 4: the feature chunk-profile must be ro_compat"),
            (format!("{FORMAT}{HASH}{}{chunks}", profile_flag("incompat")), "line 3: the feature chunk-profile must be ro_compat"),
            (format!("{FORMAT}{HASH}{chunks}{ro}{chunks}"), "line 5: two chunks lines"),
        ];
        for (text, want) in cases {
            let err = parse(Path::new(STORE), &text).expect_err(want).to_string();
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    /// [Chunks line]: a `chunks` line has three sizes in decimal digits
    /// that `ChunkProfile::new` accepts; anything else is M9, never a panic.
    #[test]
    fn a_bad_chunks_line_is_refused() {
        let ro = profile_flag("ro_compat");
        for line in [
            "chunks 16384 65536",
            "chunks 16384 65536 262144 1",
            "chunks +16384 65536 262144",
            "chunks 16384 65536 0x40000",
            "chunks 12288 24576 49152",
            "chunks 65536 16384 262144",
            "chunks 32 256 1024",
            "chunks 4096 8192 33554432",
            "chunks 131072 524288 2097152",
            "chunks 16384 65536 99999999999",
        ] {
            let err = parse(Path::new(STORE), &format!("{FORMAT}{HASH}{line}\n{ro}")).expect_err(line).to_string();
            assert!(err.contains("line 3: bad chunks line"), "{line}: {err}");
        }
    }

    /// [Chunks line]: a header with a profile opens read and write for
    /// every `Want` (the flag is known here) and writes back byte for byte.
    #[test]
    fn a_profile_header_round_trips() {
        let (chunks, ro) = ("chunks 16384 65536 262144\n", profile_flag("ro_compat"));
        for text in [format!("{FORMAT}{HASH}{chunks}{ro}"), format!("{FORMAT}{HASH}{ro}{chunks}")] {
            let h = parse(Path::new(STORE), &text).unwrap();
            assert_eq!(h.chunk_profile(), Some(ChunkProfile::new(16384, 65536, 262144).unwrap()));
            for want in ALL {
                assert_eq!(decide(Path::new(STORE), &h, want).unwrap(), Access::ReadWrite, "{want:?}");
            }
            assert_eq!(h.to_text(), text);
        }
        let h = parse(Path::new(STORE), &format!("{FORMAT}{HASH}chunks 4096 8192 32768\n{ro}")).unwrap();
        assert_eq!(h.chunk_profile(), Some(ChunkProfile::DEFAULT));
    }

    /// [Compat], [Parse order]: a rewrite by `select_hash` keeps every flag
    /// line and every unknown line in its order; only the hash line changes.
    #[test]
    fn compat_flags_and_unknown_lines_are_kept_on_rewrite() {
        let d = tempfile::tempdir().unwrap();
        let (line, new) = ("chunk-profile 16 64 256\n", "hash test-keyed-blake3\n");
        let old = format!("{FORMAT}{COMPAT}{line}{HASH}");
        std::fs::write(d.path().join(FORMAT_FILE), &old).unwrap();
        crate::content::select_hash(d.path(), Some("test-keyed-blake3")).unwrap();
        assert_eq!(read(d.path()).unwrap().unwrap(), old.replace(HASH, new));
        std::fs::write(d.path().join(FORMAT_FILE), format!("{FORMAT}{COMPAT}{line}")).unwrap();
        crate::content::select_hash(d.path(), Some("test-keyed-blake3")).unwrap();
        assert_eq!(read(d.path()).unwrap().unwrap(), format!("{FORMAT}{new}{COMPAT}{line}"), "the hash line is line 2");
    }

    /// T-21.1-01: the read is bounded to 64 KiB, and the header is UTF-8 text.
    #[test]
    fn a_large_header_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let max = format!("{FORMAT}{}\n", "x".repeat(MAX_HEADER - FORMAT.len() - 1));
        assert_eq!(max.len(), MAX_HEADER);
        std::fs::write(d.path().join(FORMAT_FILE), &max).unwrap();
        assert_eq!(read(d.path()).unwrap().map(|s| s.len()), Some(MAX_HEADER), "64 KiB is read");
        std::fs::write(d.path().join(FORMAT_FILE), format!("{max}x")).unwrap();
        let err = read(d.path()).expect_err("refused").to_string();
        assert!(err.contains("larger than 64 KiB"), "{err}");
        std::fs::write(d.path().join(FORMAT_FILE), b"slicefs-store 7 \xff\xfe\n").unwrap();
        let err = read(d.path()).expect_err("not UTF-8");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
    }

    #[test]
    fn the_byte_core_keeps_the_header_rules() {
        let a64 = "a".repeat(64);
        for n in ["a", "a-b", "a1", a64.as_str()] {
            assert!(valid_name_bytes(n.as_bytes()), "{n}");
        }
        let a65 = "a".repeat(65);
        for n in ["", "A", "-a", "a-", "a--b", "a_b", a65.as_str()] {
            assert!(!valid_name_bytes(n.as_bytes()), "{n}");
        }
        let (g32, g33) = ("r".repeat(32), "r".repeat(33));
        assert!(valid_release_bytes(b"r") && valid_release_bytes(g32.as_bytes()));
        for r in ["", "a b", g33.as_str()] {
            assert!(!valid_release_bytes(r.as_bytes()), "{r}");
        }
        let first = |l: &str| first_line_number(l.as_bytes());
        assert_eq!(first("slicefs-store 7"), Some(7));
        assert_eq!(first("slicefs-store 07 free text"), Some(7));
        assert_eq!(first("slicefs-store 4294967295"), Some(u32::MAX));
        for l in ["slicefs-store 4294967296", "slicefs-store +7", "slicefs-store ", "slicefs-store 7x", "slicefs-storE 7"] {
            assert_eq!(first(l), None, "{l}");
        }
        assert_eq!(Class::from_word_bytes(b"compat"), Some(Class::Compat));
        assert_eq!(Class::from_word_bytes(b"ro_compat"), Some(Class::RoCompat));
        assert_eq!(Class::from_word_bytes(b"incompat"), Some(Class::Incompat));
        assert_eq!(Class::from_word_bytes(b"compatx"), None);
        assert_eq!(Class::from_word_bytes(b""), None);
    }
}

// Kani proofs of the byte-level core (TSAFE-04). Each harness writes its
// bound. Run with scripts/safety/kani.sh.
#[cfg(kani)]
mod proofs {
    use super::*;

    // Bound: names up to 8 bytes (the real limit is 64; the loops are the same).
    #[kani::proof]
    #[kani::unwind(10)]
    fn name_bytes_total() {
        let b: [u8; 8] = kani::any();
        let n: usize = kani::any();
        kani::assume(n <= 8);
        let _ = valid_name_bytes(&b[..n]);
    }

    // Bound: releases up to 8 bytes (the real limit is 32; the loop is the same).
    #[kani::proof]
    #[kani::unwind(10)]
    fn release_bytes_total() {
        let b: [u8; 8] = kani::any();
        let n: usize = kani::any();
        kani::assume(n <= 8);
        let _ = valid_release_bytes(&b[..n]);
    }

    // Bound: the 14 prefix bytes and 12 symbolic bytes (u32::MAX has 10
    // digits, so 12 bytes reach the overflow). Some(v) means that every
    // byte before the first space is a digit.
    #[kani::proof]
    #[kani::unwind(28)]
    fn first_line_number_total() {
        let tail: [u8; 12] = kani::any();
        let mut line = [0u8; 26];
        line[..14].copy_from_slice(b"slicefs-store ");
        line[14..].copy_from_slice(&tail);
        if first_line_number(&line).is_some() {
            let end = tail.iter().position(|&b| b == b' ').unwrap_or(tail.len());
            assert!(end > 0 && tail[..end].iter().all(u8::is_ascii_digit));
        }
    }

    // Bound: class words up to 9 bytes (the longest word, `ro_compat`, has 9).
    #[kani::proof]
    #[kani::unwind(10)]
    fn class_word_total() {
        let b: [u8; 9] = kani::any();
        let n: usize = kani::any();
        kani::assume(n <= 9);
        let _ = Class::from_word_bytes(&b[..n]);
    }
}
