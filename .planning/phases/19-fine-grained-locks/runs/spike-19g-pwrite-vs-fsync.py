# (syncer in a separate process: no GIL effect)
# pwrite latency while another thread syncs (same file / other file), per sync kind.
import os, sys, threading, time, fcntl, tempfile, platform
F_FULLFSYNC = getattr(fcntl, "F_FULLFSYNC", 51)
d = tempfile.mkdtemp(dir=sys.argv[1] if len(sys.argv) > 1 else None)
def run(kind, same, secs=2.0):
    a = os.open(os.path.join(d, "a"), os.O_RDWR | os.O_CREAT, 0o644)
    b = a if same else os.open(os.path.join(d, "b"), os.O_RDWR | os.O_CREAT, 0o644)
    stop = time.time() + secs; syncs = []
    def syncer():
        off = 0
        while time.time() < stop:
            os.pwrite(b, b"y" * 4096, (1 << 30) + off); off += 4096  # dirty the synced file
            t = time.perf_counter()
            if kind == "none": time.sleep(0.02)
            elif kind == "fsync": os.fsync(b)
            elif kind == "fdatasync": os.fdatasync(b)
            else: fcntl.fcntl(b, F_FULLFSYNC)
            syncs.append(time.perf_counter() - t)
    import multiprocessing as mp
    th = mp.get_context("fork").Process(target=syncer); th.start()
    lat = []; off = 0
    while time.time() < stop:
        t = time.perf_counter(); os.pwrite(a, b"x" * 8192, off); lat.append(time.perf_counter() - t); off += 8192
        time.sleep(0.0005)
    th.join(); lat.sort()
    q = lambda p: lat[min(len(lat) - 1, int(p * len(lat)))] * 1e6
    slow = sum(1 for x in lat if x > 0.005)
    print(f"{platform.system()} sync={kind:9s} target={'same' if same else 'other':5s} pwrites={len(lat)} p50_us={q(.5):.0f} p99_us={q(.99):.0f} max_us={lat[-1]*1e6:.0f} over5ms={slow} syncer=separate-process", flush=True)
    os.close(a)
    if not same: os.close(b)
    for n in ("a", "b"):
        try: os.unlink(os.path.join(d, n))
        except FileNotFoundError: pass
kinds = ["none", "fsync"] + (["fdatasync"] if hasattr(os, "fdatasync") else []) + (["fullfsync"] if platform.system() == "Darwin" else [])
for k in kinds:
    for same in (True, False):
        run(k, same)
