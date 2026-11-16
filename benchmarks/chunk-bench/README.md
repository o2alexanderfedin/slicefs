# chunk-bench

Compares FastCDC + BLAKE3 with blockset (the current SliceFS chunker and hash) on real data. Input for the store decision in #15; the benchmark is #17.

The tool does not change the store format and does not write to disk. Each engine stores its objects or chunk hashes in memory.

## Run

```sh
cargo build --release -p chunk-bench
B=target/release/chunk-bench

# One series: the inputs share one dedup set, in the given order.
$B fastcdc:16 tar-1.85=boost_1_85_0.tar tar-1.86=boost_1_86_0.tar tar-1.87=boost_1_87_0.tar
$B blockset   tree-1.85=boost_1_85_0/ tree-1.86=boost_1_86_0/
```

- Engine `fastcdc:<avg KiB>`: FastCDC (v2020) with min = avg / 4 and max = avg × 4. Each chunk is hashed with BLAKE3. A chunk is new if its hash was not seen before in the series.
- Engine `blockset`: `FileStorageAdd` + `State::push_all` for each file, the same calls that SliceFS makes. An object is new if its name was not written before in the series. Stored bytes include the blockset node format.
- A path is a file (for example a tar) or a directory. A directory is read as a tree of regular files, sorted by path. Symbolic links are skipped.

## Output

One markdown row per input:

| Column | Meaning |
|---|---|
| new units | New chunks (FastCDC) or new objects (blockset) |
| new stored MB | Bytes that earlier inputs in the series did not store |
| new / logical | Stored bytes per input byte (lower is better; blockset can be above 1 because of its node format) |
| CPU s | User + system CPU time of the chunk + hash work only (file reads excluded) |
| MB/s (CPU) | Logical MB per CPU second, one thread |

CPU time is used, not wall-clock time, because wall-clock time depends on the other load on the machine.

For FastCDC, the stored bytes do not include the manifest (32 bytes per chunk reference). The tool prints the reference count so that you can add it.
