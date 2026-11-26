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
  neither line.
- A rewrite (`--hash` on `mount` or `seed`) changes only the hash line. All
  other lines stay, in their order, also the `chunks` line, the flag lines
  and the lines that this SliceFS does not know. If there is no hash line, the new hash line
  becomes line 2.

The header of a new store (format 7, no flags):

```
slicefs-store 7 (store header with feature flags)
hash blake3-256
```

The header of a new store with the chunk profile 16/64/256 KiB
(`seed --chunk-profile 16/64/256`; `<release>` is the value of
`CHUNK_PROFILE_RELEASE`):

```
slicefs-store 7 (store header with feature flags)
hash blake3-256
chunks 16384 65536 262144
ro_compat chunk-profile <release>
```

A header with one flag of each class (the names are examples, and
`<release>` is the place of a release name):

```
slicefs-store 7 (store header with feature flags)
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
| Chunks line | `chunks <min> <avg> <max>` | The FastCDC chunk profile of the store. Exactly four words. The sizes are in bytes, as decimal digits. They are powers of two with min < avg < max, inside the limits of fastcdc v2020 (min 64 B to 1 MiB, avg 256 B to 4 MiB, max 1 KiB to 16 MiB), and FastCDC must cut a run of zeros at max. At most one chunks line. It always comes with the ro_compat flag `chunk-profile`, and the flag always comes with the line. No chunks line means 4/8/32 KiB. |
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

Parse order:

1. Read the number of the first line. If the first line does not have the
   form `slicefs-store <number>`, stop (M4). If the number is not 7, stop
   (M2 for a smaller number, M3 for a larger number). A store of another
   number can have another header layout, so SliceFS reads no more lines.
2. Read all other lines. A bad flag line, a name that occurs two times, two
   hash lines, a bad chunks line or two chunks lines stop the parse (M9).
   These errors occur also when a flag is unknown. Then a chunks line with
   no `chunk-profile` flag, a `chunk-profile` flag with no chunks line, or
   a `chunk-profile` flag that is not ro_compat stops the parse (M9).
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

| Class | Decision | Write commands (seed, gc, snapshot create, snapshot switch) | Read commands (stats, scrub, snapshot list) | mount |
|-------|----------|-------------------------------------------------------------|----------------------------------------------|-------|
| compat | go on | run | run | read-write mount |
| ro_compat | read-only | refused (M6w) | run | read-only mount (M6), see the read-only mode |
| incompat | refuse | refused (M5) | refused (M5) | refused (M5) |

A SliceFS that knows the flag (the name is in its `KNOWN_FLAGS`) opens the
store for read and write. A SliceFS of store format 6 or older refuses a
format 7 store, because the first line has another number.

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

Store format 7 has one flag: `KNOWN_FLAGS` is `chunk-profile`.

| Name | Class | Release | What it means |
|------|-------|---------|---------------|
| chunk-profile | ro_compat | `CHUNK_PROFILE_RELEASE` | The store has a FastCDC chunk profile that is not 4/8/32 KiB, in its `chunks` line. An older SliceFS can read the store: a reader takes each chunk length from the manifest entries, and its zero shortcut matches only zero chunks of its own max length (it reads all other chunks). It must not write the store: it would cut new data with 4/8/32 KiB, so a store would have two profiles, and a spliced file would not have the key of a new write of the same bytes. |

The release word is the constant `CHUNK_PROFILE_RELEASE` in `header.rs`. It
is a placeholder (now `v2.2`) until the release is named.

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
read-only mode. Commands that write: `seed`, `gc`, `snapshot create`,
`snapshot switch`.

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
  `seed`, `snapshot create`, `snapshot switch`) take it exclusive, with a short retry (250 ms),
  after their `mount.lock` check (`seed`: first, before the header
  check), and hold it while they run. If it is held, they are refused
  (M14).
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
release, `gc`, `seed`, `snapshot create`, `snapshot switch` and a
read-write mount take it.

This mode is only for an unknown ro_compat flag. A snapshot mount of a
store with no unknown flag also has `RO`, but it still opens the store to
write.

## Messages

`{store}` is the store path as given on the command line.

| Id | When | Text |
|----|------|------|
| M1 | The store has data in packs/ or segments/, but no FORMAT file. | `{store} has data but no store header (the file FORMAT is missing). This SliceFS does not open a store with no header.` |
| M2 | The format number is smaller than 7. | `{store} was made by an older SliceFS ({what}). The store format changed without a migration: copy the files out with the older version, then into a new store` |
| M3 | The format number is larger than 7. | `{store} was made by a newer SliceFS (store format {n}). This SliceFS reads store format {known} only.` |
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
| M13 | A read-only mount while a writable mount, gc, seed, snapshot create or snapshot switch holds the store lock. | `{store} is in use: it is mounted to write, or a command that writes it (gc, seed, snapshot create or snapshot switch) runs now. Mount it when that mount or command ends.` |
| M14 | gc (offline), seed, snapshot create or snapshot switch while a mount or another of these commands holds the store lock. | `{store} is in use: it is mounted, or another command (gc, seed, snapshot create or snapshot switch) runs now. Unmount it or let that command end first.` |
| M15 | A writable mount while another mount, gc, seed, snapshot create or snapshot switch holds the store lock. | `{store} is in use: it is mounted, or a command (gc, seed, snapshot create or snapshot switch) runs now. Unmount it or let that command end first.` |
| M16 | `seed` or `mount` with `--chunk-profile` that differs from the profile of the store (also a read-only mount). Nothing in the store changes. | `{store} has the chunk profile {have}; --chunk-profile {want} is another profile. The chunk profile of a store is fixed when the store is made.` |

The values in the texts:

- `{name}` and `{release}`: the words of the flag line.
- `{n}` in M3: the format number of the store. `{known}` in M3: 7, the
  format number of this SliceFS. `{n}` in M7: the number of records in the
  write log.
- `{have}` in M16: the profile of the store; `{want}`: the profile of the
  option. Each is printed as `<min>/<avg>/<max> KiB` (in bytes, with `B`,
  when a size is not whole KiB).
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
  - `unknown setting`
- `{what}` in M2 is one of:
  - `metadata in vt0/` (format 0 to 4)
  - `224-bit object IDs` (format 5)
  - `store format 6, no feature flags` (format 6)

Other texts:

- FORMAT is not UTF-8 text:
  `{store}/FORMAT is not UTF-8 text; it is not a SliceFS store header.`
- `gc`, `snapshot create` or `snapshot switch` on a path with no
  `segments/` directory (the command makes nothing there; the text is in
  `content.rs`):
  `failed to load segments: store not found at {store}: no segments/ directory`

## Stats

`slicefs stats` shows four lines after the line `Mounted`:

```
Store format     : 7
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
"format": 7,
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

## Format policy

- The format number changes only when an older SliceFS cannot parse the
  header safely.
- Any other change of the store adds a flag, by the rule of the flag list.
- The format number is separate from the release number.
- A store of another format number is refused. There is no migration.
- The store has other version numbers inside it: packs and the chunk index
  have version 2, the index MANIFEST (`SFMF`) has version 1, and the runs
  (`SFRN`) have version 2. They are part of the store format. Thus a change
  of one of them needs a flag or a new format number, by this policy.
- The format is not frozen yet.
