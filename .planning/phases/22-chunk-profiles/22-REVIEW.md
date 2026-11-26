---
phase: 22-chunk-profiles
reviewed: 2026-11-26T10:58:30Z
depth: standard
files_reviewed: 19
files_reviewed_list:
  - README.md
  - crates/chunk-store/examples/chunk_profile_bench.rs
  - crates/chunk-store/src/file.rs
  - crates/chunk-store/src/lib.rs
  - crates/chunk-store/tests/profile_tests.rs
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/header.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/mount.rs
  - crates/slicefs-cli/src/seed.rs
  - crates/slicefs-cli/src/stats.rs
  - crates/slicefs-cli/tests/chunk_profile_tests.rs
  - docs/design/chunk-store.md
  - docs/design/store-format.md
  - docs/research/chunk-profiles.md
  - scripts/chunk-profiles/README.md
  - scripts/chunk-profiles/bench.sh
  - scripts/docker-linux-test/run.sh
findings:
  critical: 0
  warning: 5
  info: 8
  total: 13
  necessity: 1
status: issues_found
---

# Phase 22: Code Review Report

**Reviewed:** 2026-11-26T10:58:30Z
**Depth:** standard
**Files Reviewed:** 19
**Status:** issues_found

## Summary

Scope: `git diff 1e7c158 HEAD` (HEAD `1bdd735`) for the 19 files above. I read them against 22-CONTEXT.md and the four plan summaries. I also read the callers that the diff does not show: `FileWriter::write_zeros`/`finish`, `metadata::store_io`, `mount::run_mount`, the dirty-cost code in `filesystem.rs`, and `ChunkStore::put` size limits.

Verified, no finding:

- **ChunkProfile validation** (`lib.rs:144-169`). The checks run in this order: powers of two (this also refuses 0), then strict min < avg < max, then the fastcdc v2020 limits, then the zero premise (2 x max + 1 zeros must cut into `[max, max, 1]`). Because of this order, FastCDC never gets values that make it panic. The zero premise only needs the bytes `[s, s+max)`: v2020 resets the hash at each chunk start and clamps `remaining` to max. So one check covers every zero run that starts on a cut, and `known_zero` asks for `start + max <= new_len`. Fields are private, so an invalid profile cannot be built. `DEFAULT` is checked against `new` in `profile_tests.rs:34`.
- **No process-wide constant left.** No non-test code in `chunk-store`, `metadata` or `slicefs-cli` reads `CHUNK_MIN/AVG/MAX` or `chunk_ranges`, and no static zero buffer remains. The zero chunk is a per-store `OnceLock` (`lib.rs:983, 2998`). The writer buffer limit, `cut`, the reader zero path, `known_zero`, the splice re-chunk and the read-ahead step all read `store.profile()`. `metadata::store_io::put_value` uses `FileWriter` with the store, so metadata follows the store profile. Every `ChunkStore::open`/`StoreIo::open` call outside a test module goes through `content::open`/`open_read_only`, which set `profile_of(header)`. One stale comment remains in `filesystem.rs:2578` (not in scope): "a buffer under 2 x CHUNK_MAX".
- **Header pairing** (`header.rs:379-391`). Every text line after the first adds exactly one entry to `lines`, so `j + 2` and `pieces[j + 1]` give the right line number and text. These cases are refused as M9: a `chunks` line without the flag, the flag without the line, a flag class other than `ro_compat`, two `chunks` lines, and a bad `chunks` line (a sign, hex, a missing size or a u32 overflow).
- **Hash change keeps the `chunks` line.** `to_text` writes `Line::Chunks` back, and the hash line goes in at index 0. Test: `a_hash_change_keeps_the_chunk_profile`.
- **Refusals.** In `check_write_with`, the hash name is checked first, then `check_with` (on an existing store it only reads), then `check_profile`, and only then the hash rewrite. A refused profile writes nothing to an existing store, and the tests compare the store tree before and after. In `mount::start`, the profile check comes right after the header check, before the read-only refusals and before any lock or load. This matches the M16 order in `store-format.md`.
- **The #201 merge.** `run_seed_with` calls `create_dir_all`, then `lock_offline`, then the header check (`seed.rs:51-57`).
- **Docs against code.** All five new M9 reason strings, the M16 text, the stats line order and the JSON field match the code exactly. I checked the report tables in `docs/research/chunk-profiles.md` (trees and tar: objects, stored, apparent, seq/rand p50/p99, edit sums) against the `run=median` rows of `runs/chunk-profiles-results.csv`, and they agree.
- **Object size.** At 16 MiB max, objects stay under the `u32` record limit (`lib.rs:1645`) and the 64 MiB pack limit.
- **bench.sh smoke mode and quoting.**
  - `SMOKE=1` refuses a data-dir argument (`bench.sh:40`) and puts its generated data only under `$WORK/smoke-data` (`bench.sh:61`), so `gen` cannot overwrite real Boost data.
  - `set -euo pipefail` is on.
  - The unquoted `$BOOST_VERS`, `$PROFILES` and `$INPUTS` (`bench.sh:75, 89, 138, 145, 147`) are intended word lists.
  - Paths and the `SRC` array are quoted.

  The defects that remain are WR-03, WR-04, IN-03 and IN-04.
