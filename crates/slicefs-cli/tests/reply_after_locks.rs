//! Replies are sent after all locks are released (POOL-02, ROADMAP
//! criterion 2).
//!
//! This is the source half of the check. It reads the source and asserts
//! that `fuse_callbacks.rs` takes the commit gate only in `serve` (and in
//! `init`), and that every request callback goes through `serve`. Then no
//! callback can send its reply while it holds the gate.
//!
//! The dynamic half is the debug assert in `serve`: its result must not keep
//! a ranked lock alive at the reply point (unit test
//! `serve_panics_when_a_lock_is_held_at_the_reply`).

use std::collections::HashMap;

const FUSE_CALLBACKS: &str = include_str!("../src/fuse_callbacks.rs");

/// The only functions that may take the gate.
const GATE_FNS: [&str; 2] = ["serve", "init"];

/// The number of request callbacks: all methods of the `Filesystem` impl
/// except `init` and `destroy`.
const REQUEST_CALLBACKS: usize = 31;

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

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// Every non-comment line that takes the gate, as (enclosing function,
/// 1-based line). The enclosing function is the nearest previous `fn` header.
fn gate_sites(src: &str) -> Vec<(String, usize)> {
    let mut current = String::from("<none>");
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        if let Some(name) = fn_header_name(line) {
            current = name.to_string();
        }
        if !is_comment(line) && line.contains("gate_shared(") {
            out.push((current.clone(), i + 1));
        }
    }
    out
}

/// The methods of `impl Filesystem for MountedFs`, as (name, non-comment
/// body). The impl ends at the first `}` in column 0.
fn callbacks(src: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut inside = false;
    for line in src.lines() {
        if !inside {
            inside = line.starts_with("impl Filesystem for MountedFs");
            continue;
        }
        if line == "}" {
            break;
        }
        if line.starts_with("    ")
            && !line.starts_with("     ")
            && let Some(name) = fn_header_name(line)
        {
            out.push((name.to_string(), String::new()));
        }
        if let Some((_, body)) = out.last_mut()
            && !is_comment(line)
        {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// The free functions (header in column 0), as name -> non-comment body.
fn free_fns(src: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut current: Option<(String, String)> = None;
    for line in src.lines() {
        if current.is_none()
            && !line.starts_with(' ')
            && let Some(name) = fn_header_name(line)
        {
            current = Some((name.to_string(), String::new()));
        }
        if let Some((_, body)) = current.as_mut()
            && !is_comment(line)
        {
            body.push_str(line);
            body.push('\n');
        }
        if line == "}"
            && let Some((name, body)) = current.take()
        {
            out.insert(name, body);
        }
    }
    out
}

#[test]
fn the_gate_is_taken_only_in_serve_and_init() {
    let sites = gate_sites(FUSE_CALLBACKS);
    let list: Vec<String> = sites.iter().map(|(n, l)| format!("{n}:{l}")).collect();
    let outside: Vec<&String> =
        sites.iter().zip(&list).filter(|((n, _), _)| !GATE_FNS.contains(&n.as_str())).map(|(_, s)| s).collect();
    assert!(
        outside.is_empty(),
        "the gate is taken outside {GATE_FNS:?} ({} sites): {outside:?}",
        outside.len()
    );
    assert_eq!(sites.len(), 2, "expected 2 gate sites (serve and init), found {list:?}");
}

#[test]
fn every_request_callback_uses_serve() {
    let cbs = callbacks(FUSE_CALLBACKS);
    let names: Vec<&str> = cbs.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"init") && names.contains(&"destroy"), "the impl parser found {names:?}");
    let helpers = free_fns(FUSE_CALLBACKS);
    assert!(helpers.contains_key("serve"), "no free fn `serve`");
    let mut missing = Vec::new();
    for (name, body) in cbs.iter().filter(|(n, _)| n != "init" && n != "destroy") {
        let via_job = helpers.iter().any(|(h, hb)| {
            h.ends_with("_job") && body.contains(&format!("{h}(")) && hb.contains("serve(")
        });
        if !body.contains("serve(") && !via_job {
            missing.push(name.as_str());
        }
    }
    assert!(missing.is_empty(), "callbacks that do not use serve ({}): {missing:?}", missing.len());
    assert_eq!(cbs.len() - 2, REQUEST_CALLBACKS, "request callbacks found: {names:?}");
}
