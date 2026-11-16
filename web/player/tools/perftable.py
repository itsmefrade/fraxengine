#!/usr/bin/env python3
"""Markdown tables from perfrun.py result directories.
  perftable.py DIR [DIR ...] [--labels a,b,c]
Several directories are repeats of the same runs: counts are shown per run (a/b/c), times and CPU as the median with the spread
(min to max) behind it."""
import json, sys, os, statistics

def load(dirs):
    runs = {}
    for d in dirs:
        for f in sorted(os.listdir(d)):
            if not f.endswith('.json'): continue
            try: j = json.load(open(os.path.join(d, f)))
            except Exception: continue
            if 'label' in j: runs.setdefault(j['label'], []).append(j)
    return runs

def med(xs, nd=0):
    xs = [x for x in xs if x is not None]
    if not xs: return 'n/a'
    m = statistics.median(xs)
    f = (lambda v: str(round(v))) if nd == 0 else (lambda v: f'{v:.{nd}f}')
    return f(m) if len(xs) == 1 or min(xs) == max(xs) else f'{f(m)} ({f(min(xs))} to {f(max(xs))})'

def each(xs):
    return '/'.join('n/a' if x is None else str(x) for x in xs)

def g(d, *path):
    for p in path:
        if d is None: return None
        d = d.get(p) if isinstance(d, dict) else None
    return d

def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    order = None
    for a in sys.argv[1:]:
        if a.startswith('--labels='): order = a.split('=', 1)[1].split(',')
    runs = load(args)
    labels = order or list(runs)
    print('| Sample | Route | Start: playing / first frame ms | Seek near ms | Seek far ms | Shown | Dropped | A/V p95 ms | Underruns | Main max gap ms |')
    print('|---|---|---|---|---|---|---|---|---|---|')
    for l in labels:
        rs = [r for r in runs.get(l, []) if r.get('result')]
        if not rs: print(f'| {l} | no result ({each([r.get("status") for r in runs.get(l, [])])}) |||||||||'); continue
        res = [r['result'] for r in rs]
        route = g(res[0], 'route', 'video') or '?'
        q = g(res[0], 'route', 'quality')
        aud = (g(res[0], 'route', 'audio') or '').split(' -> ')[0][:46]
        near = [s.get('frameMs') for r in res for s in r.get('seeks') or [] if s.get('kind') == 'near']
        far = [s.get('frameMs') for r in res for s in r.get('seeks') or [] if s.get('kind') == 'far']
        bad = sum(1 for r in res for s in r.get('seeks') or [] if not s.get('ok'))
        print(f"| {l} | {route} ({q}); {aud} | {med([r.get('playingMs') for r in res])} / {med([r.get('firstFrameMs') for r in res])} | {med(near)} | {med(far)}{' ; ' + str(bad) + ' failed' if bad else ''} | "
              f"{each([g(r, 'steady', 'shown') for r in res])} | {each([g(r, 'steady', 'dropped') for r in res])} | {med([g(r, 'steady', 'av', 'p95Ms') for r in res])} | "
              f"{each([g(r, 'steady', 'audio', 'underruns') for r in res])} | {med([g(r, 'steady', 'mainThread', 'maxGapMs') for r in res])} |")
    print()
    print('| Sample | Load | CPU % page renderer | CPU % GPU process | CPU % browser | CPU % Chrome total | RSS MB renderer at 10 s / end | RSS MB GPU at 10 s / end | RSS MB Chrome total end | JS+WASM MB at 10 s / end |')
    print('|---|---|---|---|---|---|---|---|---|---|')
    for l in labels:
        rs = [r for r in runs.get(l, []) if r.get('result') and r.get('steady')]
        if not rs: continue
        c = lambda k: med([g(r, 'steady', 'cpuPct', k) for r in rs], 1)
        m = lambda w, k: med([g(r, w, 'rssMB', k) for r in rs])
        print(f"| {l} | {med([r['env']['load'][0] for r in rs], 1)} | {c('pageRenderer')} | {c('gpu')} | {c('browser')} | {c('chromeTotal')} | "
              f"{m('first10', 'pageRenderer')} / {m('steady', 'pageRenderer')} | {m('first10', 'gpu')} / {m('steady', 'gpu')} | {m('steady', 'chromeTotal')} | "
              f"{med([g(r, 'result', 'steady', 'memAt10', 'mem', 'sumMB') for r in rs])} / {med([g(r, 'result', 'steady', 'memEnd', 'mem', 'sumMB') for r in rs])} |")

main()
