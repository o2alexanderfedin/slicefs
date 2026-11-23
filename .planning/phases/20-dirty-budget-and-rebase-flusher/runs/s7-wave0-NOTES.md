# S7 Wave 0 baseline (plan 20-01 Task 3)

- System: OrbStack Docker on a macOS host, kernel `Linux 7.2.12-orbstack x86_64`, 32 CPUs in the container, store on the container overlay. In process, no mount.
- Commit measured: `1bfc290` (tree clean, sent into the container as a tar stream; fresh target volume `slicefs-target-p20`).
- Date: 2026-11-22.
- Host load (`uptime`): 32.63 / 31.91 / 30.52 at the start, 18.43 / 23.28 / 26.85 at the end of the six runs. Other projects used the host. The load of the OrbStack VM (`/proc/loadavg` in the container) was 2.9 to 4.2 for the 1-minute value; the log has the value before each run.
- Workload: the defaults of `s7_budget_baseline` (4 writers, 1024 MiB file each, 128 MiB bigwrite mix each, fsync every 64 MiB, commit every 30 s), release build, dirty budget 64 MiB.

IMAGE_ID = sha256:71e0cab49714973153cac51c263ee0256568a0afb4bb5e9775439b6781891f13
RUSTC = rustc 1.100.0 (0ef1bc386 2026-11-09)

Image note: Docker here uses the containerd image store. Each `docker build -q` gives a new image ID, also when every step comes from the cache. The image config did not change (created 2026-11-13, 7 layers). The six measurement runs used the build with ID `sha256:56b5f12a62850cd52fda8dea1c44d87415ce27b735393b1be2ba730acd13e415` (see the log). The arena run built again and got the ID above, which is the ID of the tag `slicefs-fusetest` now. Later plans must not build the image again; a build changes the ID but not the content.

## The six runs

All six runs passed their model check (`test result: ok`, no mismatch, no errno).

| Run | serialize | vmhwm_mib | peak_rss_mib | rss_start_mib | rss_after_writes_mib | max_dirty_mib | slowest_write_ms | rebase_steps | commits |
|---|---|---|---|---|---|---|---|---|---|
| P_old 1 | true | 248 | 244 | 41 | 222 | 64 | 2688.5 | 453 | 3 |
| P_old 2 | true | 260 | 255 | 41 | 222 | 64 | 1890.6 | 454 | 3 |
| P_old 3 | true | 254 | 249 | 40 | 222 | 64 | 1544.6 | 454 | 3 |
| P0 1 | false | 267 | 267 | 40 | 207 | 64 | 330.0 | 607 | 2 |
| P0 2 | false | 247 | 241 | 39 | 214 | 64 | 2816.5 | 454 | 2 |
| P0 3 | false | 263 | 257 | 39 | 215 | 64 | 2235.6 | 455 | 2 |

## The cap rule (stated in plan 20-01 before the runs)

- P_old (serialize on): max 260 MiB, spread 260 - 248 = 12 MiB.
- P0 (serialize off): max 267 MiB, spread 267 - 247 = 20 MiB.
- Branch check: no failed run. P0 max (267) is not more than P_old max + 64 (324). The cap is above 256, so the arena branch applied (see below).
- Pm = 267. Spread = 20.
- B = 64 (dirty budget). N x W = 4 x 1 = 4. O = Pm - B - N x W = 267 - 64 - 4 = 199. F = 8 (one flusher step, 2 x `FLUSH_STEP`).
- R = max(16, ceil(267 / 10) = 27, 2 x 20 = 40) = 40.
- B + N x W + O + F + R = Pm + F + R = 267 + 8 + 40 = 315, rounded up to a multiple of 8 = 320.

CAP_MIB = 320

## Arena run (CAP_MIB above 256)

One serialize-off run with `MALLOC_ARENA_MAX=2` (`runs/s7-wave0-arena.log`, not part of the rule): vmhwm_mib 230, peak_rss_mib 230, rss_after_writes_mib 207, slowest_write_ms 546.8, rebase_steps 454, commits 2, passed. Host load 19.44 / 23.08 / 26.64. This is 17 to 37 MiB below the three P0 runs, so a part of the overhead O is glibc malloc arenas of the parallel threads. The value is not an input of the cap.

## Compared with Phase 18

Phase 18 S7 (old code `d24da49`, serialized, one run, getrusage) gave a peak RSS of 234 MiB. The P_old runs here give a getrusage peak of 244 to 255 MiB and a VmHWM of 248 to 260 MiB on the Phase 19 code. So the Phase 19 code with the same serialized workload holds about 10 to 20 MiB more at peak. With 256 MiB as the cap, by `peak_rss_mib` all three P_old runs are within it, but only P0 run 2 is (`within_cap=false` for P0 runs 1 and 3). By VmHWM, P_old run 2 (260) is also above 256.
