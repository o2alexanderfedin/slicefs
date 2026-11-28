# Store header and format policy

This document gives the store header, the feature flags, the messages and
the format policy of a SliceFS store. The format is not frozen yet: it can
still change before a freeze, which is a later decision.

The code is `crates/slicefs-cli/src/header.rs`.

## Header file

- The header is the file `<store>/FORMAT` at the store root.
- It is text, one item on each line, and each line ends with `\n`. SliceFS
  writes only ASCII. A file that is not UTF-8 text is refused.
- It is at most 64 KiB (`MAX_HEADER`). A larger file is refused.
- SliceFS writes it durably: it writes a temp file of its own
  (`FORMAT.tmp.<pid>.<n>`), syncs it, puts it in place as `FORMAT`, then
  syncs the store directory. For a new store, the temp file becomes
  `FORMAT` with a hard link, which never replaces a `FORMAT` that a
  parallel create wrote first; that create then uses the existing header.
  On a file system with no hard links, the create falls back to a rename
  of the temp file to `FORMAT` (only when `FORMAT` does not exist just
  before). That rename can replace a `FORMAT` that a parallel create wrote
  at the same moment.
  A rewrite renames the temp file over `FORMAT`. A crash can leave a temp
  file; SliceFS does not read it.
- For a new store, the header is written before `packs/` and `segments/`
  exist. Thus a crash cannot leave data with no header. With `seed --hash`,
  the first header has that hash, and SliceFS prints no hash-change line.
  With `seed --chunk-profile MIN/AVG/MAX` (sizes in KiB) and a profile that
  is not 4/8/32 KiB, the first header also has the `chunks` line and the
  flag line `ro_compat chunk-profile <release>`. The default profile writes
  neither line. With `seed --hash sha256`, the first header has the flag
  line `incompat hash-sha256 <release>` right after the hash line.
  With `seed --boundary-salt random` (32 bytes from `/dev/urandom`) or
  `seed --boundary-salt <64 hex digits>`, the first header also has the
  `boundary-salt` line and the flag line `ro_compat boundary-salt <release>`
  (after the `chunks` lines). Without the option a new store has neither
  line.
- A rewrite (`--hash` on `mount` or `seed`) changes only the hash line. All
  other lines stay, in their order, also the `chunks` line, the flag lines
  and the lines that this SliceFS does not know. If there is no hash line, the new hash line
  becomes line 2. A rewrite to `sha256` also adds the flag line
  `incompat hash-sha256 <release>` at the end, in the same header write,
  when the header does not have it.

The header of a new store (format 8, no flags):

```
slicefs-store 8 (store header with feature flags, prolly metadata maps)
hash blake3-256
```

The header of a new store with the chunk profile 16/64/256 KiB
(`seed --chunk-profile 16/64/256`; `<release>` is the value of
`CHUNK_PROFILE_RELEASE`):

```
slicefs-store 8 (store header with feature flags, prolly metadata maps)
hash blake3-256
chunks 16384 65536 262144
ro_compat chunk-profile <release>
```

The header of a new store with the write hash SHA-256
(`seed --hash sha256`; the release word is the value of
`HASH_SHA256_RELEASE`, now `v2.3`):

```
slicefs-store 8 (store header with feature flags, prolly metadata maps)
hash sha256
incompat hash-sha256 v2.3
```

The header of a new store with a boundary salt
(`seed --boundary-salt random`; the digits are an example, and the release
word is the value of `BOUNDARY_SALT_RELEASE`, now `v2.4`):

```
slicefs-store 8 (store header with feature flags, prolly metadata maps)
hash blake3-256
boundary-salt 00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff
ro_compat boundary-salt v2.4
```

The salt is an optional per-store key of the boundary hash of the metadata
trees (the boundary rule of the trees). The default is no salt, so equal
metadata gives equal tree nodes in every store. The salt is fixed when the
store is made: SliceFS has no command to change it, and an existing store
refuses any other salt (M18). The salt is not a secret: it is plain text in
`FORMAT`.

A header with one flag of each class (the names are examples, and
`<release>` is the place of a release name):

```
slicefs-store 8 (store header with feature flags, prolly metadata maps)
hash blake3-256
compat example-feature <release>
ro_compat example-read-feature <release>
incompat example-new-feature <release>
```

## Grammar and parse order

Lines are split into words at each space.

| Line | Form | Rules |
|------|------|-------|
| First line | `slicefs-store <number> [free text]` | `<number>` is decimal digits. The free text is optional and SliceFS does not read it. |
| Flag line | `<class> <name> <release>` | Exactly three words. `<class>` is `compat`, `ro_compat` or `incompat`. |
| Hash line | `hash <name>` | Exactly two words. At most one hash line. No hash line means `blake3-256`. |
| Chunks line | `chunks <min> <avg> <max>` | The FastCDC chunk profile of the store. Exactly four words. The sizes are in bytes, as decimal digits. They are powers of two with min < avg < max, max at most 1 MiB (1048576, the SliceFS limit), at least the fastcdc v2020 minimums (min 64 B, avg 256 B, max 1 KiB), and FastCDC must cut a run of zeros at max. At most one chunks line. It always comes with the ro_compat flag `chunk-profile`, and the flag always comes with the line. No chunks line means 4/8/32 KiB. |
| Boundary-salt line | `boundary-salt <salt>` | The key of the boundary hash of the metadata trees. Exactly two words. `<salt>` is exactly 64 digits of `0-9` and `a-f` (lowercase) and stands for 32 bytes. At most one salt line. It always comes with the ro_compat flag `boundary-salt`, and the flag always comes with the line. No salt line means no salt. Any other line that starts with the word `boundary-salt` is an error (M9), not an other line. |
| Other line | any other text | An unknown setting (see the parse order). |

