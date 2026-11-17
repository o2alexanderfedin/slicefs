# chunk-bench

Measures FastCDC + BLAKE3 chunking on real data: stored bytes, dedup across a series, and CPU time. It was the input for the store decision in #15 (benchmark #17). The tool had a `blockset` engine for that comparison; it was removed with blockset. The results of the comparison are in #17.

The tool does not change the store format and does not write to disk. It keeps the chunk hashes in memory.

## Run

```sh
cargo build --release -p chunk-bench
B=target/release/chunk-bench

# One series: the inputs share one dedup set, in the given order.
$B fastcdc:16 tar-1.85=boost_1_85_0.tar tar-1.86=boost_1_86_0.tar tar-1.87=boost_1_87_0.tar
$B fastcdc:8  tree-1.85=boost_1_85_0/ tree-1.86=boost_1_86_0/
```

- Engine `fastcdc:<avg KiB>`: FastCDC (v2020) with min = avg / 4 and max = avg × 4. Each chunk is hashed with BLAKE3. A chunk is new if its hash was not seen before in the series.
- A path is a file (for example a tar) or a directory. A directory is read as a tree of regular files, sorted by path. Symbolic links are skipped.

## Output

One markdown row per input:

| Column | Meaning |
|---|---|
| new units | New chunks |
| new stored MB | Bytes that earlier inputs in the series did not store |
| new / logical | Stored bytes per input byte (lower is better) |
| CPU s | User + system CPU time of the chunk + hash work only (file reads excluded) |
| MB/s (CPU) | Logical MB per CPU second, one thread |

CPU time is used, not wall-clock time, because wall-clock time depends on the other load on the machine.

For FastCDC, the stored bytes do not include the manifest (32 bytes per chunk reference). The tool prints the reference count so that you can add it.
