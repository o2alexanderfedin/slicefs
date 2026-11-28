# Prolly benchmark

`bench.sh` measures the metadata commit path of SliceFS (#19, PROLLY-02,
PMAP-07) and compares a flat map with a prolly tree on synthetic maps. For
each commit of a fixed, seeded edit script it records, for the real commit
(`whole`):

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
| `bench.sh <data-dir> [runs]` | Seed the first tree once; for each run (default 3) copy the seeded store and run the edit script in the mode `whole`, as its own process; then the synthetic `scale` rows; print one CSV row per commit and one median row per (mode, pattern, k) |
| `layout.sh` | The synthetic `synth` rows of Phase 25 (see the last section) |
| `pmap07.sh prepare-v230 <dir>` | PMAP-07: make the detached worktree of tag `v2.3.0` and copy the commit bench and the flat report module into it |
| `pmap07.sh commit <src-dir> <label> <n> <samples> <out-file>` | PMAP-07 checks A and B: the ignored test `one_change_commit_at_scale` of `<src-dir>` in Docker, release build, store on the volume `slicefs-pmap07-work` |
| `pmap07.sh boost <bin-src-dir> <label> <data-dir> <out-dir>` | PMAP-07 check C: `bench.sh` of this tree in Docker with `BIN` built from `<bin-src-dir>`, with the settings of the rule (`SMOKE=1` and `-` as data dir: a smoke) |
| `pmap07.py` | The PMAP-07 decision script: reads the commit logs and the two `results.csv`, prints checks A, B and C and `verdict pmap07=<pass\|fail>` (`--self-test`, `--summary`) |

`pmap07.sh` refuses to start while another container named `pmap07-*` runs,
so two decision runs never overlap. The rule of PMAP-07 (builds, sizes,
samples, limits and the re-run policy) is in
`.planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-rule.md`.
It and `pmap07.py` were committed before any measurement and do not change.

**Retired modes.** The modes `batches`, `flat`, `prolly`, `maps` and `layout`
read the flat metadata maps of a product store. Since Phase 26 the maps are
prolly trees, so these modes are gone (store formats have no backward
compatibility). Tag `v2.3.0` keeps them, for re-runs on flat stores. `bench.sh`
takes a `BIN` of any build that has `seed`, `whole` and `scale`: the `v2.3.0`
binary gives the flat baseline of PMAP-07 and the binary of a branch gives the
tree rows.

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

Linux smoke in Docker (plan 24-04, image `rust:1.100.0-bookworm`): the
prolly tests pass, `golden_root` included, so the golden root and the
cut-decision bits are the same on Linux as on macOS. The image has no GNU
`time`, so `peak_rss_bytes` is `NA` there. (The numbers of the smoke of plan
24-04 were taken before the retired modes were removed.)

## Subcommands and modes

`bench.sh` and `layout.sh` call these subcommands of `prolly-bench`. Each prints one line
per record: the first word is the kind, then `key=value` fields.

| Subcommand | What | Store |
|---|---|---|
| `gen tree` | `SMOKE=1` only: a tree of N files in 16 subdirectories | none |
| `seed` | Seeds the first tree with the seed code path of SliceFS (default profile and hash), once per start of the script | `seeded` |
| `whole` | The real `DictMetadataStore` commit on a copy of the seeded store: a setup commit, the `scatter` and `cluster` commits, the next tree as one commit (the version step), then the `insdel` commits | the copy of `seeded` |
| `scale` | Synthetic: N sorted entries, a build, then one changed middle value (`apply1`), for `flat` or `prolly` | a new store per row |

`whole` is the real commit, with every map. It is the cost that a user sees.
`scale` is the cost of one synthetic map, the flat way and the prolly way.

