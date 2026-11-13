# Btrfs deduplication: alignment constraints and the boundary-shift problem

Scope: what alignment and granularity rules Btrfs extent sharing (reflink / FIDEDUPERANGE) follows, and whether dedup can survive a 1-byte insert, change, or delete, or share variable-size CDC chunks that start at arbitrary byte offsets. Kernel source was read from torvalds/linux `master` on 2026-11-12.

## Q1. On-disk model: can a file extent item reference a sub-range of an extent, and must offsets be sector-aligned?

### Takeaway
Yes, a file extent item can point into the middle of an existing on-disk extent (through its `offset` and `num_bytes` fields). But the tree-checker requires every one of these fields, plus the file offset in the item key, to be a multiple of `sectorsize`. The only exception is an inline extent, which must sit at file offset 0 and be at most one sector. So sharing is variable-length but always sector-granular. Byte-granular references cannot be expressed on disk.

### Cited Findings
- `struct btrfs_file_extent_item` has the fields `generation, ram_bytes, compression, encryption, other_encoding, type, disk_bytenr, disk_num_bytes, offset, num_bytes`. The comment on `offset` reads: "the logical offset in file blocks (no csums) this extent record is for. This allows a file extent to point into the middle of an existing extent on disk, sharing it between two snapshots". The comment on `num_bytes` reads: "the logical number of file blocks ... always reflects the size uncompressed". — [include/uapi/linux/btrfs_tree.h](https://raw.githubusercontent.com/torvalds/linux/master/include/uapi/linux/btrfs_tree.h)
- The tree-checker (`check_extent_data_item`) rejects any EXTENT_DATA key whose file offset is not sector-aligned: `if (!IS_ALIGNED(key->offset, sectorsize)) ... "unaligned file_offset for file extent, have %llu should be aligned to %u" ... return -EUCLEAN;` — [fs/btrfs/tree-checker.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/tree-checker.c)
- For regular and prealloc extents, the tree-checker requires `ram_bytes`, `disk_bytenr`, `disk_num_bytes`, `offset` and `num_bytes` to be sector-aligned: `CHECK_FE_ALIGNED(leaf, slot, fi, ram_bytes, sectorsize) || ... disk_bytenr ... disk_num_bytes ... offset ... num_bytes ... return -EUCLEAN;`. This check runs whatever the compression type, so compressed extents get it too. — [fs/btrfs/tree-checker.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/tree-checker.c)
- Inline extents: "Inline extent must have 0 as key offset". An item with a nonzero key offset is rejected with `-EUCLEAN`. — [fs/btrfs/tree-checker.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/tree-checker.c)
- Maximum extent size: `#define BTRFS_MAX_EXTENT_SIZE SZ_128M`. — [fs/btrfs/fs.h](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/fs.h)
- Compressed extents are limited to 128 KiB: `#define BTRFS_MAX_COMPRESSED (SZ_128K)` and `#define BTRFS_MAX_UNCOMPRESSED (SZ_128K)`. — [fs/btrfs/compression.h](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/compression.h)
- At mount, sectorsize must be a power of two and at least `BTRFS_MIN_BLOCKSIZE` (4 KiB): `if (!is_power_of_2(sectorsize) || sectorsize < BTRFS_MIN_BLOCKSIZE || ...)`. The current source also has code paths for `sectorsize > PAGE_SIZE` as well as the subpage case `sectorsize < PAGE_SIZE`. — [fs/btrfs/disk-io.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/disk-io.c)
- `/* Minimum data and metadata block size. */ #define BTRFS_MIN_BLOCKSIZE (SZ_4K)` and `#define BTRFS_MAX_BLOCKSIZE (SZ_64K)`. These are unconditional defines. — [fs/btrfs/fs.h](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/fs.h)

### Inferences
- Every shareable unit is described by `(file_offset, disk_bytenr, offset, num_bytes)`, and all four are multiples of sectorsize (4 KiB in practice). A file can therefore only reference data that sits at a sector-aligned position inside an extent, and only at a sector-aligned position in the file. "Variable size" is allowed, but only in steps of 4 KiB (one sector up to 128 MiB per extent; a compressed extent holds at most 128 KiB). A shared *range* can span many extents, so the per-extent cap does not limit the size of a dedup request.
- `nodesize` (16 KiB by default) is the size of a metadata (B-tree) block. It controls how many extent items fit in a leaf and the cost of metadata CoW. It has no effect on data extent alignment, which depends only on `sectorsize`.
- The `offset` field of a compressed extent indexes the decompressed data. It is still sector-aligned, so compression does not give a byte-granular reference either.

### Gaps
- I did not check the btrfs.readthedocs.io "On-disk format" page itself. The constraints above come straight from the kernel source, which is the authority the docs describe.
- I did not verify in this session the default sectorsize that current btrfs-progs `mkfs` uses on 64 KiB-page systems (for example ppc64le or some arm64). I believe recent btrfs-progs defaults to 4 KiB everywhere, but that is from memory and not sourced here.

## Q2. Can reflink/dedup operations target non-aligned offsets? What is the EOF / tail exception?

### Takeaway
No. The VFS layer that FIDEDUPERANGE and FICLONERANGE share (`generic_remap_checks`) rejects any request whose source or destination start offset is not block-aligned. Lengths are rounded down to whole blocks. The single exception is a partial last block, and only when the range ends at EOF. For dedupe, both files must end there.

### Cited Findings
- `generic_remap_checks()` says: "The start of both ranges must be aligned to an fs block. `if (!IS_ALIGNED(pos_in, bs) || !IS_ALIGNED(pos_out, bs)) return -EINVAL;`", with `bs = inode_out->i_sb->s_blocksize`. — [fs/remap_range.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/remap_range.c)
- Length rule in the same function: "If the user wanted us to link to the infile's EOF, round up to the next block boundary ... Otherwise, make sure the count is also block-aligned": `if (pos_in + count == size_in && (!(remap_flags & REMAP_FILE_DEDUP) || pos_out + count == size_out)) bcount = ALIGN(size_in, bs) - pos_in; else { if (!IS_ALIGNED(count, bs)) count = ALIGN_DOWN(count, bs); ... }`. Dedupe also requires "both ranges to be within EOF". — [fs/remap_range.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/remap_range.c)
- `generic_remap_check_len()`: "For deduplication we accept a partial EOF block only if it ends at the destination file's EOF (can not link it into the middle of a file)." If the range cannot be shortened, the call returns `-EBADE` for dedupe. — [fs/remap_range.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/remap_range.c)
- Btrfs's `btrfs_remap_file_range_prep` calls `generic_remap_file_range_prep(...)`, so all of the checks above apply to Btrfs. Btrfs also splits dedupe internally into chunks of `BTRFS_MAX_DEDUPE_LEN = SZ_16M` and passes `ALIGN(len, bs)` to `btrfs_clone`. — [fs/btrfs/reflink.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/reflink.c)
- Btrfs reflink.c on inline extents: "Inline extents always have to start at file offset 0 and can never be bigger then the sector size. We can never clone only parts of an inline extent, since all reflink operations must start at a sector size aligned offset, and the length must be aligned too or end at the i_size (which implies the whole inlined data)." Cloning an inline extent copies its bytes (`clone_copy_inline_extent` / `copy_inline_to_page`). It does not share them. — [fs/btrfs/reflink.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/reflink.c)
- The man page says: "Disk filesystems generally require the offset and length arguments to be aligned to the fundamental block size", and "Neither Btrfs nor XFS support overlapping deduplication ranges in the same file." — [ioctl_fideduperange(2)](https://man7.org/linux/man-pages/man2/ioctl_fideduperange.2.html)
- History: the partial-EOF-block case has caused data-corruption bugs. Filipe Manana's commit "Btrfs: fix data corruption when deduplicating between different files" (Fixes: e1d227a "btrfs: Handle unaligned length in extent_same"; CC stable 4.2+) fixes a bug where deduping a source range that ends at a non-sector-aligned EOF into the middle of a destination file could overwrite good data with zeros. The fix rounds the dedupe length down to a sector boundary in that case. — [commit de02b9f (mirror in ctrliq kernel-src-tree)](https://github.com/ctrliq/kernel-src-tree/commit/de02b9f6bb65a6a1848f346f7a3617b7a9b930c0). In a 2018 kernel mailing-list post, Dave Chinner described a related problem: `extent_same_check_offsets()` extended the range for alignment checks but then used the original length for compare and dedupe (taken from the search snippet; I did not read the thread in full). — [Dave Chinner, 2018-08-20](https://lore.kernel.org/r/20180820010932.GV2234@dastard)
- The kernel dedupe path compares bytes, not hashes: `vfs_dedupe_file_range_compare()` reads both ranges through the page cache and compares them before remapping. — [fs/remap_range.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/remap_range.c)

### Inferences
- To share a range, you need src_offset ≡ 0 (mod 4096), dst_offset ≡ 0 (mod 4096), and byte-identical content. A partial last block can be shared only when it is the final block of both files.

### Gaps
- None material.

## Q3. The boundary-shift problem: changed byte vs inserted or deleted byte

### Takeaway
A changed byte costs only the one 4 KiB block that holds it. Everything else still matches at the same aligned offsets. If you insert or delete N bytes with N not a multiple of 4096, every byte after the edit moves to a new position modulo 4096. No aligned 4 KiB block of the new file then equals any aligned block of the old file, so neither duperemove nor bees, nor any other tool built on FIDEDUPERANGE, can recover sharing for the rest of the file. That includes the tail. Only inserts or deletes of exactly k·4096 bytes keep the alignment.

### Cited Findings
- Both offsets must be block-aligned (`pos_in` and `pos_out`), and the on-disk `offset` and file key offset must be sector-aligned. — [fs/remap_range.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/remap_range.c); [fs/btrfs/tree-checker.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/tree-checker.c)
- bees reads and hashes extents "one 4 KiB block at a time". It looks up each block hash in its table and uses a hit as an anchor to "a much longer run of adjacent duplicate blocks". Its table holds "pairs — one per 4 KiB block it can remember". — [bees docs/scan-one-algorithm.md](https://raw.githubusercontent.com/Zygo/bees/master/docs/scan-one-algorithm.md)
- bees constants: `BLOCK_SIZE_CLONE = 4096`, `BLOCK_SIZE_SUMS = 4096`, `BLOCK_SIZE_MAX_EXTENT_SAME = 4096 * 4096` (16 MiB), `BLOCK_SIZE_MAX_COMPRESSED_EXTENT = 128 * 1024`. — [bees src/bees.h](https://raw.githubusercontent.com/Zygo/bees/master/src/bees.h)
- bees on extent replacement: "btrfs cannot partially overwrite extents--the _entire_ extent must be replaced". bees also "avoids splitting the extent in the middle" for compressed extents. — [bees docs/how-it-works.md](https://raw.githubusercontent.com/Zygo/bees/master/docs/how-it-works.md)
- duperemove matches whole extents by default. Block-level matching is opt-in: `--dedupe-options=partial` is described as "Duperemove can often find more dedupe by comparing portions of extents to each other. This can be a lengthy, CPU intensive task so it is turned off by default." The `-b size` option means "Use the specified block size for reading file extents. Defaults to 128K." Its source sets `MIN_BLOCKSIZE (4U*1024)` and `MAX_BLOCKSIZE (1024U*1024)`. — [duperemove.8](https://raw.githubusercontent.com/markfasheh/duperemove/master/duperemove.8); [duperemove opt.h](https://raw.githubusercontent.com/markfasheh/duperemove/master/opt.h)
- The restic authors describe the general fixed-block failure: "what happens when the user adds a byte to the beginning of the file? ... The chunk boundaries ... would shift by one byte, changing every chunk in the file." With CDC, "any subsequent chunk will remain identical thanks to the content-defined chunk boundaries". restic uses a 64-byte Rabin window and cuts where the low 21 bits are zero. — [restic blog, 2015](https://restic.net/blog/2015-09-12/restic-foundation1-cdc/)

### Inferences (worked example)
- Let B = 1 byte + A. Block k of B (B[4096k, 4096k+4096)) holds A[4096k−1, 4096k+4095). For dedup, that block's content would have to equal some aligned block A[4096j, 4096j+4096). That happens only by coincidence, such as all-zero or periodic data. All-zero blocks are a known special case: bees turns them into holes. So for ordinary data, sharing is lost from the insertion point to EOF. That includes the last partial block, because the files no longer end at congruent offsets.
- Changed byte (same length): only block ⌊pos/4096⌋ differs. All other blocks match at identical aligned offsets, and a tool can share 100% minus 4 KiB. In practice, tool behaviour can lose more than 4 KiB. With default duperemove (extent-level matching), the changed extent as a whole no longer matches. With `partial`, at least the 128 KiB `-b` block that holds the change is likely lost unless you use a smaller `-b` (inference from the man page; not tested). bees may rewrite or copy parts of an extent, because btrfs cannot partially overwrite extents.
- Insert or delete of N bytes: sharing after the edit is possible if and only if N mod 4096 = 0. A sector size other than 4 KiB changes the modulus, not the principle.
- Smaller hash blocks in a tool cannot fix this. 4 KiB is the Btrfs minimum anyway (`BTRFS_MIN_BLOCKSIZE = SZ_4K`; duperemove's `MIN_BLOCKSIZE`; bees `BLOCK_SIZE_CLONE`). The limit is the kernel's alignment rule, not tool design.

### Gaps
- I found no bees or duperemove issue or doc that discusses the shift case explicitly. The conclusion follows from kernel constraints, not from a tool statement.

## Q4. Variable-length extents, and whether CDC can be layered on Btrfs

### Takeaway
Btrfs can share variable-length ranges, but only as multiples of the sector size at aligned offsets. You can dedupe a 12 KiB or 52 KiB range, but not a 13,377-byte chunk that starts at byte 7. CDC chunk boundaries (for example 2–64 KiB FastCDC chunks) would have to be rounded to 4 KiB. Rounding does not help, because the problem is not the chunk's length but its position modulo 4096 in each file. After a non-4K shift, a chunk with identical content sits at a different residue in B than in A, and no aligned reference can bridge that. I found no tool or paper that applies CDC to Btrfs reflink dedup.

### Cited Findings
- Extents are any sector multiple up to `BTRFS_MAX_EXTENT_SIZE` (128 MiB); a compressed extent holds at most 128 KiB. A shared range may span several extents, and the dedupe ioctl handles long requests internally in 16 MiB pieces. — [fs/btrfs/fs.h](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/fs.h); [fs/btrfs/compression.h](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/compression.h); [fs/btrfs/reflink.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/reflink.c)
- FastCDC (Xia et al., USENIX ATC '16) is a Gear-based CDC with simplified cut-point judgment, sub-minimum cut-point skipping, and normalized chunking. It is about 10x faster than Rabin-based CDC. — [USENIX ATC'16 FastCDC](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia)
- A search for CDC combined with Btrfs reflink found only general CDC material and the kernel alignment threads above. Nothing combined the two. — e.g. [Chonkers CDC paper (arXiv 2509.11121)](https://arxiv.org/pdf/2509.11121), [Kruus et al., FAST'10 bimodal CDC](https://static.usenix.org/events/fast10/tech/full_papers/kruus.pdf)

### Inferences
- CDC fixes the boundary problem: it decides where chunks start. Btrfs then adds a placement problem: a chunk can only be referenced at the same residue modulo 4096 in both files. In a plain POSIX file, a byte's position is fixed by the file's contents (everything before it), so a filesystem-level dedup agent cannot choose placement. Even a perfect CDC index can share only the chunks whose old and new offsets happen to be congruent mod 4096. After a 1-byte shift, about 0 of them are.
- The only ways around it are (a) a writer that controls layout and pads each chunk to a 4 KiB boundary. That changes the file's byte content, so it only works for container formats that tolerate padding (see Q5). Or (b) an indirection layer above the block filesystem whose extent map has byte-granular `(file_offset, chunk_id, chunk_offset, length)`, so that logical bytes can start at any offset inside a stored chunk. Btrfs's own `offset`/`num_bytes` fields are that kind of indirection, but they are clamped to sector granularity by the tree-checker. A byte-granular variant would be a format change, not a tool change.
- Inline extents (at most 1 sector, file offset 0 only) and compressed extents (offset is in decompressed space but still sector-aligned) do not give a way to reference byte-unaligned data. Inline data is copied, not shared, on clone.
- Waste when chunks are rounded: if CDC chunks with an 8 KiB average were trimmed inward to aligned interiors, each chunk would lose on average about 4 KiB at its edges (about 2 KiB at each end) even with no shift at all. That makes small-average CDC a poor fit even before considering placement.

### Gaps
- No primary source found for any Btrfs proposal to add byte-granular extent offsets.
- No measurement found of dedup ratio for block-aligned versus CDC on the same shifted dataset on Btrfs. The near-zero-after-shift claim is a derivation, not a benchmark.

## Q5. Application-level workarounds, and other block filesystems (XFS, ext4)

### Takeaway
The working workaround is to make the producer align data to 4 KiB, as in the 2024 GNU tar `--reflink` patches, which pad archive members to 4096-byte boundaries. XFS uses the same VFS `generic_remap_file_range_prep` checks, so it has the same block-alignment rule. ext4 in mainline has no `remap_file_range` operation, so it supports neither reflink nor FIDEDUPERANGE. The limitation applies to block filesystems in general, not to Btrfs only.

### Cited Findings
- GNU tar `--reflink` patch (Matteo Croce, "[PATCH 1/4] reflink: align data on block size on archive create", 26 Oct 2024): "Add a --reflink option which, on archive creation, aligns file data on a 4k block boundary. This is done by padding the header with a standard \"comment\" pax record, which will be ignored on extraction." — [bug-tar msg00028](https://lists.endsoftwarepatents.org/archive/html/bug-tar/2024-10/msg00028.html). Per a search snippet I did not open, the other messages in the series add extraction through FICLONERANGE with a copy fallback, plus a benchmark. I have not verified the figures. — [msg00027](https://lists.endsoftwarepatents.org/archive/html/bug-tar/2024-10/msg00027.html), [msg00029](https://lists.gnu.org:443/archive/html/bug-tar/2024-10/msg00029.html), [msg00031](https://lists.endsoftwarepatents.org/archive/html/bug-tar/2024-10/msg00031.html)
- XFS: `xfs_file_remap_range()` accepts `REMAP_FILE_DEDUP | REMAP_FILE_ADVISORY` and calls `xfs_reflink_remap_prep()`, which calls `generic_remap_file_range_prep(...)`. The same `IS_ALIGNED(pos_in/pos_out, bs)` checks therefore apply. — [fs/xfs/xfs_file.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/xfs/xfs_file.c); [fs/xfs/xfs_reflink.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/xfs/xfs_reflink.c); [fs/remap_range.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/remap_range.c)
- ext4: `fs/ext4/file.c` in current master (1,086 lines, defines `ext4_file_operations`) contains no `remap_file_range` hook, so ext4 has no reflink or dedupe-range support. — [fs/ext4/file.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/ext4/file.c)
- The man page gives the general rule: "Disk filesystems generally require the offset and length arguments to be aligned to the fundamental block size." — [ioctl_fideduperange(2)](https://man7.org/linux/man-pages/man2/ioctl_fideduperange.2.html)

### Inferences
- Producer-side alignment, such as tar padding or per-file 4 KiB-aligned members in container or image formats, keeps unchanged members shareable after edits to other members. It does not help with byte inserts inside one member.
- Compression-level tricks like `gzip --rsyncable` or `zstd --rsyncable` make compressed output resynchronize after an edit. The resynchronized output still lands at arbitrary byte offsets, so for block-FS dedup they only help if combined with alignment padding. (Unsourced in this session; see Gaps.)
- For SliceFS-style designs: if byte-shift-resilient sharing is required, the mapping from logical file bytes to stored chunks has to sit above the block filesystem's reflink layer, with byte-granular offsets. Btrfs, XFS or ext4 then serve only as aligned chunk storage.

### Gaps
- I did not fetch primary docs for `gzip --rsyncable` or `zstd --rsyncable` in this session.
- I did not confirm whether the GNU tar `--reflink` patches were merged into a released GNU tar version as of 2026.
- I did not survey bcachefs or ZFS (block cloning in OpenZFS 2.2 is record-aligned). From memory, they share the same block or record granularity limit, but this is unverified here.
