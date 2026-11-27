# Upgrade a store

This release reads and writes store format 8 only.

A store made by SliceFS v2.3.0 is store format 7. This release does not open
it. It prints a message that names this page and changes nothing in the
store. Every command does this: mount, stats, gc, seed, rehash and the
snapshot commands.

## See the format of a store

- Read the first line of the file `FORMAT` in the store directory. It starts
  with `slicefs-store 7` or `slicefs-store 8`.
- Or run `slicefs stats <store>` with v2.3.0 and read the `Store format` row.

## What to do with a format 7 store

Keep using SliceFS v2.3.0 for it. The steps to copy a format 7 store into a
new format 8 store will be added to this page with issue #155. A copy of the
live tree does not carry over the snapshots of the old store.
