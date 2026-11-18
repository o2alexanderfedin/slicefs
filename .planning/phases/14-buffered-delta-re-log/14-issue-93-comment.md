Fixed in #103 (merge 75a9743) with #98. A commit now logs, for each open file with random writes, only the bytes changed since the last rebase, in records of at most 1 MiB. Before, a commit logged the full buffer of the file in one record, and a record over 64 MiB stopped the replay at that record.

Tests:
- `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase`: the test uses a 4 MiB file and a 16 MiB file. After one 4 KiB write, a commit logs 4096 bytes. After a second 4 KiB write at a different place, with no rebase, the next commit logs 8192 bytes. After a rebase and one more 4 KiB write, the commit logs 4096 bytes. The values are the same for both file sizes.
- `an_overlay_with_more_than_64_mib_of_relog_replays`: one commit logs a 65 MiB extent again (more than 64 MiB in total). After a crash, the replay gives the same content.
- `commit_and_fsync_do_not_rebase_an_open_overlay`: a commit and an fsync do not chunk the file again.