- **Tests.** I read `profile_tests.rs` and `chunk_profile_tests.rs` in full. They check what they claim: the golden cut points, two stores in one process, zero reads with store reads failing, an old reader, and refusals that leave the tree unchanged. The report text in `docs/research/chunk-profiles.md`, read in full, matches its CSV. I checked the derived percents for 8/32/128 and 16/64/256 against the median rows.

The main concerns:

- A failed `seed` fixes the profile of a store that has no data. I reproduced this with the built binary.
- At large profiles, the stream buffer is not counted in the dirty budget.
- The benchmark script has two safety and exit-status defects.

## Narrative Findings (AI reviewer)

## Warnings

### WR-01: A failed `seed` fixes the chunk profile of a store with no data

**File:** `crates/slicefs-cli/src/seed.rs:51-70` (with `crates/slicefs-cli/src/content.rs:133-143`)
**Issue:** `run_seed_with` writes the header and then opens the chunk store (`content::store_io`, which makes `packs/0000000000000001.pack` and `index/`). Only after that does it read `std::fs::metadata(source_dir)`. A typo in the source path still leaves a store whose profile can never change, even though it has no root and no file. Reproduced at HEAD:

```
slicefs seed --chunk-profile 16/64/256 STORE MISSING   -> "No such file or directory (os error 2)", rc 1
                                                         STORE/FORMAT now has "chunks 16384 65536 262144" + flag,
                                                         STORE/packs/0000000000000001.pack exists
slicefs seed --chunk-profile 8/16/64 STORE SRC          -> refused (M16), rc 1
slicefs seed STORE SRC                                  -> rc 0, silently seeds with 16/64/256
```

The user did not get the profile they asked for on the second try. Because a pack now exists, `has_data` is true, so even deleting FORMAT by hand gives M1 (header missing) and not a fresh store. The only way out is `rm -rf STORE`. The `--hash` path has the same order, but a hash can be rewritten later. A profile cannot, so Phase 22 makes this order a one-way trap.
**Fix:** Check the source before any store side effect:

```rust
pub fn run_seed_with(store_path: &Path, source_dir: &Path, hash: Option<&str>, profile: Option<ChunkProfile>) -> Result<(), Box<dyn std::error::Error>> {
    let source_meta = std::fs::metadata(source_dir)
        .map_err(|e| format!("source {}: {e}", source_dir.display()))?;
    if !source_meta.is_dir() {
        return Err(format!("source {} is not a directory", source_dir.display()).into());
    }
    std::fs::create_dir_all(store_path)?;
    let _store_lock = crate::header::lock_offline(store_path)?;
    ...
}
```

Then pass `source_meta` down to the place that sets the root inode. Add a test: seed with a missing source, then confirm that no FORMAT exists and that seed with another profile works.

### WR-02: The stream buffer of a large profile is outside the dirty budget

**File:** `crates/slicefs-cli/src/filesystem.rs:301-306` (cost); `crates/chunk-store/src/file.rs:134-140` (buffer); `crates/chunk-store/src/lib.rs:157` (limit up to 16 MiB)
**Issue:** `dirty_cost` counts only `pending` for a streaming file. The `FileWriter` buffer of the stream (up to 2 x max − 1 after a write) is not counted, and neither is the clone made in `stream_step_in`. At 4/8/32 KiB this was under 64 KiB per file and did not matter. With the new limit of max = 16 MiB, each streaming file holds up to about 32 MiB, and about 64 MiB during a step, outside `--dirty-budget`. Example: 50 files copied at the same time into a 1/4/16 MiB store hold about 1.6 GiB (3.2 GiB during steps), while `--dirty-budget 64M` is set. `cli.rs:105` tells the user that the budget "is memory that the daemon may use". D-17 accepted the per-file bound (it is bounded, not O(file size)), but the global promise of the budget does not hold. `chunk-store.md` gives only the per-stream number.
**Fix:** Cap max first. Make `ChunkProfile::new` (or `parse_chunk_profile` and `parse_chunks`) refuse a max above a store-level cap, for example 1 MiB. That keeps the buffer at most 2 MiB per stream and still covers all five benchmarked profiles. Then document the total, not only the per-stream cost, in `chunk-store.md`.

