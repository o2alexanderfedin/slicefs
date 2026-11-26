# Prolly benchmark

`bench.sh` measures the prolly prototype (`crates/prolly`) against the
metadata commit path of SliceFS (#19, PROLLY-02). For each commit of a fixed,
seeded edit script it records, for the real commit and for two map-only
modes:

- commit wall time (`begin_commit` and `finish_commit` apart for the real commit)
- CPU user and system time
- peak memory: the high-water mark of the process after the commit, and the
  peak RSS of the whole process
- bytes serialized and hashed per commit, and the new objects and new stored
  bytes in the chunk store
- the on-disk size of each store at the start and at the end

The work runs in process, on the chunk-store and metadata APIs, in the
binary `benchmarks/prolly-bench`. Nothing mounts, so the script runs on
macOS and on Linux. It runs by hand only. It is never part of `cargo test`
(`cargo test -p prolly-bench` runs only a small smoke test of the binary).

| Script | What |
|---|---|
| `bench.sh <data-dir> [runs]` | Seed the first tree once; for each run (default 3) copy the seeded store and run the edit script in the modes `whole`, `flat` and `prolly`, each step as its own process; then the synthetic `scale` rows; print one CSV row per commit and mode and one median row per (mode, pattern, k) |

## Inputs

Two Boost trees `DIR/x/boost_<v>/`: the seed tree and the next tree of
`BOOST_VERS` (default `1_84_0`, then `1_85_0`). Get them with the
mount-test script:

```sh
scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0
```

A missing data dir or tree stops the script (exit 2) with this command. The
script only reads the data dir, never writes in it (it can be read-only). It
refuses a data dir that does not exist, and it does not make one.

## Run

```sh
timeout 5400 scripts/prolly-bench/bench.sh DIR > results.csv
WORK=WORKDIR RUNS=5 KS="1 100" timeout 5400 scripts/prolly-bench/bench.sh DIR
SMOKE=1 RUNS=1 timeout 600 scripts/prolly-bench/bench.sh
```

The script has no time limit of its own on a step, so give it one with
`timeout`. On Linux the same lines work. The smoke in Linux Docker (no FUSE)
is

```sh
timeout 3600 scripts/docker-linux-test/run.sh prolly-bench-smoke
```

It mounts nothing: no FUSE device, no `SYS_ADMIN`, no `unshare`. In the
container it runs `cargo test -p prolly` in release (so the golden root and
the cut-decision table of the prototype are checked on Linux), builds
`prolly-bench` in release, and runs `bench.sh` with `SMOKE=1 RUNS=1`. A full
run in Docker needs the data dir in the container (a `-v DIR:/data:ro`
volume, then `bench.sh /data`). `SLICEFS_TARGET_VOL` names the cargo target
volume, as for the other modes of `run.sh`.

Linux smoke in Docker (plan 24-04, image `rust:1.100.0-bookworm`): the 21
prolly tests pass, `golden_root` included, so the golden root and the
cut-decision bits are the same on Linux as on macOS. The smoke gives the same
CSV shape as on macOS (30 run rows and 18 median rows, two `ok=1` checks).
The `prolly` rows have the same entries, serialized bytes, new objects and
nodes read and written as on macOS. The `whole` and `flat` byte columns
differ a little, because the file mtimes of the generated trees go into the
inode digests, and FastCDC cuts the flat maps by content. The image has no
GNU `time`, so `peak_rss_bytes` is `NA` there.

## Subcommands and modes

`bench.sh` calls these subcommands of `prolly-bench`. Each prints one line
per record: the first word is the kind, then `key=value` fields.

| Subcommand | What | Store |
|---|---|---|
| `gen tree` | `SMOKE=1` only: a tree of N files in 16 subdirectories | none |
| `seed` | Seeds the first tree with the seed code path of SliceFS (default profile and hash), once per start of the script | `seeded` |
| `whole` | The real `DictMetadataStore` commit on a copy of the seeded store: a setup commit, the edit script, then the next tree as one commit (the version step) | the copy of `seeded` |
| `batches` | Not timed. For each pair of consecutive commit roots of `whole`, the change of the `inode_data` map (sorted puts and deletes), streamed from the two map nodes | reads the copy |
| `flat` | The current design for one map: the whole `inode_data` map in RAM, interned in full per commit by a copy of `intern_u64_digest_map` (through `store_io::put_value`), then `cs.sync()` | a new store with only this map |
| `prolly` | The prototype: a bulk build from the first map, then one `prolly::apply` of each batch and one `cs.sync()` per commit | a new store with only this map |
| `scale` | Synthetic: N sorted entries, a build, then one changed middle value (`apply1`), for `flat` or `prolly` | a new store per row |

The three modes:

- `whole` is the real commit, with every map. It is the cost that a user
  sees.
- `flat` and `prolly` are the cost of one map (`inode_data`), the current way
  and the prolly way.

Why three processes and replayed real changes, and not a stats delta of
one map inside the real commit:

- The commit internals of `metadata` are private: the fields of
  `PreparedCommit` and `intern_u64_digest_map` are not `pub`. So the cost of
  one map inside the real commit cannot be timed from outside the crate,
  and this phase does not change `metadata`. `flat` is a copy of that code.
- `inode_map` holds the same entries as `inode_data` (both get the same
  `(ino, digest)` pair). The two serializations differ only by the 8-byte
  count prefix of `inode_data`, so FastCDC cuts them into mostly the same
  chunks. Inside the real commit, `inode_map` is interned first, so a stats
  delta of `inode_data` there reads near 0 and makes the current design look
  free. Each map-only mode thus has its own store with only that map.
- A `prolly` process must not hold the metadata maps, else its peak RSS
  includes them. `flat` and `prolly` replay the batch files that `batches`
  made from the real `inode_data` change of each commit, so they see the
  same entries (real digests, real creates and deletes) without opening
  `DictMetadataStore`.
- Both replays end with a `check` line: a BLAKE3 digest of the final map
  that must be the same as the digest of the real final map (`final` line
  of `batches`). `ok=1` means they match. A mismatch stops the run.

The modes run in this order in each run: `whole`, `batches`, then `flat`
and `prolly` (flat first on odd runs, prolly first on even runs), then the
`scale` rows of each n in the same mode order.

## Edit script

All from `SEED` (default 24), so the same for every run:

- Setup commit (seq 0, pattern `setup`): makes a `bench` directory for the
  created files.
- Small commits: for each pattern of `PATTERNS`, each K of `KS` and each of
  `COMMITS` commits, one commit with K edits. Edit i of commit seq has the
  kind (i + seq) mod 4: metadata change (mtime and ctime of an existing
  file), content change (new 4 KiB content), create (a new file in
  `bench`), delete (the oldest file that the script created; a create when
  there is none).
- Patterns: `scatter` draws each file at random; `cluster` draws one start
  per commit, then takes the next regular files in inode order. New inode
  numbers only grow, so creates go to the right edge of the map in both
  patterns.
- In `scatter` and `cluster` most creates and deletes cancel inside the
  commit: a delete takes the oldest file that the script created, and that
  is mostly a file of the same commit. So the `inode_data` change of these
  commits is about K / 2 value replacements plus a few entries at the right
  edge (see `batches.csv`). With fixed-width values a replacement moves no
  node boundary.
- Version step (seq after the `scatter` and `cluster` commits, pattern
  `version`): the next tree as one commit. A file with the same content key
  is left alone; a changed file gets new content; new files and directories
  are made; names that are only in the store are removed.
- `insdel` (after the version step, so the commits before it do not change):
  for each K of `KS` and each of `COMMITS` commits, one commit with K edits.
  Edit i of commit seq is a delete when (i + seq) mod 2 is 0, else an
  insert. A delete removes a random regular file of a random directory of
  the tree (not `bench`, and not a file inserted in the same commit). An
  insert makes the oldest file that an earlier commit deleted again, at the
  same inode number, name and content, with the mtime and ctime one second
  later (`create_inode_at`); when there is none, the edit is a delete. So
  each edit inserts or deletes one key in the middle of the committed map,
  and the `inode_data` change of the commit is exactly K entries. The
  product never uses a freed inode number again; the inserts stand for the
  maps whose keys can appear in the middle.

With the defaults (`KS="1 10 100 1000"`, `COMMITS=10`, three patterns) a run
has 1 + 2 x 4 x 10 + 1 + 4 x 10 = 122 commits per mode.

## Environment

| Env | Default | What |
|---|---|---|
| `WORK` | a new temp dir | `results.csv`, `scale.csv`, `disk.csv`, `edits.csv`, `batches.csv`, `shape.txt`, `machine.txt`. The stores, step outputs and the smoke data go in `WORK/stores.XXXXXX`, a new dir that the script makes with `mktemp -d` and removes at exit (also after a signal) |
| `BIN` | build it | The `prolly-bench` binary. When unset, the script runs `cargo build --release -p prolly-bench` |
| `RUNS` | `3` | Runs, when the second argument is not given |
| `SMOKE` | `0` | `1`: two small generated trees and a short edit script (see below) |
| `BOOST_VERS` | `1_84_0 1_85_0` | Exactly two versions: the seed tree, then the next tree |
| `KS` | `1 10 100 1000` | Changed files per small commit |
| `COMMITS` | `10` | Small commits per pattern and K |
| `PATTERNS` | `scatter cluster insdel` | Edit patterns (`insdel` runs after the version step) |
| `SEED` | `24` | Seed of the edit script and of the scale entries |
| `SCALE_NS` | `100000 1000000 4000000` | Entry counts of the scale rows; empty (`SCALE_NS=`) skips them |

stdout gets only the CSV of `results.csv`. Progress goes to stderr. The
script writes all output files in `WORK` new at each start, so use a `WORK`
of its own. It refuses an empty `WORK` result or `/`. It removes only its own
`WORK/stores.XXXXXX` dir and paths in it. A step that fails stops the script
with its exit code and the step name on stderr. The script needs `python3`
for the median rows and checks for it at start.

Median rows: after the runs, one row with `run=median` per group. In it,
each number is the `median_low` of that column over the run rows of the
group, so it is always a value of a real run. A column that is `NA` in every
row of the group stays `NA`. The groups are (mode, pattern, k) for
`results.csv` (with `seq=all`), (mode, n, op) for `scale.csv` and (mode,
phase) for `disk.csv`. `edits.csv` and `batches.csv` have no median rows.

## Smoke mode

`SMOKE=1` makes two trees in `WORK/stores.XXXXXX/smoke-data` with
`prolly-bench gen tree`: 2,000 files (`x/boost_0_0_1`) and 2,100 files
(`x/boost_0_0_2`, the same seed, other bytes in every tenth file, and 100
new files), each file up to 4 KiB. It refuses a data dir argument, so smoke
data never go over real data. It sets `BOOST_VERS="0_0_1 0_0_2"` and, unless
they are set, `KS="1 10"`, `COMMITS=2` and `SCALE_NS="10000 50000"`. With
`RUNS=1` it gives 42 run rows and 24 median rows in `results.csv`, 16 rows
in `scale.csv`, 12 in `disk.csv`, 14 in `edits.csv` and 13 in `batches.csv`.
It checks that the script works; its numbers say nothing about Boost
(`machine.txt` has `bench_data=smoke`).

## results.csv columns

One row per commit and mode (the line `commit` of `whole`, `flat` and
`prolly`).

| Column | Unit | What |
|---|---|---|
| `mode` | | `whole`, `flat` or `prolly` |
| `run` | | 1 to RUNS, or `median` |
| `pattern` | | `setup`, `scatter`, `cluster`, `version` or `insdel` (`whole`); `bulk` for seq 0 of `flat` and `prolly` (the first full intern or bulk build) |
| `k` | files | K of the small commit; 0 for setup, bulk and version |
| `seq` | | Commit number in the run, from 0; `all` in median rows |
| `entries_changed` | entries | `inode_data` entries put or deleted by this commit (puts + dels of the batch; for `bulk`, the entries of the first map); `NA` for seq 0 of `whole` |
| `serialized_bytes` | bytes | Bytes serialized and hashed by the commit. `whole`: the six map nodes plus the 230-byte root record. `flat`: the `inode_data` node (8 + 41 x entries). `prolly`: the bytes of the nodes written |
| `begin_ms` | ms | `whole`: `begin_commit` (it clones every map); `NA` for `flat` and `prolly` |
| `finish_ms` | ms | `whole`: `finish_commit` (interns all maps, writes the root, syncs). `flat`, `prolly`: the intern or `apply`, plus `cs.sync()` |
| `total_ms` | ms | `begin_ms + finish_ms` (`whole`); `finish_ms` (`flat`, `prolly`) |
| `cpu_user_s` | s | User CPU time of the commit (`getrusage` before and after) |
| `cpu_sys_s` | s | System CPU time of the commit |
| `maxrss_bytes` | bytes | `ru_maxrss` of the process after the commit (a high-water mark, so only a rise is a cost of this commit) |
| `new_objects` | objects | New objects in the store by this commit (`ChunkStore::stats()` delta; a deduped put adds 0) |
| `new_raw_bytes` | bytes | Bytes of the new objects before zstd |
| `new_stored_bytes` | bytes | Bytes of the new object payloads in the packs (after zstd) |
| `new_record_bytes` | bytes | `new_stored_bytes` plus 42 header bytes per new object: the bytes that the packs grow by |
| `nodes_read` | nodes | `prolly`: nodes read by `apply` (0 for the bulk build); `NA` for `whole` and `flat` |
| `nodes_written` | nodes | `prolly`: nodes written; `NA` for `whole` and `flat` |
| `peak_rss_bytes` | bytes | Peak RSS of the whole step process (`/usr/bin/time -l` on macOS, `-v` with GNU time); the same for every row of a step; `NA` without one of them |

## scale.csv columns

`mode,run,n,op,ms,cpu_user_s,cpu_sys_s,maxrss_bytes,serialized_bytes,new_objects,new_stored_bytes,peak_rss_bytes`

| Column | Unit | What |
|---|---|---|
| `mode` | | `flat` or `prolly` |
| `run` | | 1 to RUNS, or `median` |
| `n` | entries | Synthetic entries (one of `SCALE_NS`) |
| `op` | | `build` (the full map: a full intern or a bulk build) or `apply1` (one changed middle value: a full intern again, or one `apply`) |
| `ms` | ms | Wall time of the op, `cs.sync()` included |
| `cpu_user_s` | s | User CPU time of the op |
| `cpu_sys_s` | s | System CPU time of the op |
| `maxrss_bytes` | bytes | `ru_maxrss` after the op |
| `serialized_bytes` | bytes | Bytes serialized and hashed by the op |
| `new_objects` | objects | New objects in the store by the op |
| `new_stored_bytes` | bytes | New stored bytes by the op |
| `peak_rss_bytes` | bytes | Peak RSS of the process (both ops of one n and mode run in one process) |

## disk.csv columns

`mode,run,phase,apparent_bytes,alloc_bytes,files`

| Column | Unit | What |
|---|---|---|
| `mode` | | `whole`, `flat` or `prolly` |
| `run` | | 1 to RUNS, or `median` |
| `phase` | | `start` (`whole`: the seeded store before the setup commit; `flat`, `prolly`: after the first intern or build) or `end` (after the last commit) |
| `apparent_bytes` | bytes | Sum of the file sizes of the store dir |
| `alloc_bytes` | bytes | Allocated blocks of those files x 512 |
| `files` | files | Files in the store dir |

## edits.csv, batches.csv, shape.txt and machine.txt

`edits.csv` has one row per commit of `whole`:

| Column | What |
|---|---|
| `run`, `pattern`, `k`, `seq` | As in `results.csv` |
| `meta` | Metadata changes (mtime and ctime) in the commit |
| `content` | Content changes (new 4 KiB content) |
| `create` | Files created (`insdel`: files inserted again at a freed inode number) |
| `delete` | Files deleted |

`batches.csv` has one row per commit after the setup commit:

| Column | What |
|---|---|
| `run`, `seq`, `pattern`, `k` | As in `results.csv` |
| `puts` | `inode_data` entries put (new or changed value) |
| `dels` | `inode_data` entries deleted |
| `inserts` | The puts of keys that the old map did not have |

`shape.txt` has the lines `seed` (prefix `run=0`) and `info`, `version`,
`final`, `check`, `hash` and `tree` (prefix `run=<r>`) as the binary prints
them:

| Line | What |
|---|---|
| `seed` | Seed time and inode count |
| `info` | `inode_data` entries, the inode count of the root record, the next inode number, the bytes of the six map nodes, `ru_maxrss` after the store load |
| `version` | Changes of the version step: files changed, created and deleted, directories created and deleted |
| `final` | Entries and BLAKE3 digest of the real final `inode_data` |
| `check` | Entries and digest of the final map of `flat` or `prolly`, and `ok=1` when it equals `final` |
| `hash` | Cost of the boundary hash per key, in ns (over the keys of the first map) |
| `tree` | Shape of the final prolly tree: depth, nodes, leaves, leaf bytes (min, mean, max), internal nodes and their mean bytes |

`machine.txt` has `key=value` lines and no path, host name or user name:

| Key | What |
|---|---|
| `os`, `arch` | `uname -sr`, `uname -m` |
| `cpu`, `cpus`, `mem_bytes` | CPU model, CPU count, RAM |
| `fs` | File system of `WORK` |
| `rustc` | `rustc -V` |
| `commit` | Short git commit of the repo, `unknown` when git cannot read it |
| `page_cache` | `warm (not dropped)` |
| `load_before`, `load_after` | Load average at start and end |
| `start_utc`, `end_utc` | Start and end time (UTC) |
| `bench_data` | `smoke` (`SMOKE=1`, generated trees) or `boost` (a data dir) |
| `runs`, `ks`, `commits`, `patterns`, `seed`, `scale_ns`, `boost_vers` | The run parameters |

## Notes

- Serialized bytes are not new stored bytes. Serialized bytes are hashed
  and cut on each commit: O(map size) for `whole` and `flat`, O(changes x
  depth) for `prolly`. New stored bytes are small in both designs: FastCDC
  already stores only the changed chunks of a flat map (D-24). Read the two
  columns apart.
- `begin_ms` of `whole` is the cost of cloning every map in `begin_commit`.
- The disk growth of `whole` includes the 4 KiB content of the content
  edits and creates. The `flat` and `prolly` stores hold only the map.
- All sizes are before GC: no GC runs, so dead nodes stay in the packs.
- The page cache is warm: the script reads both trees once before the runs
  and does not drop the cache.
- `peak_rss_bytes` is `NA` where there is no `/usr/bin/time` with `-l` or
  GNU `-v` (for example the Linux Docker image).
- The `scale` rows are synthetic entries, not Boost.
- The prototype has no node cache (D-17): every `apply` reads its paths
  from the store.
- The target map is `inode_data` only. It is one of the four maps with one
  entry per inode or file (`inode_map`, `inode_data`, `dir_data`,
  `manifest_data`) and about a third of the serialized bytes per commit. So
  a map-only gain of `prolly` over `flat` is not the gain of the whole
  commit.
- Memory of the script is bounded: it holds no data in RAM. The bench
  processes stream the trees and the map nodes in pieces (D-02); `flat`
  holds its map, as the current design does.
- Run time of a full Boost run: plan 24-05 measures it.