`whole` ends each run with the shape lines of `shape.txt`. The modes run in
this order in each run: `whole`, then the `scale` rows of each n (flat first
on odd runs, prolly first on even runs).

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
  edge. With fixed-width values a replacement moves no node boundary.
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
| `WORK` | a new temp dir | `results.csv`, `scale.csv`, `disk.csv`, `edits.csv`, `shape.txt`, `machine.txt`. The stores, step outputs and the smoke data go in `WORK/stores.XXXXXX`, a new dir that the script makes with `mktemp -d` and removes at exit (also after a signal) |
| `BIN` | build it | The `prolly-bench` binary. When unset, the script runs `cargo build --release -p prolly-bench` in the repo and takes the binary from `CARGO_TARGET_DIR` (a relative `CARGO_TARGET_DIR` is relative to the repo, as for cargo) or `target` |
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
phase) for `disk.csv`. `edits.csv` has no median rows.

## Smoke mode

`SMOKE=1` makes two trees in `WORK/stores.XXXXXX/smoke-data` with
`prolly-bench gen tree`: 2,000 files (`x/boost_0_0_1`) and 2,100 files
(`x/boost_0_0_2`, the same seed, other bytes in every tenth file, and 100
new files), each file up to 4 KiB. It refuses a data dir argument, so smoke
data never go over real data. It sets `BOOST_VERS="0_0_1 0_0_2"` and, unless
they are set, `KS="1 10"`, `COMMITS=2` and `SCALE_NS="10000 50000"`. With
`RUNS=1` it gives 14 run rows and 8 median rows in `results.csv`, 16 rows
in `scale.csv`, 4 in `disk.csv` and 14 in `edits.csv`. It checks that the
script works; its numbers say nothing about Boost (`machine.txt` has
`bench_data=smoke`).

`LAYOUT_SMOKE=1 layout.sh` makes a small synthetic matrix (18 rows, 180 rows
in `layout.csv`, 18 `check` lines).

## results.csv columns

One row per commit (the line `commit` of `whole`).

| Column | Unit | What |
|---|---|---|
| `mode` | | `whole` |
| `run` | | 1 to RUNS, or `median` |
| `pattern` | | `setup`, `scatter`, `cluster`, `version` or `insdel` |
| `k` | files | K of the small commit; 0 for setup, bulk and version |
| `seq` | | Commit number in the run, from 0; `all` in median rows |
| `entries_changed` | entries | `NA` (the batch files that gave it are retired) |
| `serialized_bytes` | bytes | Bytes serialized and hashed by the commit |
| `begin_ms` | ms | `begin_commit` (it clones every map) |
| `finish_ms` | ms | `finish_commit` (interns all maps, writes the root, syncs) |
| `total_ms` | ms | `begin_ms + finish_ms` |
| `cpu_user_s` | s | User CPU time of the commit (`getrusage` before and after) |
| `cpu_sys_s` | s | System CPU time of the commit |
| `maxrss_bytes` | bytes | `ru_maxrss` of the process after the commit (a high-water mark, so only a rise is a cost of this commit) |
| `new_objects` | objects | New objects in the store by this commit (`ChunkStore::stats()` delta; a deduped put adds 0) |
| `new_raw_bytes` | bytes | Bytes of the new objects before zstd |
| `new_stored_bytes` | bytes | Bytes of the new object payloads in the packs (after zstd) |
| `new_record_bytes` | bytes | `new_stored_bytes` plus 46 bytes per new object (the 42-byte record header and the 4-byte record CRC): the bytes that the packs grow by |
| `nodes_read` | nodes | `NA` |
| `nodes_written` | nodes | `NA` |
| `peak_rss_bytes` | bytes | Peak RSS of the whole step process (`/usr/bin/time -l` on macOS, `-v` with GNU time); the same for every row of a step; `NA` without one of them |

## scale.csv columns

`mode,run,n,op,ms,cpu_user_s,cpu_sys_s,maxrss_bytes,serialized_bytes,new_objects,new_stored_bytes,peak_rss_bytes`

