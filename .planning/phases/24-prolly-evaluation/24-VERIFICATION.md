---
phase: 24-prolly-evaluation
verified: 2026-11-26T22:45:03Z
status: human_needed
score: 2/3 must-haves verified
covered_files:
  - .planning/phases/24-prolly-evaluation/24-01-PLAN.md
  - .planning/phases/24-prolly-evaluation/24-01-SUMMARY.md
  - .planning/phases/24-prolly-evaluation/24-02-PLAN.md
  - .planning/phases/24-prolly-evaluation/24-02-SUMMARY.md
  - .planning/phases/24-prolly-evaluation/24-03-PLAN.md
  - .planning/phases/24-prolly-evaluation/24-03-SUMMARY.md
  - .planning/phases/24-prolly-evaluation/24-04-PLAN.md
  - .planning/phases/24-prolly-evaluation/24-04-SUMMARY.md
  - .planning/phases/24-prolly-evaluation/24-05-PLAN.md
  - .planning/phases/24-prolly-evaluation/24-05-SUMMARY.md
  - Cargo.toml
  - benchmarks/prolly-bench/Cargo.toml
  - benchmarks/prolly-bench/build.rs
  - benchmarks/prolly-bench/src/main.rs
  - benchmarks/prolly-bench/src/replay.rs
  - benchmarks/prolly-bench/src/whole.rs
  - benchmarks/prolly-bench/tests/smoke.rs
  - crates/prolly/Cargo.toml
  - crates/prolly/src/apply.rs
  - crates/prolly/src/lib.rs
  - crates/prolly/src/node.rs
  - crates/prolly/src/rule.rs
  - crates/prolly/src/tests.rs
  - crates/prolly/src/tree.rs
  - docs/design/metadata-maps.md
  - docs/research/prolly-trees.md
  - scripts/docker-linux-test/run.sh
  - scripts/prolly-bench/README.md
  - scripts/prolly-bench/bench.sh
covered_digest: "v4:sha256:2f3b37219e7c213594e33cacfb094390f0d65bc5847c32178e16c22589e019a8"
behavior_unverified: 0
overrides_applied: 0
human_verification:
  - test: "Post the text of 24-ISSUE-19-COMMENT.md as a comment on #19 (D-27: the orchestrator or the owner posts it)"
    expected: "#19 has the comment with the recommendation 'later' and the statement that file manifests are already content-cut trees"
    why_human: "ROADMAP criterion 3 says the decision is 'on #19'. On 2026-11-26 #19 is open with 0 comments. Executors and the verifier run no gh write command."
---

# Phase 24: Prolly evaluation - Verification Report