Do not simply add the buffer to `dirty_cost`. Steps cannot free those bytes, because the buffer stays at up to 2 x max until the stream ends. So `throttle_write` would keep writers waiting while `total >= budget`, and `pay_budget` would end each time at its `step == 0` exit (`filesystem.rs:3545`). If the buffer must be counted, count it as a fixed reservation per open stream that comes off the budget (`budget - reserved`), not as dirty bytes that a step can free.

### WR-03: `bench.sh` deletes a `store` directory that it did not make

**File:** `scripts/chunk-profiles/bench.sh:44-46, 149, 174`; `scripts/chunk-profiles/README.md:85`
**Issue:** `WORK` comes from the user, and the only guards are "not empty" and "not `/`". `STORE=$WORK/store` is then removed with `rm -rf` before and after every step, whether or not the script made it. Scenario: `WORK=/data/slicefs bench.sh DIR`, where `/data/slicefs/store` is a real SliceFS store. Its whole contents are deleted at the first step. The same run also overwrites `results.csv`, `edits.csv` and `machine.txt` in that directory. The README says, wrongly, "The script removes only `WORK/store`, which it makes".
**Fix:** Refuse a `WORK` that already holds a `store` child at start, or use a store path that the script owns:

```bash
[ ! -e "$WORK/store" ] || { echo "refusing: $WORK/store exists (not made by this script)" >&2; exit 2; }
STORE=$(mktemp -d "$WORK/store.XXXXXX")   # and rm -rf only this path
```

Then correct README line 85 to match.

### WR-04: A failure of the median step is ignored, and the script exits 0

**File:** `scripts/chunk-profiles/bench.sh:183-192`; `scripts/chunk-profiles/README.md:86-87`
**Issue:** `row "$(python3 -c '...' "$R" "$P" "$IN")"` loses the exit status of the command substitution, because the status of `row` is the status of its last `echo`. `set -e` and `pipefail` do not catch this. These cases all write empty `median` lines to stdout and `results.csv`, and the script exits 0:
- `python3` is missing (`command not found`);
- an empty field from `kv` makes `float('')` raise;
- any other Python error.

The README says that "the script stops with a non-zero exit code when a step fails". The report is built from the median rows, so a broken run can look complete.
**Fix:** Do the substitution first, then check it:

```bash
command -v python3 > /dev/null || { echo "python3 is needed" >&2; exit 2; }   # at start
...
med=$(python3 -c '...' "$R" "$P" "$IN") || { echo "median failed: $P $IN" >&2; exit 1; }
[ -n "$med" ] || { echo "empty median row: $P $IN" >&2; exit 1; }
row "$med"
```

### WR-05: Impossible-state guard: `human_stats` checks a profile again, and its fallback never runs

**File:** `crates/slicefs-cli/src/stats.rs:346`
**Category:** necessity
**Issue:** `git grep -n ProfileOut -- crates` finds one constructor outside tests: `stats.rs:255`, from a `ChunkProfile` that `profile_of` already checked. The other constructor is the test at `stats.rs:634`. So `ChunkProfile::new(p.min, p.avg, p.max)` cannot fail here, and the `map_or_else(|_| format!("{}/{}/{} B", ..))` fallback is a branch that never runs. The extra call is not free: `new` runs the zero premise again, an allocation of 2 x max + 1 bytes and a FastCDC pass over it (about 32 MiB at the 16 MiB limit), only to format one line.
**Fix:** Keep the checked profile. For example, add `#[serde(skip)] profile: ChunkProfile` next to `chunk_profile` in `StoreStats` (or keep a `Display` string), and print it directly: `writeln!(o, "Chunk profile    : {}", stats.profile)`.

## Info

### IN-01: The zero premise runs on every header parse

**File:** `crates/chunk-store/src/lib.rs:161-167`, called from `crates/slicefs-cli/src/header.rs:401`
**Issue:** Each `parse` of a header with a `chunks` line allocates 2 x max + 1 zero bytes and chunks them. One `mount` parses the header at least three times (`run_mount` pre-flight at `mount.rs:690`, `start`, `content::open`). At max = 16 MiB that is about 100 MB of allocation and scan per mount, and more for `stats` (see WR-05). The cost is bounded, and the check on header data is right, because the header crosses a trust boundary. But the work repeats.
**Fix:** Cache the result per `(min, avg, max)` in a small `static OnceLock<Mutex<HashMap<..>>>`, or allocate once and reuse a thread-local zero buffer. The check still runs once per process for each profile.

### IN-02: A `--chunk-profile` error says "chunk profile" twice and gives bytes for a KiB input