- `<name>`: 1 to 64 characters; lowercase letters `a-z`, digits `0-9` and
  `-`; it starts and ends with a letter or a digit; no `--` (kebab-case).
  A name occurs at most once in the header.
- `<release>`: 1 to 32 printable ASCII characters, with no space. It is the
  first SliceFS release that knows the flag. SliceFS shows it in messages
  and in `stats`. SliceFS never compares it.
- An empty line, or a `hash` line that does not have exactly two words, is
  an other line.
- The hash line is a store setting, not a flag. A hash name that this
  SliceFS does not know is refused.
- The chunks line is a store setting too. It comes with its flag, by the
  rule below. SliceFS writes it only for a profile that is not 4/8/32 KiB;
  a chunks line of 4/8/32 KiB with the flag is accepted.
- The boundary-salt line is a store setting too. It comes with its flag, by
  the rule below. SliceFS writes it only when `seed --boundary-salt` makes
  the store.

Parse order:

1. Read the number of the first line. If the first line does not have the
   form `slicefs-store <number>`, stop (M4). If the number is not 8, stop
   (M2 for a number from 0 to 6, M17 for 7, M3 for a larger number). A store of another
   number can have another header layout, so SliceFS reads no more lines.
2. Read all other lines. A bad flag line, a name that occurs two times, two
   hash lines, a bad chunks line or two chunks lines stop the parse (M9).
   These errors occur also when a flag is unknown. Then a chunks line with
   no `chunk-profile` flag, a `chunk-profile` flag with no chunks line, or
   a `chunk-profile` flag that is not ro_compat stops the parse (M9). A
   `hash sha256` line with no `hash-sha256` flag, or a `hash-sha256` flag
   that is not incompat, also stops the parse (M9). A `hash-sha256` flag
   with no `hash sha256` line is valid: the store can still hold SHA-256
   objects after the write hash went back. A line that starts with
   `boundary-salt` and is not two words with a 64-digit lowercase hex
   value, or a second `boundary-salt` line, stops the parse (M9). Then a
   `boundary-salt` line with no `boundary-salt` flag, a `boundary-salt` flag
   with no `boundary-salt` line, or a `boundary-salt` flag that is not
   ro_compat stops the parse (M9).
3. Find the unknown flags: the flags whose name is not in `KNOWN_FLAGS`.
   The strongest unknown flag decides:
   - an unknown incompat flag: refuse (M5);
   - else an unknown ro_compat flag: read-only;
   - else an unknown compat flag: go on, read and write.
   When a flag is unknown, other lines are allowed: they belong to the
   unknown feature.
4. If no flag is unknown, an other line is an error (M9, `unknown setting`).

Rule for later phases: a new setting line always comes with a flag. Thus an
older SliceFS never finds an unknown line without an unknown flag.

## Flag classes

What a SliceFS does when the header has a flag that it does not know (a
known flag changes nothing in this table):

| Class | Decision | Write commands (seed, gc, rehash, snapshot create, snapshot switch) | Read commands (stats, scrub, snapshot list) | mount |
|-------|----------|---------------------------------------------------------------------|----------------------------------------------|-------|
| compat | go on | run | run | read-write mount |
| ro_compat | read-only | refused (M6w) | run | read-only mount (M6), see the read-only mode |
| incompat | refuse | refused (M5) | refused (M5) | refused (M5) |

A SliceFS that knows the flag (the name is in its `KNOWN_FLAGS`) opens the
store for read and write. A SliceFS of store format 7 or older refuses a
format 8 store, because the first line has another number.

If the header has more than one unknown incompat flag, the message has one
M5 text for each of them. If it has more than one unknown ro_compat flag,
the messages name the first of them in the file.

Comparison with ext4. ext4 has three feature masks in its superblock, with
these meanings (from the kernel ext4 superblock documentation):

- compat: the kernel can still read and write the file system;
- ro_compat: the kernel can still mount it read-only;
- incompat: the kernel should stop.

The SliceFS classes use the same meanings. The differences:

- A SliceFS flag is a named text line with a release. Thus a message can
  name the flag and the release. ext4 has only bit numbers.
- SliceFS also has a format number, for a change of the header layout
  itself.
- A read-only mount of SliceFS never replays the write log. If the store
  has a write log to replay, or a `mount.lock`, the read-only mount is
  refused (M7, M8).

## Flag list

Store format 8 has three flags: `KNOWN_FLAGS` is `chunk-profile`,
`hash-sha256` and `boundary-salt`.

| Name | Class | Release | What it means |
|------|-------|---------|---------------|
| chunk-profile | ro_compat | `CHUNK_PROFILE_RELEASE` | The store has a FastCDC chunk profile that is not 4/8/32 KiB, in its `chunks` line. An older SliceFS can read the store: a reader takes each chunk length from the manifest entries, and its zero shortcut matches only zero chunks of its own max length (it reads all other chunks). It must not write the store: it would cut new data with 4/8/32 KiB, so a store would have two profiles, and a spliced file would not have the key of a new write of the same bytes. |
| hash-sha256 | incompat | `HASH_SHA256_RELEASE` | The store has, or had, objects with SHA-256 IDs (tag 0x12). An older SliceFS cannot verify or read these objects, so it must not open the store. SliceFS writes the flag before the first SHA-256 object: in the first header of `seed --hash sha256`, in the same header write as the hash line of `mount --hash sha256`, and alone before `rehash --to sha256` writes an object. The flag stays when the write hash goes back to `blake3-256`, because SHA-256 objects can still be live. Removing it after a GC that proves no SHA-256 object is left is deferred. |
| boundary-salt | ro_compat | `BOUNDARY_SALT_RELEASE` | The store has a boundary salt, in its `boundary-salt` line. An older SliceFS can read the metadata trees, because a reader follows the node keys and does not cut. It must not write the store: it would cut the trees with another rule (no salt), so equal data would get different tree nodes, and the trees of the store would mix two rules. Same reason as `chunk-profile`. |