**Phase goal:** The owner has measured numbers and a written decision on prolly trees for metadata maps (#19).
**Verified:** 2026-11-26, HEAD 7fab663, branch `feature/phase-24-prolly-evaluation`
**Status:** human_needed (one item: post the #19 text)
**Re-verification:** No

The test and clippy gate result on HEAD is recorded by the orchestrator. The verifier did not run cargo. In the orchestrator's gate log, which was still running at the time of this check, the `prolly` block (23 passed) and the `prolly-bench` smoke block (2 passed) are already complete. The gate of the fix branch is `runs/gate-24-fix.log`: prolly 23 passed, smoke 2 passed, clippy ok, workspace 1,696 passed.

## Roadmap criteria

| # | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | Prototype for one map is a separate crate, not wired into the root record; nodes are chunk-store objects; tests show the node boundaries depend only on the keys | VERIFIED | `crates/prolly` depends only on `chunk-store` and `blake3`. Only `benchmarks/prolly-bench` depends on it, and no product source names it. `git diff a882251 HEAD` is empty for `crates/metadata`, `crates/chunk-store`, `crates/slicefs-cli`, `crates/slicefs-traits` and `docs/design/store-format.md`. Nodes are written with `store.put` (`tree.rs:146`) and read with `store.read` (`node.rs:208`). `rule.rs` `boundary_hash(level, key)` hashes only the key. The tests `boundaries_ignore_value_bytes`, `order_independence`, `apply_inserts_equal_bulk`, `canonical_root` and `golden_root` passed in both gate logs. |
| 2 | Boost benchmark (about 78,000 files per version) compares the current commit path with the prototype: commit time, CPU, peak memory, bytes written per commit, on-disk size after several small commits; it runs outside the test suite | VERIFIED | `runs/prolly-*` (macOS) and `runs/prolly-linux-*`: `bench_data=boost`, Boost 1_84_0 and 1_85_0, 84,104 inodes, 3 runs, 122 commits per run. Each system has 6 digest checks (flat and prolly, 3 runs), all `ok=1`. The CSV columns cover `total_ms`, `cpu_user_s`/`cpu_sys_s`, `peak_rss_bytes`, `serialized_bytes`, `new_stored_bytes`, and the disk sizes in `prolly-disk.csv`. The driver is `scripts/prolly-bench/bench.sh`. `tests/smoke.rs` uses only generated trees of 200 and 220 files. `machine.txt` gives `commit=9a26452`, and from that commit to HEAD only one README line changed in `crates`, `benchmarks` and `scripts`, so the data comes from the code at HEAD. |
| 3 | Written decision (adopt, later, drop) is in the design docs and on #19, and it states that file manifests are already content-cut trees | HUMAN (#19 part) | Design docs: VERIFIED. `docs/design/metadata-maps.md` ("Decision: **later**") and `docs/research/prolly-trees.md` (`**Recommendation: later**`, section "File manifests are already content-cut trees") both have it. #19: the text is in `24-ISSUE-19-COMMENT.md` and states the manifest fact, but it is not posted (#19 has 0 comments). Posting is an owner or orchestrator action (D-27), so it is not a gap. |

Requirements: PROLLY-01, PROLLY-02 and PROLLY-03 map to criteria 1, 2 and 3, with the same status.

## Report numbers against the CSVs

- `python3 -I runs/report-tables.py --dir runs --stem prolly --check docs/research/prolly-trees.md` gives `REPORT-OK`. The same command with `--stem prolly-linux` also gives `REPORT-OK`.
- Independent awk recomputation from the `run=median` rows matches the prose numbers:
  - scatter K=1: flat 6.6, prolly 0.6 CPU ms. The whole commit is 63.9 ms, and the estimate with prolly is 58.4 ms (91.4 %).
  - The gain where prolly helps is 3.7 to 8.3 ms (4.8 to 14.0 %) on macOS, and 3.4 to 6.5 ms (9.8 to 23.2 %) on Linux for K up to 10.
  - insdel rows on macOS and Linux: K=100 is 30.8/10.1 against 34.5/12.5 (macOS), and 16.7/13.4 against 15.4/12.7 (Linux).
  - Scale at 4M entries: 242.1 against 22.0 ms.
  - Disk growth is 64.4 against 38.4 MiB.
  - Peak RSS at the last commit is 478.2, 175.4 and 33.7 MiB, the median of the 3 runs at seq 121.
- The disk lines now count the pack index: flat and prolly have `files=2` at the start. This confirms WR-02 in the data.

## Recommendation against the rubric (plan 24-05, Task 3)

- **Not `drop` on macOS.** For K up to 100, prolly has lower CPU and lower wall time in all 9 rows. It is also better in the 1M and 4M scale rows (1M: 44.6 against 64.4 ms, 0.9 against 44.1 CPU ms). Its stored bytes are at most 0.47 x flat's.
- **Not `adopt`.** The estimated whole-commit gain is below 10 ms in every row on both systems. On macOS it is also below 20 %. Linux scatter K=1 is 23.2 %, but at 6.5 ms it is below 10 ms. `later` applies when the gain is "below 20 % or below 10 ms".
- **Linux insdel K=100.** On Linux, prolly is slower at insdel K=100 (16.7 against 15.4 ms, 13.4 against 12.7 CPU ms). The report Summary and Recommendation give this row. The Recommendation says that a Linux-only, every-row reading would give `drop`, and that plan 24-05 applied the rubric to the macOS run (the main run since the first run). This is honest. The macOS run started under a host load of 13 to 18 (on 32 CPUs), and the report says so.

## Review fixes (24-REVIEW.md "Fixes")

| Finding | Status | Evidence in code |
|---|---|---|
| WR-01 | VERIFIED | `whole.rs` has the `insdel` loop, alternating `delete_random` and `insert_freed` (`create_inode_at`, an existing metadata API). The smoke test asserts `puts == inserts` and that there is at least 1 insert and at least 1 delete. In the CSV, insdel `entries_changed` equals K. |
| WR-02 | VERIFIED | In `replay.rs`, `cs.close()` comes before each `print_disk` (lines 473/500, 578/607). |
| WR-03 | VERIFIED | `Pieces` times only the build pieces: the bulk build (line 550) and the scale build (line 694). `base.bin` is read outside the timer. |
| WR-04 | VERIFIED | `node::load_child` checks the last key, the lower bound and the count. `get`, cursor and `walk` (`tree.rs:342/401/461`) and `apply` use it. `LevelWriter::check_ascending` refuses a key that does not ascend. The test is `inconsistent_tree_is_an_error`. |
| IN-01 | VERIFIED | `REC_OVERHEAD_BYTES = 42 + 4`. The README row says 46 bytes (header and CRC). |
| IN-02 | VERIFIED | `decode(bytes, cfg)` refuses a node above `node_max`, a key above `max_key` and an entry above `max_entry`. The test is `foreign_config_node_is_an_error_in_apply`. |
| IN-03 | VERIFIED | `intern_flat` returns the stored key, and the flat check streams it back with `MapStream` (`replay.rs:495`). |
| IN-04 | VERIFIED | `batches` opens the store with `read_only: true` (`replay.rs:149`). |
| IN-05 | VERIFIED | `bench.sh:76-78` makes a relative `CARGO_TARGET_DIR` relative to the repo. No `allow(dead_code)` is left in `crates/prolly`. |

## Findings

- **WARNING (wording, fix before posting #19).** The review's Fixes section says that the report, the design section and the #19 text all state that a strict Linux reading gives `drop`. Only the report states it.
  - `24-ISSUE-19-COMMENT.md` opens with "a Linux Docker run gives the same answer" with no qualifier. It gives the Linux K=100 numbers further down.
  - `metadata-maps.md` says "the same answer, with one row that is not better" and gives the numbers.
  - Fix: add one sentence to the #19 text, for example: "One Linux row (insdel K = 100) is not better, so a Linux-only reading of the rubric would give drop". This does not change the decision.
- **INFO.** The boundary rule is size-aware, so cuts depend on the key and on the entry size, not on the value bytes. This matches PROLLY-01 ("depend only on the keys, with a size-aware rule"). `boundaries_ignore_value_bytes` changes the value bytes and keeps their length the same.
- **INFO.** The `rustc` line of `prolly-machine.txt` was written by hand. The review says so.
- No debt markers. The `XXXXXX` strings in `bench.sh` and the README are `mktemp` templates.
- Q-1 to Q-3 are open owner questions. They are listed in the report, the design doc and the #19 text. They do not block the goal, because the deliverable is the recommendation.

## Human verification required

1. **Post the #19 text.** Post `24-ISSUE-19-COMMENT.md` on #19, preferably with the one-sentence Linux qualifier above. Expected: #19 has the decision `later` and the content-cut manifest statement.

---

_Verified: 2026-11-26_
_Verifier: Claude (gsd-verifier)_