| Column | Unit | What |
|---|---|---|
| `mode` | | `flat` or `prolly` |
| `run` | | 1 to RUNS, or `median` |
| `n` | entries | Synthetic entries (one of `SCALE_NS`) |
| `op` | | `build` (the full map: a full intern or a bulk build) or `apply1` (one changed middle value: a full intern again, or one `apply`) |
| `ms` | ms | Wall time of the op, `cs.sync()` included. The values are made outside the timer in both modes: `flat` makes its map first, and the `prolly` build makes them in pieces of 16,384 entries and times only the pushes, `finish` and the sync |
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
| `mode` | | `whole` |
| `run` | | 1 to RUNS, or `median` |
| `phase` | | `start` (the seeded store before the setup commit) or `end` (after the last commit and a close of the store). A close seals the active pack and writes its index, so every size includes the pack index |
| `apparent_bytes` | bytes | Sum of the file sizes of the store dir |
| `alloc_bytes` | bytes | Allocated blocks of those files x 512 |
| `files` | files | Files in the store dir |

## edits.csv, shape.txt and machine.txt

`edits.csv` has one row per commit of `whole`:

| Column | What |
|---|---|
| `run`, `pattern`, `k`, `seq` | As in `results.csv` |
| `meta` | Metadata changes (mtime and ctime) in the commit |
| `content` | Content changes (new 4 KiB content) |
| `create` | Files created (`insdel`: files inserted again at a freed inode number) |
| `delete` | Files deleted |

`shape.txt` has the lines `seed` (prefix `run=0`) and `info` and `version`
(prefix `run=<r>`) as the binary prints them:

| Line | What |
|---|---|
| `seed` | Seed time and inode count |
| `info` | `inode_data` entries, the inode count of the root record, the next inode number, the bytes of the six map nodes, `ru_maxrss` after the store load |
| `version` | Changes of the version step: files changed, created and deleted, directories created and deleted |

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
  and cut on each commit: O(map size) for a flat map, O(changes x depth) for
  a prolly tree. New stored bytes are small in both designs: FastCDC already
  stores only the changed chunks of a flat map (D-24). Read the two columns
  apart.
- `begin_ms` of `whole` is the cost of cloning every map in `begin_commit`.
- The disk growth of `whole` includes the 4 KiB content of the content
  edits and creates.
- All sizes are before GC: no GC runs, so dead nodes stay in the packs.
- The page cache is warm: the script reads both trees once before the runs
  and does not drop the cache.
- `peak_rss_bytes` is `NA` where there is no `/usr/bin/time` with `-l` or
  GNU `-v` (for example the Linux Docker image).
- The `scale` rows are synthetic entries, not Boost.
- The prototype has no node cache (D-17): every `apply` of a `scale` row
  reads its paths from the store.
- Memory of the script is bounded: it holds no data in RAM. The bench
  processes stream the trees in pieces (D-02); `scale --mode flat` holds its
  map, as the flat design does.
- Run time of a full Boost run: plan 24-05 measures it.

## Layout and constants benchmark (Phase 25)

Phase 25 asks two questions of the prolly metadata format: how to lay the five
metadata maps out in trees (Q-2), and which constants to use (L and NODE_MAX).
`layout.sh` runs the whole matrix; `decide.py` holds the rules that read it.
The Boost rows of Phase 25 (real commits of two Boost versions) ran with the
modes `maps` and `layout`. These modes are retired (see the top of this file);
tag `v2.3.0` keeps them. `layout.sh` now runs the synthetic rows only.

**Layouts** (`layout.rs`). The key of a per-inode record is the inode number;
the key of a refcount record is the 33-byte content key.

- `a`: five trees, one per map (inode, dir, manifest, xattr, refcounts), each
  keyed by its native key.
- `b`: one tree. Key of a per-inode record: `0x01`, the inode (8 bytes big
  endian), a kind byte (0 inode, 1 dir, 2 manifest, 3 xattr), so the records
  of one file share a leaf. Key of a refcount record: `0x02`, then the content
  key.
- `c`: the tree of `b` for the four per-inode maps, and a separate tree for
  refcounts (refcounts have another key space and lifetime).

**Configs.** The weight of the boundary rule `L` and `NODE_MAX`, as
`L:NODE_MAX`: `4096:16384` (the Dolt values, the config of the layout
decision), `8192:16384` and `8192:32768`.

**Mode** of `prolly-bench` (it prints `key=value` lines with
`mode=layout source=.. n=.. layout=.. l=.. node_max=..`):

