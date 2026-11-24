//! The store header: the text file `<store>/FORMAT`.
//!
//! The first line names the store format (`slicefs-store 7 ...`). Each
//! later line is one item: the store setting `hash <name>` (the write
//! hash), or a feature flag `<class> <name> <release>`, where the class is
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

use chunk_store::{ObjectHasher, DEFAULT_HASHER};

/// The header file in the store root.
pub const FORMAT_FILE: &str = "FORMAT";
/// The store format number that this build reads and writes.
pub const FORMAT_NUMBER: u32 = 7;
/// The first line that this build writes.
pub const FORMAT: &str = "slicefs-store 7 (store header with feature flags)\n";
/// The largest header that [`read`] accepts, in bytes.
pub const MAX_HEADER: usize = 64 * 1024;
/// The feature flags that this build knows. Format 7 ships no flag; Phases
/// 22 and 23 add names here with the rule of docs/design/store-format.md.
pub const KNOWN_FLAGS: &[&str] = &[];

/// The header of a new store whose new objects use `hasher`.
pub fn marker(hasher: &dyn ObjectHasher) -> String {
    format!("{FORMAT}hash {}\n", hasher.name())
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
        match w {
            "compat" => Some(Class::Compat),
            "ro_compat" => Some(Class::RoCompat),
            "incompat" => Some(Class::Incompat),
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

fn msg_ro_write(store: &Path, flag: &Flag) -> String {
    let (store, name, release) = (store.display(), &flag.name, &flag.release);
    format!("{store} uses the feature {name}, which needs SliceFS {release} or later to write. This SliceFS can only read it.")
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
/// number is parsed (the free text is not compared). Every later line is
/// kept in its order: `hash <name>`, a flag line of three words, or a line
/// of another kind. A flag line of another word count is an error.
pub fn parse(store: &Path, text: &str) -> std::io::Result<Header> {
    let mut pieces: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        pieces.pop();
    }
    let first = pieces.first().copied().unwrap_or("");
    let format = parse_first_line(first).ok_or_else(|| refuse(msg_unknown_format(store, first)))?;
    if format != FORMAT_NUMBER {
        return Err(refuse(msg_unknown_format(store, first)));
    }
    let mut lines = Vec::new();
    for (i, &line) in pieces.iter().enumerate().skip(1) {
        let words: Vec<&str> = line.split(' ').collect();
        let parsed = match (words[0], Class::from_word(words[0])) {
            ("hash", _) if words.len() == 2 => Line::Hash(words[1].to_string()),
            (_, Some(class)) => {
                if words.len() != 3 {
                    return Err(refuse(msg_bad_line(store, i + 1, "bad feature flag line", line)));
                }
                Line::Flag(Flag { class, name: words[1].to_string(), release: words[2].to_string() })
            }
            _ => Line::Other(line.to_string()),
        };
        lines.push(parsed);
    }
    Ok(Header { format, first: first.to_string(), lines })
}

/// The format number of a first line `slicefs-store <digits>[ <free text>]`.
fn parse_first_line(first: &str) -> Option<u32> {
    let rest = first.strip_prefix("slicefs-store ")?;
    let number = rest.split_once(' ').map_or(rest, |(n, _free)| n);
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
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
/// No header: with `Want::Write` the store gets a new header ([`create`])
/// and the check goes on with it; with `Want::Read` or `Want::Mount`
/// nothing is written and the access is read and write (`header: None`; a
/// mount needs `segments/`, and `load_store` refuses a store with no
/// committed state).
pub fn check(store: &Path, want: Want) -> std::io::Result<Opened> {
    let text = match read(store)? {
        Some(text) => text,
        None if want == Want::Write => {
            match create(store) {
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

// ── Write ─────────────────────────────────────────────────────────────────────

/// Make the header of a new store: the first step of a new store, before
/// `packs/` or `segments/` exist. Makes the store directory if needed and
/// writes [`marker`]`(DEFAULT_HASHER)` durably. Refuses (`AlreadyExists`)
/// when FORMAT exists; the file stays as it is. Tests that build a store
/// with `StoreIo::open` or `ChunkStore::open` call it first.
pub fn create(store: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(store)?;
    if store.join(FORMAT_FILE).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{}/FORMAT exists", store.display()),
        ));
    }
    write_text(store, &marker(DEFAULT_HASHER))
}

/// Write `header` as the header of `store`, durably (see [`write_text`]).
pub fn write_header(store: &Path, header: &Header) -> std::io::Result<()> {
    write_text(store, &header.to_text())
}

/// Write `text` as the header of `store` durably: a synced temp file
/// renamed over the old header, then a sync of the directory. A power loss
/// leaves the old header or the new one, never an empty or torn file.
pub(crate) fn write_text(store: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = store.join(format!("{FORMAT_FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, store.join(FORMAT_FILE))?;
    std::fs::File::open(store)?.sync_all()
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

    fn ro_flag() -> Flag {
        Flag { class: Class::RoCompat, name: "test-ro-feature".to_string(), release: "r-test".to_string() }
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
        for first in ["slicefs-store x", "slicefs-store 8 (x)"] {
            let err = parse(Path::new(STORE), &format!("{first}\n{HASH}")).expect_err(first).to_string();
            assert!(err.contains("unknown store format") && err.contains(first), "{err}");
        }
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
}