**File:** `crates/slicefs-cli/src/content.rs:92`
**Issue:** `slicefs seed --chunk-profile 12/24/48 ...` prints `chunk profile 12/24/48: chunk profile 12288/24576/49152 bytes: each size must be a power of two`. The user typed KiB but sees bytes. The limit message also gives the limits in B/MiB, while the option takes KiB.
**Fix:** Split the reason from its prefix in `ChunkProfile::new`, for example with an error enum or a reason-only string, and let each caller add its own context: KiB for the CLI, bytes for the header.

### IN-03: `bench.sh` writes outside `WORK`: it creates a mistyped data dir, and the rand/sparse files go into DATA

**File:** `scripts/chunk-profiles/bench.sh:71, 94-99`
**Issue:** `mkdir -p "$D"` runs before the inputs are checked (line 105). So `bench.sh /dat` creates an empty `/dat` and only then exits 2 with `missing /dat/x/boost_...`. The `rand` and `sparse` inputs write 1 GiB files (default) into the data dir. The README documents this, but it breaks "no writes outside WORK". These two inputs (not `trees` or `tar`) also fail when the data dir is mounted read-only (`-v DIR:/data:ro`, the Docker run that the README suggests).
**Fix:** Outside smoke mode, use `[ -d "$D" ] || { echo "no data dir: $D" >&2; exit 2; }` in place of `mkdir -p`. Put the generated rand/sparse files under `$WORK/inputs/` unless an opt-in variable says otherwise.

### IN-04: The `chunk-profiles-smoke` Docker entry has no time limit

**File:** `scripts/docker-linux-test/run.sh:73-85`
**Issue:** The new entry runs a release build and the whole bench with no `timeout`. The `index-bench` entry next to it wraps its binary in `timeout 1800` (`run.sh:98`). `bench.sh` itself has no limit on any step, and the report gets its 5400 s limit only from the outer command line. A hang in the container (for example in `splice` on new data) blocks the session with no bound. The project budget asks for short waits.
**Fix:** `timeout 600 bash /src/scripts/chunk-profiles/bench.sh || exit 1` (and `timeout 1200` on the `cargo build`). Also show the `timeout` wrapper in the README run lines.

### IN-05: The report command line cannot run on a fresh machine

**File:** `docs/research/chunk-profiles.md:54-57`
**Issue:** `> "$OUT.stdout.csv" 2> "$OUT.progress.log"` opens files in `$M/results/` before `bench.sh` runs, and the shell does not create `$M/results`. On a machine without that directory, the shell fails with "No such file or directory" and the benchmark never starts. So the "reproducible" command lines do not reproduce as written.
**Fix:** Add `mkdir -p "$M/results"` before the `WORK=...` line.

### IN-06: The mount refuses a different profile only after the macOS backend selection

**File:** `crates/slicefs-cli/src/mount.rs:689-705` and `406-411`
**Issue:** The `run_mount` pre-flight header check ("an early refusal, before the backend choice") does not compare `--chunk-profile`. The M16 refusal comes only in `start`, after the backend selection (`select_backend_auto`). So on macOS, a mount with the wrong profile goes through that step first, and an error there can hide the real refusal. The order in `store-format.md` is still right for `start`.
**Fix:** Use the `Opened` from the pre-flight check at line 690: `let opened = header::check(..)?; if opened.header.is_some() { content::check_profile(store_path, opened.header.as_ref(), chunk_profile)?; }`.

### IN-07: The `--json` error output is not valid JSON for the new M9 messages

**File:** `crates/slicefs-cli/src/main.rs:37, 47, 57`
**Issue:** `eprintln!("{{\"error\": \"{e}\"}}")` puts the message in without escaping. M9 quotes the line (`{line:?}`), so the new reasons, for example `FORMAT line 3: chunks line with no chunk-profile flag: "chunks 16384 65536 262144"`, produce output with unescaped `"` that a JSON parser rejects. The pattern was there before; Phase 22 adds five more messages that trigger it.
**Fix:** `eprintln!("{}", serde_json::json!({ "error": e.to_string() }));`

### IN-08: `StoreIo::open` is public, ignores the store header, and opens with the default profile

**File:** `crates/metadata/src/store_io.rs:160-166` (not in the changed set)
**Issue:** The doc says it is for "tests, tools", and today only test modules call it. But it is `pub` and opens `Options::default()`. A future tool that uses it on a store with a non-default profile would write metadata values with 4/8/32 KiB. That breaks the one-profile-per-store rule (PROF-02) without any error, and the header check of D-20 never runs.
**Fix:** Mark it `#[cfg(any(test, feature = "test-util"))]`, or make it take `Options` and document that callers must pass `content::open` values.

---

_Reviewed: 2026-11-26T10:58:30Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
