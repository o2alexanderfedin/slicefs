# slicefs-dedup

Persistent, redb-backed `DedupIndex` for SliceFS.

## API surface

```rust
use slicefs_dedup::{DedupIndexConfig, DurabilityMode, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex};

// Mount-time
let cfg = DedupIndexConfig::builder("/path/to/store/cas")
    .mode(DurabilityMode::Default)
    .build();
let idx = RedbDedupIndex::open(cfg.clone())
    .or_else(|_| RedbDedupIndex::create(cfg.clone()))?;

// Hot path
if !idx.bloom_check(&hash) { /* fast: definitely absent */ }
match idx.lookup(&hash)? { /* Present | Absent | DefinitelyAbsent */ }
idx.insert(&hash)?;
idx.remove(&hash)?;

// Operator path
RedbDedupIndex::rebuild_from_cas(cfg)?; // offline; idempotent
```

## Modes

| Mode | Durability | Coalesce window | verify_on_present |
|------|------------|-----------------|-------------------|
| `Seed` | `None` | 20 ms / 100 K batch | false |
| `Default` | `Immediate` (per-mode override; redb 4.x removed `Eventual`) | 2 ms / 10 K batch | false |
| `Paranoid` | `Immediate` per insert | 0 ms / 1-tx | **true** |

## Layout

`<store>/cas/.dedup-index/`

- `index.redb`        authoritative SET (table `dedup_index_v1`)
- `index.redb.lock`   redb's flock — multi-process exclusion
- `bloom.snap`        advisory bloom snapshot, rename-atomic, xxh3-128 + CRC32C
- `manifest.json`     JSON sidecar with version + last_clean_shutdown

## Caller contract

A successful `insert(h)` reply does **not** promise post-crash visibility.
With `Durability::Eventual` (or its redb-4.x successor `Immediate`-with-batch-coalesce), the commit can return before the device flushed; a `kill -9` within the group-commit window can lose the entry on next mount. After a crash, a follow-up `lookup(h)` may return `Absent` — benign per `[ARCHITECTURE §3 I4]`.

**Callers MUST NOT cache "I inserted h" as authoritative across a crash boundary without first calling `flush()`.** See ARCHITECTURE §8.1 "Caller observability note" and the FI-13 test in `tests/failure_injection_kill9.rs`.

## See also

`.planning/research/dedup-index/ARCHITECTURE.md` — binding spec.
