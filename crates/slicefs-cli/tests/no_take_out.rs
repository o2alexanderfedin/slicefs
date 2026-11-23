//! No request takes another file's state out (Phase 20, ROADMAP criterion 1).
//!
//! This is the source half of the check. It reads the source and asserts
//! that only four functions remove a cell from the cell map, and that the
//! writer-side budget function pays for its own file only (by name).
//!
//! The dynamic half is the rank rule: a thread that takes a second
//! `Rank::Cell` lock panics in debug builds (`chunk-store` `sync.rs`), and
//! the flusher thread calls `require_gate` (plan 20-02).
//!
//! The flusher (`flusher.rs`) steps cells of other files, so it must never
//! take one out of the map: its source has no removal text at all.

const FILESYSTEM: &str = include_str!("../src/filesystem.rs");
const HANDLERS: &str = include_str!("../src/handlers.rs");
const FUSE_CALLBACKS: &str = include_str!("../src/fuse_callbacks.rs");
const FLUSHER: &str = include_str!("../src/flusher.rs");

/// The only functions that may remove a cell from the cell map.
const ALLOWED: [&str; 4] = ["finalize", "finalize_locked", "reclaim_inode", "setattr_size_logged"];

/// The function name of a header line: optional `pub` or `pub(...)`, optional
/// `const`/`async`/`unsafe`, then `fn NAME` followed by `(` or `<`.
fn fn_header_name(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    if let Some(r) = rest.strip_prefix("pub") {
        let r = if r.starts_with('(') { &r[r.find(')')? + 1..] } else { r };
        if !r.starts_with(' ') {
            return None;
        }
        rest = r.trim_start();
    }
    loop {
        let before = rest;
        for q in ["const ", "async ", "unsafe "] {
            if let Some(r) = rest.strip_prefix(q) {
                rest = r.trim_start();
            }
        }
        if rest == before {
            break;
        }
    }
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    let (name, tail) = rest.split_at(end);
    (!name.is_empty() && (tail.starts_with('(') || tail.starts_with('<'))).then_some(name)
}

/// Every line that removes a cell, as (enclosing function, 1-based line).
fn removal_sites(src: &str) -> Vec<(String, usize)> {
    let mut current = String::from("<none>");
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        if let Some(name) = fn_header_name(line) {
            current = name.to_string();
        }
        if line.contains("self.cells.remove(") {
            out.push((current.clone(), i + 1));
        }
    }
    out
}

#[test]
fn only_four_functions_remove_a_cell() {
    let sites = removal_sites(FILESYSTEM);
    let list: Vec<String> = sites.iter().map(|(n, l)| format!("{n}:{l}")).collect();
    let outside: Vec<&String> = sites.iter().zip(&list).filter(|((n, _), _)| !ALLOWED.contains(&n.as_str())).map(|(_, s)| s).collect();
    assert!(outside.is_empty(), "cell removal outside {ALLOWED:?}: {outside:?} (all sites: {list:?})");
    assert_eq!(sites.len(), 4, "expected 4 cell removal sites, found {list:?}");
}

#[test]
fn the_budget_step_is_own_file_only() {
    // Built from two parts, so this file does not hold the joined name.
    let old = format!("fn {}_{}", "keep", "budget");
    let old_decl: Vec<usize> = FILESYSTEM
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains(&old) && fn_header_name(l).is_some())
        .map(|(i, _)| i + 1)
        .collect();
    assert!(old_decl.is_empty(), "the old cross-file budget function is back at lines {old_decl:?}");
    let pay = FILESYSTEM.matches("fn pay_budget(").count();
    assert_eq!(pay, 1, "expected one `fn pay_budget(`, found {pay}");
    // This part only keeps the old name from coming back; the allow-list
    // above and the debug rank rule are the real checks.
    for (file, src) in [("filesystem.rs", FILESYSTEM), ("handlers.rs", HANDLERS), ("fuse_callbacks.rs", FUSE_CALLBACKS)] {
        assert!(!src.contains("open_files"), "{file} has the token `open_files`");
    }
}

#[test]
fn the_flusher_takes_no_cell_out() {
    let lines: Vec<usize> =
        FLUSHER.lines().enumerate().filter(|(_, l)| l.contains("cells.remove(")).map(|(i, _)| i + 1).collect();
    assert!(lines.is_empty(), "flusher.rs removes a cell at lines {lines:?}");
}

#[test]
fn header_parser_finds_names() {
    assert_eq!(fn_header_name("    fn finalize(&self) {"), Some("finalize"));
    assert_eq!(fn_header_name("    pub(crate) fn reclaim_inode(&self) {"), Some("reclaim_inode"));
    assert_eq!(fn_header_name("pub fn f<T>(x: T) {"), Some("f"));
    assert_eq!(fn_header_name("    pub const unsafe fn g() {"), Some("g"));
    assert_eq!(fn_header_name("    // fn finalize(&self)"), None);
    assert_eq!(fn_header_name("    let f = |x| x;"), None);
}