The release words are the constants `CHUNK_PROFILE_RELEASE`,
`HASH_SHA256_RELEASE` and `BOUNDARY_SALT_RELEASE` in `header.rs`. The first
two are `v2.3`, the first release that writes these flags. The last is
`v2.4`.

Rule to add a flag:

1. Choose the class by what an older SliceFS can do safely with a store
   that uses the feature:
   - it can read and write the store: `compat`;
   - it can only read the store: `ro_compat`;
   - it cannot read the store: `incompat`.
2. Choose a short kebab-case name (see the grammar).
3. The release is the first release that knows the flag.
4. Add the name to `KNOWN_FLAGS` in `header.rs`.
5. Write the flag line when the store first uses the feature.
6. Add a row to the table above.

## Read-only mode

A mount of a store with an unknown ro_compat flag is read-only:

- The mount has the FUSE `RO` option.
- It makes no `mount.lock` and no write-log segment.
- It holds a shared `flock` on the store directory for the life of the
  mount (the store lock, see below). This writes nothing to the store.
- It opens the chunk store and the index read-only.
- It does no write-log replay, and it starts no flusher, no online GC, no
  segment GC and no commit timer.
- It does no commit at unmount.
- It prints M6 on stdout. In place of the chunk store line of a
  read-write mount, it prints `Chunk store: read-only.` (it runs no GC and
  does no hole punch probe). The `Workers`, `Allocator`, `Compression of
  new data` and `SliceFS mounted at` lines are as for every mount. There
  is no `Replayed` line, because there is no replay.

The mount is refused when (SliceFS does the checks in this order):

- `--chunk-profile` names another profile than the store has (M16).
- `--hash` is given (M11).
- `--auto-snapshot` is given (M12).
- The store lock is held exclusive: a writable mount or a command that
  needs no live mount runs now (M13).
- `mount.lock` exists: the last mount did not end cleanly, or a mount runs
  now (M8).
- The write log has records to replay (M7).

The daemon has no EROFS check for each request. With `RO`, the kernel stops
the write requests. A write that reached the daemon would fail in the
read-only chunk store. A metadata change would stay in RAM and is not
committed.

Commands that read: `stats`, `scrub`, `snapshot list`, and `mount` in this
read-only mode. Commands that write: `seed`, `gc`, `rehash`,
`snapshot create`, `snapshot switch`.

### Store lock

A read-only mount makes no `mount.lock`, so the commands that look for
`mount.lock` do not see it. The store lock is how they see it:

- The lock is a `flock` on the store directory (not on `FORMAT`: a header
  rewrite replaces the `FORMAT` file, and a lock on the old file then
  protects nothing).
- A read-only mount takes it shared, with a short retry (250 ms), and holds it until the
  mount ends. The order is: the header check reads `FORMAT`, then the
  `--chunk-profile` check (M16), the `--hash` and `--auto-snapshot`
  checks (M11, M12), then the store lock
  (M13), then the `mount.lock` check (M8), the write-log check (M7) and the
  read-only open of the chunk store and the index. Thus the lock comes
  after the read of `FORMAT`, and before every other read of the store.
- The commands that need no live mount (`gc` with no `mount.lock`,
  `seed`, `rehash`, `snapshot create`, `snapshot switch`) take it
  exclusive, with a short retry (250 ms), after their `mount.lock` check
  (`seed`: first, before the header check; `rehash`: after the check
  that the store exists and that the target hash is known, before the
  header check), and hold it while they run (`rehash`: to its end, its GC
  included). If it is held, they are refused (M14).
- `rehash` keeps its temp files (the old-to-new map and the refcount sort
  files) in `<store>/rehash-tmp/`. It removes the directory at its end,
  and also on an error that is not a crash. The map of `rehash --dry-run`
  goes to the system temp dir (`TMPDIR`), not to the store.
  Every writable open of the store removes what a crash left there.
- A power loss during `rehash` is tested by `scripts/power-loss/` (see
  its README). The store is on LazyFS, which drops all unsynced data on
  `clear-cache`. The rehash process is killed just before each of its
  sync-type calls (`fsync`, `fdatasync`, `rename`), and at its end, and the
  unsynced data is then dropped. After each crash the store must open with
  the old roots or the new roots, scrub must be clean, the files must be
  equal to the source, and a rerun must finish the job. The test does not
  model a lost rename or a missing directory sync (LazyFS makes renames
  durable at once), torn writes, or a crash during the rerun.
- `stats` shows `Mounted` as `yes` when `mount.lock` exists or the store
  lock is held, also while an offline `gc` runs. (It takes the lock
  exclusive and drops it at once, so a mount, `gc` or snapshot command
  that starts at the same moment can be refused, M13, M14 or M15.)
