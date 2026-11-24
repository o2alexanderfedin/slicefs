# Phase 21.1: deferred items

What Phase 21.1 leaves open. Each item is outside the scope of the phase or
was found and not fixed.

## Open

1. **Snapshot mounts of a normal store still write** ([Snapshots]). A
   snapshot mount of a store with no unknown flag has FUSE `RO`, but it
   still takes `mount.lock`, opens a write-log segment, opens the store to
   write, runs the segment GC and the commit timer, and commits in
   `destroy`. With the `Access` wiring of this phase the fix is small: use
   `set_read_only` and the read-only load (`load_store_read_only`) for a
   snapshot mount too.
2. **A public `--read-only` mount option** for a store with no unknown
   flag. The read-only daemon mode exists; only the option is missing.
3. **The format freeze.** A later decision of the owner. The header layout
   (the first line and the line grammar) is the part that an older SliceFS
   must always read.
4. **The flags of Phase 22 (chunk profile) and Phase 23 (write hash).**
   They follow the rule in `docs/design/store-format.md`: a new setting
   line always comes with a flag. Phase 23 also decides whether a new hash
   adds a flag (the `hash` line is a setting now).
5. **No EROFS check for each request in the daemon.** The FUSE `RO` option
   stops write requests in the kernel. A write that reached the daemon
   fails in the read-only chunk store; a metadata change stays in RAM and
   is not committed.
6. **The ext4 journal statement of RESEARCH A1** (ext4 replays a journal on
   a read-only mount unless the device is read-only or `noload` is given)
   is not verified. The doc does not use it: it states only the three cited
   ext4 meanings and what SliceFS does. Doc wording only.
7. **`slicefs unmount --store` removes `mount.lock` with no header check**
   (`clean_mount_lock` in `crates/slicefs-cli/src/unmount.rs`). A
   read-only mount makes no lock, so a lock there is the lock of a writer
   mount or of a crash. When it is removed, a later read-only mount of a
   ro_compat store does not see it (M8) and has only the write-log check
   (M7). The effect was not tested in this phase.
8. **The parse-order proposal of RESEARCH Open Question 1** (an owner
   decision before the freeze). The locked rule is in the code: with an
   unknown flag, unknown lines are allowed; with no unknown flag, an
   unknown line is an error. A new setting line must always come with a
   flag, so a `compat` or `ro_compat` feature that adds a line is safe only
   if its flag is in the header too.
9. **`seed --hash <unknown name> <new dir>`** writes the header of the new
   store, then refuses the hash name (21.1-02 SUMMARY). The header check
   comes before the hash name check. Fixed by review fix WR-03: the name is
   read first, so nothing is made.
10. **The Linux form of `mount::start`** and the FMT-02 section of
    `scripts/docker-linux-test/verify-phases.sh` have run only in the
    Docker method of plan 21.1-03 (tar stream, `cargo build -j 1`). They
    have not run through `scripts/docker-linux-test/run.sh verify` (bind
    mount, build with no `-j`). The Linux CI job is the first run in that
    form.
11. **Red-run limits** (21.1-02 and 21.1-03 SUMMARYs): each red test
    stopped at its first failed assert, so not every command and option
    was shown red one by one. The asserts pass after the change.
12. **A read-write mount and `seed` do not take the store lock** (review
    fix WR-02). Only `gc`, `snapshot create` and `snapshot switch` refuse
    while a read-only mount runs. The rule for later releases is in
    `docs/design/store-format.md` (Store lock).

