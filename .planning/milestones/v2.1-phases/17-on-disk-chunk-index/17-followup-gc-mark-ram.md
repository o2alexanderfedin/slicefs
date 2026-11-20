storage: bound the RAM of the GC mark set and the touched set

Deferred from #85 (on-disk chunk index, PR #109).

## Problem

After #85 the chunk index needs about 4 B of RAM for each object. Online GC still holds sets that grow with the object count:

- The live set: a `HashSet<ObjectId>` that the caller gives to `gc_finish`. It holds every live object.
- The touched sets: `Inner::touched` records the objects put or read after `gc_begin`, and a second touched set records them while `gc_finish` runs.

Measured with `index-bench gc` (macOS, 3 of 4 objects live, see `docs/design/chunk-store.md`, section "Index results (#85)"): the peak RSS of the gc process was 1,416 MB at 10M objects and 7,427 MB at 100M objects (75M live IDs). This peak includes the live set. The split between the live set and the other parts of the process is not measured. Thus during a GC the RAM grows with the live object count.

## To examine

- A mark on disk: write the live IDs to a sorted temporary file, then compare it with the sorted index runs in one merged pass.
- A mark with a filter: a Bloom filter of the live IDs. A false positive only keeps a dead object, so no live object is lost. The cost is the dead objects that stay (about 1 % at 10 bits for each key). An exact check could remove these later.
- The same question for the touched sets, which grow with the write load during a GC.

## Measure

- The peak RSS of the gc process at 10M and 100M objects, before and after.
- The GC wall time and the longest hold of the store lock (`last_gc_timing`).
- The dead objects that stay after the GC.