| Mode | What |
|---|---|
| `synth --n N --layout a\|b\|c --store P [--seed S] [--l L] [--node-max M] [--commits C] [--ks "100 1000"] [--patterns "one mixed insdel"]` | The same for a synthetic source: N inodes (about 90% files with a manifest entry and 2 refcount keys each, about 10% directories, about 5% with an xattr entry), made from sorted streams on the fly (never a whole map in RAM), and the D-02 commit mix: `one` (k=1), `mixed` (k changed files) and `insdel` (k/2 creates at the right edge, k/2 deletes). The check covers the entry count of each map and every changed, inserted, created and deleted key |

**The runner.** `layout.sh` runs one `synth` process per size in `SYNTH_NS`,
layout and config. Each row process has its own store, a time limit
(`ROW_TIMEOUT`) and its output in
`rows/<source>-<n>-<layout>-<L>-<NODE_MAX>.txt`, ending with a line
`rc=<code>`. With `RESUME=1` a finished row is not run again. `LAYOUT_SMOKE=1`
runs a small matrix in minutes. It takes no data dir. Env: `WORK`, `BIN`,
`CONFIGS`, `LAYOUTS`, `SYNTH_NS`, `SYNTH_KS`, `COMMITS`, `SEED`,
`ROW_TIMEOUT`, `RESUME`, `LAYOUT_SMOKE` (the defaults are in the header of the
script). The default of `--l` of `synth` is 8192, the product value (Phase 25
verdict).

**Output files** (in `WORK`, no path, host or user):

| File | Columns |
|---|---|
| `layout.csv` | `source,n,layout,l,node_max,pattern,k,seq,edits,ms,cpu_ms,maxrss_bytes,new_objects,new_stored_bytes,nodes_read,nodes_written,bytes_written,forced_cuts`: one row per commit; `cpu_ms` is 1000 x (user + system CPU seconds) |
| `shape.csv` | `source,n,layout,l,node_max,phase,map,entries,depth,levels,leaf_bytes,internal_bytes,internal_entries,forced_cuts`: one row per tree and phase; `levels` is the node count per level from the leaves up, joined by `/` |
| `build.csv` | `source,n,layout,l,node_max,ms,cpu_ms,maxrss_bytes,nodes_written,bytes_written,forced_cuts`: the base build of each row (the time of a synthetic build includes making its streams) |
| `checks.txt` | the `check` lines; every one must be `ok=1` |
| `machine.txt` | machine and run parameters |

**Docker.** `OUT=<result dir> scripts/docker-linux-test/run.sh prolly-layout`
builds the bench in release and runs the runner in a container. `OUT`
receives the files above and `rows/`, and the stores live on the disk-backed
named volume `slicefs-prolly-work` (no tmpfs), removed at the end unless
`RESUME=1`. It mounts nothing and needs no FUSE device.

**The decision script.** `python3 scripts/prolly-bench/decide.py [--expect-ns N1,N2] [--expect-ks K1,K2] DIR`
reads `layout.csv`, `shape.csv` and `checks.txt` of `DIR` and prints the rows it
used, one `rule` line per threshold, the 1B extrapolation of each tree, and a
last line `verdict layout=<a|b|c> l=<L> node_max=<M>`. It exits 2 when a
required row is missing or a check is not `ok=1`. `--self-test` checks the
rules on small fixtures. The rules (the thresholds, the rows, the 1B
extrapolation) are written in its docstring and are fixed before the real run:
they are not changed after the data is seen.

`decide.py` also asks for the Boost rows, so it reads only the data of the
Phase 25 run; a new run of `layout.sh` has the synthetic rows only.

**Boost refcounts are empty** (in the Phase 25 Boost rows). The product never
writes refcounts today, so the refcount map of every Boost store has no entry. In the Boost rows the
refcounts tree of layouts a and c is empty, and layout b has no `0x02` keys. So
the Boost rows say nothing about refcounts in a layout, and the signal about
layout b (refcounts inside) against layout c (refcounts apart) comes only from
the synthetic rows, which carry 2 refcount keys per file.
