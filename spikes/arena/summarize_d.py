import json, glob, statistics as S, sys, os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'results', 'd')
def load(f):
    return [json.loads(l) for l in open(f) if l.startswith('{')]
def med(xs):
    xs = [x for x in xs if x is not None]
    return S.median(xs) if xs else float('nan')
def mm(xs):
    xs = [x for x in xs if x is not None]
    return (min(xs), max(xs)) if xs else (float('nan'),) * 2
MIB = 1 << 20
out = []
p = out.append
# probe
f = R + '/dprobe.jsonl'
if os.path.exists(f):
    recs = load(f)
    p('## dprobe (raw F_PUNCHHOLE facts, 64 MiB file full of data; median of 3 per file kind)')
    keys = ['fs_block','errno_off_unaligned','errno_len_unaligned','errno_512','errno_readonly_fd','errno_aligned',
            'blocks_freed_before_sync','blocks_freed_after_sync','bavail_delta','len_unchanged','reads_zero_same_fd',
            'reads_zero_new_fd','neighbours_intact','seek_hole_at','seek_data_from_hole','punch_1mib_ms',
            'punch8k_p50_us','punch8k_p99_us','punch8k_cpu_us','sync_after_punch_ms','sync_after_1000_ms',
            'write_hole_sync_p50_ms','write_hole_sync_p99_ms','overwrite_sync_p50_ms','overwrite_sync_p99_ms','load1']
    vs = ['grow','sparse','reserve','zeroed']
    p('| field | ' + ' | '.join(vs) + ' |')
    p('|---|' + '---|' * len(vs))
    for k in keys:
        p(f'| {k} | ' + ' | '.join(f"{med([r.get(k) for r in recs if r['variant']==v]):.6g}" for v in vs) + ' |')
    p('')
# dgc
for pat in ['file', 'obj']:
    p(f'## dgc pattern={pat} (256 MiB mixed data, 64 MiB arenas; one GC; median of 3, [min..max] for times)')
    p('| dead | var | freed MiB | dead MiB | freed/dead | pred punchable | space amp before->after | GC wrote data MiB | GC wrote idx KiB | punch calls | GC wall s [min..max] | GC cpu s [min..max] | read after/before wall | load1 |')
    p('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|')
    for dead in ['0.1', '0.3', '0.5', '0.7', '0.9']:
        f = f'{R}/dgc_{pat}_{dead}.jsonl'
        if not os.path.exists(f):
            continue
        recs = load(f)
        for v in ['RS', 'A', 'D', 'D0']:
            rs = [r for r in recs if r['variant'] == v]
            if not rs:
                continue
            g = lambda k: med([r.get(k) for r in rs])
            w = mm([r['wall_s'] for r in rs]); c = mm([r['cpu_s'] for r in rs])
            ratio = med([r['read_after_wall_s'] / r['read_before_wall_s'] for r in rs])
            pf = g('pred_frac') if v != 'RS' else float('nan')
            p(f"| {dead} | {v} | {g('freed')/MIB:.1f} | {g('dead_target')/MIB:.1f} | {g('freed_frac'):.3f} | {pf:.3f} | {g('space_amp_before'):.3f}->{g('space_amp'):.3f} | {g('gc_wrote_data')/MIB:.1f} | {g('gc_wrote_idx')/1024:.0f} | {g('punch_calls'):.0f} | {g('wall_s'):.3f} [{w[0]:.3f}..{w[1]:.3f}] | {g('cpu_s'):.3f} [{c[0]:.3f}..{c[1]:.3f}] | {ratio:.3f} | {g('load1'):.0f} |")
    p('')
# churn
for pat in ['file', 'obj']:
    f = f'{R}/dchurn_{pat}.jsonl'
    if not os.path.exists(f):
        continue
    recs = load(f)
    p(f'## dchurn pattern={pat} (192 MiB live, 6 rounds of 25 % delete + 25 % new, GC each round; median of 3)')
    p('| var | space amp r1..r6 (alloc / live stored) | WA (user+GC data+idx)/user | GC data MiB | GC idx MiB | GC wall s total [min..max] | GC cpu s total | read MiB/s wall | arena files end | load1 |')
    p('|---|---|---|---|---|---|---|---|---|---|')
    for v in ['RS', 'A', 'D']:
        rs = [r for r in recs if r['variant'] == v]
        if not rs:
            continue
        g = lambda k: med([r.get(k) for r in rs])
        amps = ' '.join(f"{g(f'amp_r{i}'):.3f}" for i in range(1, 7))
        w = mm([r['gc_wall_s'] for r in rs])
        rd = med([r['read_mib'] / r['read_wall_s'] for r in rs])
        p(f"| {v} | {amps} | {g('wa_gc'):.3f} | {g('gc_wrote_data')/MIB:.1f} | {g('gc_wrote_idx')/MIB:.1f} | {g('gc_wall_s'):.2f} [{w[0]:.2f}..{w[1]:.2f}] | {g('gc_cpu_s'):.2f} | {rd:.0f} | {g('files_end'):.0f} | {g('load1'):.0f} |")
    p('')
print('\n'.join(out))
