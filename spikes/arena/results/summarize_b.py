import json, sys, glob, os, statistics as st
R = '/Users/alexanderfedin/projects/slicefs/.claude/worktrees/wf_6fd0291a-cdd-6/spikes/arena/results/b'
def load(name):
    p = os.path.join(R, name + '.jsonl')
    if not os.path.exists(p): return []
    return [json.loads(l) for l in open(p) if l.strip()]
def med(v): return st.median(v) if v else float('nan')
def table(name, fields, base):
    recs = load(name)
    if not recs:
        print(f'### {name}: not run\n'); return
    vs = []
    for r in recs:
        if r['variant'] not in vs: vs.append(r['variant'])
    print(f'### {name}  (n per variant: ' + ', '.join(f"{v}={sum(1 for r in recs if r['variant']==v)}" for v in vs) +
          f"; load1 {min(r['load1'] for r in recs):.0f}-{max(r['load1'] for r in recs):.0f})")
    print('| field | ' + ' | '.join(vs) + ' |')
    print('|---|' + '---|' * len(vs))
    for f in fields:
        row = []
        b = med([r[f] for r in recs if r['variant'] == base and r.get(f) is not None])
        for v in vs:
            x = [r[f] for r in recs if r['variant'] == v and r.get(f) is not None]
            if not x: row.append('-'); continue
            m = med(x)
            s = f'{m:.4g} [{min(x):.4g}..{max(x):.4g}]'
            if v != base and b == b and b != 0:
                s += f' ({(m-b)/b*100:+.1f} %)'
            row.append(s)
        print(f'| {f} | ' + ' | '.join(row) + ' |')
    extra = [r for r in recs if r.get('cold_valid') not in (None, 'n/a')]
    if extra:
        print('cold_valid: ' + ', '.join(f"{r['variant']}:{r['cold_valid']}({r['resident_max']:.4f})" for r in extra))
    if any(r.get('miss', 0) for r in recs):
        print('MISSES: ' + ', '.join(f"{r['variant']}:{r['miss']}" for r in recs if r.get('miss')))
    print()
def noise():
    recs = load('noise')
    if not recs: print('noise: not run'); return {}
    out = {}
    for f in ['wall_s', 'cpu_s']:
        x = [r[f] for r in recs]
        h1, h2 = x[:len(x)//2], x[len(x)//2:]
        a = med(x)
        n = abs(med(h1) - med(h2)) / a
        # also V1 vs V1b
        v1 = med([r[f] for r in recs if r['variant'] == 'V1']); v1b = med([r[f] for r in recs if r['variant'] == 'V1b'])
        out[f] = n
        print(f'noise N ({f}): halves {n*100:.1f} %, V1 vs V1b {abs(v1-v1b)/a*100:.1f} %  (runs {len(x)})')
    print()
    return out
noise()
G = ['wall_s', 'cpu_s', 'user_s', 'sys_s', 'cpu_us_per_op', 'wall_us_per_op', 'mib_per_s_wall', 'minflt', 'majflt', 'miss']
for n in ['bb2_rand_random', 'bb2_z2_random', 'bb2_rand_seq', 'bb2_z2_seq', 'bb3_warm_fresh', 'bb3_cold']:
    table(n, G, 'V1')
for n in ['bb1_random', 'bb1_offset']:
    table(n, ['wall_s', 'cpu_s', 'user_s', 'sys_s', 'cpu_us_per_op', 'minflt', 'majflt'], 'pread')
for t in [1, 4, 16]:
    for s in ['', '_gate', '_lookup']:
        table(f'bb4_t{t}{s}', ['ops_per_s', 'cpu_us_per_op', 'wall_s', 'cpu_s', 'miss'], 'V1')
for n in ['bb5_random', 'bb5_seq']:
    table(n, G, 'V1M')
table('bb6', ['mapped', 'rss_open_mib', 'rss_pass_mib', 'rss_unmap_mib', 'footprint_open_mib', 'footprint_pass_mib', 'footprint_unmap_mib', 'miss'], 'V1')
table('bb7', ['objects', 'arenas', 'miss', 'first_miss', 'fds_before', 'fds_after', 'mapped', 'wall_s'], 'RS')
