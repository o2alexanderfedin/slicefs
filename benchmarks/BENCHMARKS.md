# SliceFS Benchmark Suite

## Purpose

This directory contains fio job files and a runner script to establish performance
baselines for SliceFS as a daily-driver filesystem. The benchmarks exercise the four
most important workload patterns: sequential write, sequential read, random read, and
metadata-heavy small-file operations.

## Performance Targets

These are targets for NVMe hardware, not hard CI gates (hardware-dependent results
are not suitable for automated enforcement):

| Workload          | Target     | Notes                                         |
|-------------------|------------|-----------------------------------------------|
| Sequential write  | 200 MB/s   | 4K blocks, 4 jobs, direct I/O with fsync      |
| Sequential read   | 400 MB/s   | 4K blocks, 4 jobs, direct I/O                 |
| Random read       | 100 MB/s   | 4K blocks, 4 jobs, 1 GB working set           |
| Small file create | 5000 ops/s | 4K files x 1000, metadata-heavy               |

Benchmarks are not run in CI. Developers run them manually on target hardware
and paste results into this document.

## Methodology

- **Tool:** [fio](https://github.com/axboe/fio) — industry-standard I/O benchmark
- **Engine:** `ioengine=sync` — matches how most application code performs I/O
- **Direct I/O:** `direct=1` — bypasses the OS page cache, measures filesystem throughput
- **Fsync:** `end_fsync=1` on writes — ensures data is persisted, not just buffered
- **Group reporting:** `group_reporting=1` — aggregates results across all jobs
- **Output format:** JSON — machine-readable for tracking over time

## How to Run

First, mount SliceFS on the directory you want to benchmark:

```bash
slicefs mount /tmp/mnt --store /tmp/bench-store
```

Then run all benchmarks:

```bash
./benchmarks/run_benchmarks.sh /tmp/mnt ./results
```

Results are written as JSON to the `./results/` directory with timestamps.

To run a single benchmark:

```bash
fio benchmarks/sequential_write.fio --directory=/tmp/mnt --output-format=json
```

To unmount after benchmarking:

```bash
# Linux
fusermount -u /tmp/mnt

# macOS (FUSE-T)
umount /tmp/mnt
```

## Job Descriptions

### sequential_write.fio

Measures sequential write throughput using 4K block size, 4 parallel jobs, and 1 GB
per job (4 GB total). Uses direct I/O with end-of-run fsync to measure real durable
write throughput rather than cached writes. This is the primary bottleneck for large
file ingestion workloads. Target: 200 MB/s on NVMe.

### sequential_read.fio

Measures sequential read throughput with the same block size and parallelism as the
write test. Reads files created by a prior sequential write run. Tests the CAS block
retrieval path including dedup index lookups. Target: 400 MB/s on NVMe (reads are
faster since there is no fsync overhead).

### random_read.fio

Measures random read throughput with 4K block size and a 1 GB working set per job.
Tests the worst case for the CAS block lookup path: random access patterns that
stress the dedup index and block store seek latency. Target: 100 MB/s on NVMe.

### small_files.fio

Measures metadata-heavy workload performance: creates 1000 small (4K) files using a
single job. Tests inode allocation, directory entry insertion, and metadata flush
throughput. This workload exercises the WAL and DictMetadataStore heavily. Target:
5000 file ops/s on NVMe.

## Dedup Index Memory

The bloom filter used for the dedup index is fixed-size, allocated once at startup
based on a configured expected element count and false positive rate. It does not grow
with the number of stored blocks. This means memory usage is bounded regardless of
filesystem content size.

To verify memory is bounded under load, check the resident set size (RSS) of the
slicefs process after ingesting a large dataset:

```bash
# Mount and ingest data
slicefs mount /tmp/mnt --store /tmp/bench-store &
SLICEFS_PID=$!
cp -r /large/dataset /tmp/mnt/

# Check RSS (in KB on Linux)
cat /proc/$SLICEFS_PID/status | grep VmRSS

# On macOS
ps -o rss= -p $SLICEFS_PID
```

Under a 100 GB dataset with a bloom filter configured for 10M elements at 1% FPR,
expected RSS for the bloom filter alone is approximately 12 MB (fixed). Total process
RSS scales with open file handles and WAL buffer, not with dataset size.

## Results

*[Run benchmarks and paste results here]*

```
Date:
Hardware:
OS:
SliceFS version:
Store location (HDD/SSD/NVMe):

Sequential write:
Sequential read:
Random read:
Small file create:
```