- A writable mount takes it exclusive, with a short retry (250 ms), before it looks at
  `mount.lock`, and holds it for the life of the daemon: it is released
  last, after the final commit, the segment clean-up and the removal of
  `mount.lock`. If it is held, the mount is refused (M15) and `mount.lock`
  is not touched (it belongs to the holder). With the lock held, an existing
  `mount.lock` is a crash leftover, and the dirty-mount replay runs. Thus
  two writable mounts of one store cannot run at the same time (#201).
  `slicefs unmount` returns only after the daemon has exited (with
  `--store`: when the store lock is free), at most 30 s; so a mount or a
  command right after it is not refused (M15, M14).
- On a file system with no `flock`, there is no lock, and the commands run
  as with no read-only mount.

Rule for later releases: only a SliceFS that knows the ro_compat flag can
write the store. Before it writes a store that a read-only mount can use,
each of its commands that writes takes the store lock exclusive (or
refuses), also a read-write mount, `seed` and a header rewrite. In this
release, `gc`, `seed`, `rehash`, `snapshot create`, `snapshot switch` and a
read-write mount take it.

This mode is only for an unknown ro_compat flag. A snapshot mount of a
store with no unknown flag also has `RO`, but it still opens the store to
write.

## Messages

`{store}` is the store path as given on the command line.

| Id | When | Text |
|----|------|------|
| M1 | The store has data in packs/ or segments/, but no FORMAT file. | `{store} has data but no store header (the file FORMAT is missing). This SliceFS does not open a store with no header.` |
| M2 | The format number is 0 to 6. | `{store} was made by an older SliceFS ({what}). The store format changed without a migration: copy the files out with the older version, then into a new store` |
| M3 | The format number is larger than 8. | `{store} was made by a newer SliceFS (store format {n}). This SliceFS reads store format {known} only.` |
| M4 | The first line does not have the form slicefs-store and a number. | `{store}: unknown store format {line:?}` |
| M5 | An unknown incompat flag, for any command. | `{store} uses the feature {name}, which needs SliceFS {release} or later. This SliceFS cannot open it.` |
| M6 | An unknown ro_compat flag: the mount is read-only (printed on stdout). | `Mounting read-only: {store} uses the feature {name}, which needs SliceFS {release} or later to write.` |
| M6w | An unknown ro_compat flag, for a command that writes. | `{store} uses the feature {name}, which needs SliceFS {release} or later to write. This SliceFS can only read it.` |
| M7 | A read-only mount, and the write log has records to replay. | `{store} has {n} request(s) in its write log. They must be replayed, but this SliceFS can only mount the store read-only (feature {name} needs SliceFS {release} or later). Mount it once with SliceFS {release} or later.` |
| M8 | A read-only mount, and mount.lock exists. | `{store}/mount.lock exists: the last mount did not end cleanly, or a mount runs now. This SliceFS can only mount the store read-only (feature {name} needs SliceFS {release} or later) and cannot replay the write log.` |
| M9 | A line of the header is bad. | `{store}/FORMAT line {k}: {reason}: {line:?}` |
| M10 | FORMAT is larger than 64 KiB. | `{store}/FORMAT is larger than 64 KiB; it is not a SliceFS store header.` |
| M11 | A read-only mount with the option --hash. | `{store} can only be mounted read-only (feature {name} needs SliceFS {release} or later to write), so --hash cannot change its write hash.` |
| M12 | A read-only mount with the option --auto-snapshot. | `{store} can only be mounted read-only (feature {name} needs SliceFS {release} or later to write), so --auto-snapshot cannot make a snapshot at unmount.` |
| M13 | A read-only mount while a writable mount, gc, seed, rehash, snapshot create or snapshot switch holds the store lock. | `{store} is in use: it is mounted to write, or a command that writes it (gc, seed, rehash, snapshot create or snapshot switch) runs now. Mount it when that mount or command ends.` |
| M14 | gc (offline), seed, rehash, snapshot create or snapshot switch while a mount or another of these commands holds the store lock. | `{store} is in use: it is mounted, or another command (gc, seed, rehash, snapshot create or snapshot switch) runs now. Unmount it or let that command end first.` |
| M15 | A writable mount while another mount, gc, seed, rehash, snapshot create or snapshot switch holds the store lock. | `{store} is in use: it is mounted, or a command (gc, seed, rehash, snapshot create or snapshot switch) runs now. Unmount it or let that command end first.` |
| M16 | `seed` or `mount` with `--chunk-profile` that differs from the profile of the store (also a read-only mount). Nothing in the store changes. | `{store} has the chunk profile {have}; --chunk-profile {want} is another profile. The chunk profile of a store is fixed when the store is made.` |
| M17 | The format number is 7 (a store of release v2.3.0). | `{store} was made by SliceFS v2.3.0 (store format 7, flat metadata maps). This SliceFS reads store format 8 only. Copy the files into a new store with the steps in docs/user/upgrade.md.` |
| M18 | `seed` with `--boundary-salt` on an existing store whose salt differs (also a store with no salt, and always for `random`). Nothing in the store changes. | `{store} has {have}; --boundary-salt {want} is another salt. The boundary salt of a store is fixed when the store is made.` |
| M19 | A mount (writable, read-only or of a snapshot) of a format 8 store whose root record fields are not prolly trees (a store of an interim build with flat maps). Nothing in the store changes (a mount with `--hash` records that hash in FORMAT before the store loads, as for any store). | `{store} was made by a development build of SliceFS (store format 8 with flat metadata maps). This SliceFS reads store format 8 with prolly metadata maps only. Copy the files into a new store with the steps in docs/user/upgrade.md.` |

The values in the texts:

- `{name}` and `{release}`: the words of the flag line.
- `{n}` in M3: the format number of the store. `{known}` in M3: 8, the
  format number of this SliceFS. `{n}` in M7: the number of records in the
  write log.
- `{have}` in M16: the profile of the store; `{want}`: the profile of the
  option. Each is printed as `<min>/<avg>/<max> KiB` (in bytes, with `B`,
  when a size is not whole KiB).
- `{have}` in M18: `a boundary salt` or `no boundary salt`; `{want}`: the
  option as given (`random`, or the 64 hex digits).
- `{k}` in M9: the line number (the first line is 1). `{line:?}` in M4 and
  M9: the line, in double quotes.
- `{reason}` in M9 is one of:
  - `bad feature flag line`
  - `the feature {name} is listed twice`
  - `two hash lines`
  - `bad chunks line`
  - `two chunks lines`
  - `chunks line with no chunk-profile flag`
  - `chunk-profile flag with no chunks line`
  - `the feature chunk-profile must be ro_compat`
  - `bad boundary-salt line`
  - `two boundary-salt lines`
  - `boundary-salt line with no boundary-salt flag`
  - `boundary-salt flag with no boundary-salt line`
  - `the feature boundary-salt must be ro_compat`
  - `hash sha256 with no hash-sha256 flag`
  - `the feature hash-sha256 must be incompat`
  - `unknown setting`
- `{what}` in M2 is one of:
  - `metadata in vt0/` (format 0 to 4)
  - `224-bit object IDs` (format 5)
  - `store format 6, no feature flags` (format 6)

Other texts:

- FORMAT is not UTF-8 text:
  `{store}/FORMAT is not UTF-8 text; it is not a SliceFS store header.`
- `gc`, `rehash`, `snapshot create` or `snapshot switch` on a path with no
  `segments/` directory (the command makes nothing there; the text is in
  `content.rs`):
  `failed to load segments: store not found at {store}: no segments/ directory`

## Stats

`slicefs stats` shows four lines after the line `Mounted`:

```
Store format     : 8
Chunk profile    : 4/8/32 KiB
Feature flags    : none
Access           : read-write
```

- `Store format`: the format number, or `none (no store header yet)` for an
  empty store with no header. The read writes nothing.
- `Chunk profile`: the FastCDC min/avg/max of the store, from the
  `chunks` line, or 4/8/32 KiB when there is no chunks line (also for an
  empty store with no header).
- `Feature flags`: `none`, or one line for each flag in the form
  `<class> <name> (needs SliceFS <release>; known here)` or
  `... unknown here)`. The order is compat, then ro_compat, then incompat.
  The first line has the label; the next lines start with 19 spaces.
- `Access`: `read-write`, or `read-only` for a store with an unknown
  ro_compat flag. (A store with an unknown incompat flag is refused, M5.)

With `--json`, the same data is in four fields:

```
"format": 8,
"flags": {"compat": [], "ro_compat": [], "incompat": []},
"chunk_profile": {"min": 4096, "avg": 8192, "max": 32768},
"access": "read-write"
```

- `format`: the number, or `null` for an empty store with no header.
- `flags`: three lists. Each entry is
  `{"name": "<name>", "release": "<release>", "known": false}`.
- `chunk_profile`: the FastCDC min, avg and max chunk sizes of the store,
  in bytes.
- `access`: `read-write` or `read-only`.

## Metadata maps (store format 8)

This section is the normative definition of the metadata maps of store
format 8: the layout, the node format, the boundary rule and its constants.
The code is `crates/prolly`. The numbers below come from the run of plan
25-07: `.planning/phases/25-prolly-format-and-layout/runs/layout/decision.txt`
and the CSV files in the same folder. The last line of that file is
`verdict layout=a l=8192 node_max=16384`.

### Design target

- One store holds 1,000,000,000 inodes (files and directories) or more. It is
  a part of a global, decentralized file system.
- No operation loads a whole map into RAM. This holds for mount, stats, GC
  and scrub too. Memory stays bounded at any store size: one path of nodes
  per tree, never all entries.
- Counts come from the counts in the internal entries or from the root
  record (`inode_count`, `logical_bytes`). They never come from a walk of
  all leaves.

### Layout

The layout is **a: five maps, five prolly trees.** The maps are
`inode_data`, `dir_data`, `manifest_data`, `xattr_data` and `refcounts`.
Each map key of the root record points to the root of its own tree.

Key grammar and value encodings of layout a (`||` joins bytes; the code
is `crates/metadata/src/inode.rs`, `dirent.rs`, `xattr.rs` and `store.rs`):

| Map | Key | Value |
| --- | --- | --- |
| `inode_data` | `ino`, 8 bytes, big endian | the inode record, inline, no tag byte (see below) |
| `dir_data` | `ino` of the directory, 8 bytes, big endian, `\|\|` the entry name bytes | child `ino` (8 bytes, little endian) `\|\|` type (1 byte, see below) |
| `manifest_data` | `ino`, 8 bytes, big endian | the 33-byte `ObjectKey` of the file's manifest |
| `xattr_data` | `ino`, 8 bytes, big endian, `\|\|` the xattr name bytes | tag (1 byte) `\|\|` the inline value or the 33-byte `ObjectKey` of a spilled value (see below) |
| `refcounts` | the 33-byte content key | count, `u64`, 8 bytes, little endian |

All keys of one map compare as bytes, so the entries of one directory (or
the xattrs of one inode) are one key range: `ino BE .. (ino + 1) BE`.

**`inode_data` value.** The inode record of `serialize_inode`, little
endian, 56 or 72 bytes; the length tells the two forms apart:

| Bytes | Field |
| --- | --- |
| 0..8 | `ino`, `u64` |
| 8..12 | `mode`, `u32` |
| 12..16 | `uid`, `u32` |
| 16..20 | `gid`, `u32` |
| 20..24 | `nlinks`, `u32` |
| 24..32 | `size`, `u64` |
| 32..40 | `mtime_sec`, `i64` |
| 40..44 | `mtime_nsec`, `u32` |
| 44..52 | `ctime_sec`, `i64` |
| 52..56 | `ctime_nsec`, `u32` |
| 56..60 | `rdev`, `u32` (72-byte form only) |
| 60..68 | `atime_sec`, `i64` (72-byte form only) |
| 68..72 | `atime_nsec`, `u32` (72-byte form only) |

The 72-byte form is written only when `rdev` or the atime is not zero. No
inode is stored as an object of its own.

**`dir_data` entries.** One entry per name. The name is UTF-8, 1 to 255
bytes, so a key is at most 263 bytes. The value is 9 bytes: the child `ino`,
then the type (1 byte, `(mode & S_IFMT) >> 12`, the Linux `DT_*` value: 4 a
directory, 8 a regular file, 10 a symlink; 0 when the child had no inode
when the name was linked). A value of another length is corrupt.

- `.` (the directory itself) and `..` (its parent; the root's `..` is 1)
  are entries of every directory, so an empty directory has two entries.
  "Is a directory" is one read of `ino || "."`.
- A hard link is one more entry: another name with the same child `ino`.
- Readdir is a range scan of the directory prefix, in name byte order (the
  flat maps of format 7 listed in name hash order).
- The names of a directory decide where the node boundaries of its range
  fall, so a user who picks the names of one directory can move them. The
  cost is bounded by `NODE_MAX` and the forced cuts; the boundary salt
  (below) makes the boundaries hard to guess.

**`xattr_data` entries.** One entry per xattr name; no xattr set object
exists. The name is UTF-8, 1 to 255 bytes (Linux `XATTR_NAME_MAX`). The
value is a tag byte, then:

- tag 0 then the value bytes, when the leaf entry (4 + key + 1 + value
  bytes, that is 4 + 8 + name + 1 + value) fits `MAX_ENTRY` (2,048);
- tag 1 then the 33-byte `ObjectKey` of the value, else. The value is
  stored like file content (FastCDC chunks and a manifest) and read by
  range.

The tag depends only on the name and value lengths, so equal xattrs give
equal trees. listxattr is a range scan of the inode prefix, in name byte
order. A spilled value is metadata: the GC mark reaches it through the
xattr tree, and it has no `refcounts` row.

The decision came from three layouts, tested on the same edit scripts:

- **a:** five trees, as above.
- **b:** one namespace tree, keys `0x01 || ino || kind` (kind 0 inode, 1 dir,
  2 manifest, 3 xattr) and `0x02 || content key` for refcounts.
- **c:** the same namespace tree, but the refcounts in their own tree.

Directory entries are stored by name (owner decision of Phase 26, issue
#215): one entry per name, in place of one list object per directory. The
longest key is 263 bytes, below `MAX_KEY` (1,024).

Refcounts are a prolly map like the others (D-04). Issue #165 may remove the
map later.

The rules were fixed before the run (`prereg.txt`, `decide.py`) and the run
did not change them. Rows: the Boost trees 1_84_0 and 1_85_0, and synthetic trees of
1,000,000 and 10,000,000 files; 10 commits per row, one process per row, in
Docker on disk-backed storage (`scripts/prolly-bench/layout.sh`; the rules are
in `scripts/prolly-bench/decide.py`). Layout b and c were measured against a
at the Dolt config (L 4,096, NODE_MAX 16,384):

| Rule | Limit | Layout b | Layout c |
| --- | --- | --- | --- |
| Mean ratio of nodes read and written per commit, 6 small rows | at most 0.80 | 0.681 pass | 0.700 pass |
| Mean ratio of CPU per commit, 6 small rows | at most 1.25 | 0.825 pass | 0.948 pass |
| Largest ratio of bytes written, 6 large rows | at most 1.10 | 0.912 pass | 0.906 pass |
| Nodes at 1,000,000,000 inodes, not more than a (47,291,816) | at most 47,291,816 | 48,922,745 fail | 48,428,987 fail |

Neither b nor c qualified, so the layout is a. b and c touch about 30% fewer
nodes for a small commit, but at 1,000,000,000 inodes they need 3.4% (b) and
2.4% (c) more nodes than five separate trees. The rule says "not more", and
the rule decided.

Two limits of the data. The Boost refcounts are empty, because the product
writes no refcounts today. So the refcount signal, and the comparison of b
against c, come from the synthetic rows only (2 refcount keys per file). The
CPU columns are noisy: the host had a high load during the run. They only
guard; the node and byte counts decided.

Effect on the GC mark: one walk per tree, five walks in all. Each tree has one
value type, so there is no dispatch on a kind byte. Memory stays one path of
nodes per tree.

### Root record

The root record of this build has 197 bytes (little endian, fixed width):

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 8 | `ROOT_DIR_INO` |
| 8 | 8 | `NEXT_INO` |
| 16 | 33 | `INODE_DATA` |
| 49 | 33 | `DIR_DATA` |
| 82 | 33 | `MANIFEST_DATA` |
| 115 | 33 | `XATTR_DATA` |
| 148 | 33 | `REFCOUNTS` |
| 181 | 8 | `LOGICAL_BYTES` |
| 189 | 8 | `INODE_COUNT` |

There is one inode map. The duplicate `inode_map` of earlier builds is gone
(D-06); `NEXT_INO` stays in the record. Each of the five 33-byte fields is the
`ObjectId` of the root node of its prolly tree. The empty tree is the empty
leaf, so every field always has a root. (Layout b would have had 65 bytes and
layout c 98 bytes.)

At load, the root object of each of the five fields must start with the
node magic `SPT1` (one read per field). A record of another size, or a field
that is not a tree root, is refused. A store of an interim build of format 8
had flat maps in these fields; a mount refuses it with M19 and writes
nothing to its segments or packs. There is no read path for flat maps.

### Node format

A node is one chunk-store object. A child pointer is the child's `ObjectId`
(33 bytes), and the root of a tree is one `ObjectId`. Nodes are not cut again
by FastCDC.

Header, 10 bytes:

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 4 | magic `SPT1` |
| 4 | 1 | rule version `u8` (1) |
| 5 | 1 | level `u8` (0 for a leaf) |
| 6 | 4 | entry count `u32`, little endian |

Leaf entry (level 0): key length `u16` LE, value length `u16` LE, key, value.

Internal entry (level 1 and up): key length `u16` LE, the last key of the
child, the child `ObjectId` (33 bytes), the number of leaf entries under the
child `u64` LE.

Keys are strictly ascending in every node. A reader checks:

- the magic, and the rule version (a node of another version is refused);
- the node size, up to 32,768 bytes (`MAX_NODE_MAX`); a writer of this
  format makes nodes up to `NODE_MAX` (16,384 bytes);
- the key and entry limits (`MAX_KEY`, `MAX_ENTRY`) and the order of the keys;
- the count of entries against the bytes of the node (no bytes left over,
  an internal node has at least one entry);
- each child against the parent entry: its last key, its first key above the
  last key of the previous sibling, and its leaf entry count.

A node that fails a check is damaged data, never a panic.

### Boundary rule

The rule decides where a node ends. It is the size-aware key splitter of Dolt
with a Weibull CDF of shape 4, in integer math only. The same input gives the
same cut points on every platform.

After an entry that grows a node in progress from `before` to `after` bytes
(the node size counts the header), in this order:

1. An internal node never ends after its first entry.
2. If `after > NODE_MAX - MAX_ENTRY`, the node ends (the forced cut).
3. If `after < NODE_MIN`, the node does not end.
4. Else the node ends if and only if `h < T(before, after)`.

`h` is the boundary hash, a 32-bit number. `T` is the threshold:

`T = 2^32 - E(delta)`, and
`delta = floor(((after^4 - before^4) * 2^32) / L^4)`, in 128-bit integers.

`E(delta)` is `e^-x * 2^32` for `x = delta / 2^32`:

- `i = delta >> 32`. If `i >= 23`, `E` is 0.
- `j` = the top 10 bits of the low 32 bits of `delta`, and `r` = the low 22
  bits.
- `frac = F[j] - (((F[j] - F[j+1]) * r) >> 22)`.
- `E = (I[i] * frac) >> 32`, with the product in 128 bits.

The tables are `I[i] = round(2^32 * e^-i)` for i = 0 to 22 and
`F[j] = round(2^32 * e^(-j/1024))` for j = 0 to 1024 (rounded half up). They
are in `crates/prolly/src/rule_table.rs`. The script
`scripts/prolly-table/gen_table.py` makes them with 60 decimal digits, and
`gen_table.py --check` compares the file with its output. The BLAKE3 hash of
the table bytes (`I` then `F`, each value as `u64` little endian) is
`6a72017ef6d3d7daf2ea9385afe01ac20fd4bf85610a5179c9bc7384bcc369e4`. The test
`golden_root` pins this hash.

The error of `T` against the real formula `1 - e^-((after^4 - before^4) /
L^4)` is at most 2^-22 (about 2.4e-7); the test `fixed_point_matches_float`
checks it. The measured maximum is 1.2e-7. In 1,000,000 random cut decisions
the integer rule and the float rule differ in none.

The boundary hash is the first 4 bytes, little endian, of BLAKE3 over the byte
`level + 1` and then the key. The level byte makes every level cut on its own
bits. The hash never reads the value, so the boundaries depend on the keys and
on the entry lengths only. With a salt, BLAKE3 runs in keyed mode (see
"Boundary salt").

The rule has a version byte, in every node header. This text defines rule
version 1. A change of the table, the formula or the constants is a new rule
version.

### Constants

| Constant | Value |
| --- | --- |
| `WEIBULL_L` (L) | 8,192 |
| `WEIBULL_K` | 4 (the code writes `x*x*x*x`) |
| `NODE_MAX` | 16,384 bytes |
| `NODE_MIN` | 512 bytes |
| `MAX_ENTRY` | 2,048 bytes |
| `MAX_KEY` | 1,024 bytes |
| `NODE_HEADER` | 10 bytes |
| `MAX_NODE_MAX` (largest node a reader accepts) | 32,768 bytes |
| `RULE_VERSION` | 1 |

The mean node is about 0.906 x L, about 7.4 KB. The forced cut is at
`NODE_MAX - MAX_ENTRY` = 14,336 bytes.

Why L and `NODE_MAX` were chosen. Each node is one object in the chunk index,
and the index keeps the full ID of each object. So the number of nodes at
1,000,000,000 inodes is a real cost. The rule of plan 25-07 compared three
configs on the layout a trees:

| Config (L / NODE_MAX) | Nodes at 1B | Ratio to Dolt | Bytes written, mean of 3 rows | Ratio | CPU, mean of 3 rows | Ratio | Forced cuts / nodes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 4,096 / 16,384 (Dolt) | 47,291,816 | 1 | 1.27 MB | 1 | 41.5 ms | 1 | 0.00000 |
| 8,192 / 16,384 | 23,479,915 | 0.4965 | 2.14 MB | 1.683 | 68.0 ms | 1.639 | 0.00011 |
| 8,192 / 32,768 | 23,479,815 | 0.4965 | 2.14 MB | 1.683 | 66.8 ms | 1.610 | 0.00000 |

A config replaces Dolt if its forced cuts are at most 0.5% of the nodes, its
nodes at 1B are at most 0.65 of Dolt's, its bytes at most 2.5 times and its CPU
at most 2.0 times. Both 8,192 configs qualify. Their node counts differ by 100
nodes, so the smaller `NODE_MAX` wins: 16,384. The price is about 68% more
bytes and about 64% more CPU for a small commit (an absolute 2.14 MB and 68
ms); the gain is half the nodes.

`NODE_MIN` is the Dolt value. `MAX_ENTRY`, `MAX_KEY` and the forced cut at
`NODE_MAX - MAX_ENTRY` are SliceFS limits: Dolt cuts after the entry that
passes its maximum, and SliceFS never lets a node pass `NODE_MAX`. A key with
a name suffix (265 bytes) fits under `MAX_KEY`, and an internal entry of the
longest key is below `MAX_ENTRY`.

The constants decide every node ID. After the format freeze, a change is a new
rule version and a copy migration of every store.

### Boundary salt

A store may have a boundary salt: the header line `boundary-salt` and the
ro_compat flag of the same name (see "Header file" and "Flag list"). The salt
is the key of keyed BLAKE3 in the boundary hash. It enters nothing else:
stored keys do not change.

- The default is no salt. Then the same metadata gives the same node IDs in
  every store.
- A reader needs no salt, because it follows the keys of the nodes and does
  not cut. A writer must use the salt of the store, so a build that does not
  know the flag must not write (the flag is ro_compat).
- The salt of a store is fixed when the store is made. A rebuild with
  another salt is a later command (D-11).

Accepted risk. In an unsalted store, a user who can pick names or keys can
search for keys with a low boundary hash (grinding) and make many nodes of
about `NODE_MIN` bytes, so the tree has more nodes than it should. The harm
has a bound: no node is above `NODE_MAX`, no node is below `NODE_MIN` except
the last of a level, and the tree never breaks. The salt stops a search made ahead
of time for all stores. It does not stop a user who can read the `FORMAT` file
of the store.

### Scale at 1,000,000,000 inodes

Method (D-05). A 10,000,000-file base tree of each map was built and measured.
For each tree: entries per leaf `e` = entries / leaves, and fanout `f` =
leaves / nodes of level 1. The entries are scaled to 1,000,000,000 inodes, the
leaves are `ceil(entries / e)`, and each level above is `ceil(previous / f)`
until one node is left. The nodes are the sum of the levels, and the depth is
the number of levels. No tree of 100,000,000 or 1,000,000,000 entries was
built.

Result for layout a at L 8,192 and `NODE_MAX` 16,384 (the `extrapolation`
lines of `decision.txt`):

| Map | Entries | Depth | Levels, leaves to root | Nodes |
| --- | --- | --- | --- | --- |
| `inode_data` | 1,000,000,000 | 5 | 6,048,100 / 40,600 / 273 / 2 / 1 | 6,088,976 |
| `dir_data` | 100,036,700 | 4 | 604,900 / 4,400 / 33 / 1 | 609,334 |
| `manifest_data` | 899,963,300 | 5 | 5,448,000 / 37,600 / 260 / 2 / 1 | 5,485,863 |
| `xattr_data` | 49,818,700 | 4 | 302,300 / 2,100 / 15 / 1 | 304,416 |
| `refcounts` | 1,799,926,600 | 5 | 10,880,200 / 110,000 / 1,113 / 12 / 1 | 10,991,326 |
| Total | | | | 23,479,915 |

The entry counts follow the mix of the synthetic base tree (2 refcount keys
per file). The table was measured with the key grammar of Phase 25 (one
`dir_data` entry per directory, 33-byte values); with the final grammar
above (one entry per name, inline inode records) the node counts differ.
They were not measured again. The refcount line is from synthetic data
only. At the Dolt config the same method gives 47,291,816 nodes.

One change at 10,000,000 files touches 52 nodes (median of 10 commits, nodes
read and written; Dolt config: 63).

### This build and the prolly maps

This build keeps the five maps in the prolly trees of this section
(`crates/prolly`, used through `crates/metadata/src/pmap.rs`) in the commit,
read, GC and rehash paths. The pending edits of each map are in an overlay
in RAM until a flush or a commit applies them to the tree; reads go through
a node cache with a byte cap (`--meta-cache-size`, default 64 MiB). The
protocol is in [Metadata maps](metadata-maps.md).

An interim build before it wrote format 8 with flat maps. No release had
that form, and this build refuses it (M19).

## Format policy

- The format number changes when an older SliceFS cannot parse the
  header safely, or when the metadata layout changes as a whole.
- Any other change of the store adds a flag, by the rule of the flag list.
- The format number is separate from the release number.
- A store of another format number is refused. There is no migration.
- The store has other version numbers inside it: packs and the chunk index
  have version 2, the index MANIFEST (`SFMF`) has version 1, and the runs
  (`SFRN`) have version 2. They are part of the store format. Thus a change
  of one of them needs a flag or a new format number, by this policy.
- Store format 8 is the format of the prolly metadata maps (issue #215). The
  owner chose a new format number over an incompat flag on format 7 in the
  Phase 25 discussion (2026-11-26), because every metadata map changes. The
  release v2.3.0 refuses format 8 at the first line (M3 of that release),
  and this release refuses format 7 with M17, which points to
  `docs/user/upgrade.md`. The number stays separate from the release number
  (the flag lines carry the release words).
- The format is not frozen yet.
