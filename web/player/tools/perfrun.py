#!/usr/bin/env python3
"""Run web/player/perf.html in a Chrome of its own (own --user-data-dir, no debugger attached) and join the page's result with
process CPU time and RSS sampled from outside.
  perfrun.py --label h264 --query "src=fixtures/zz-h264-1080p.mp4&secs=60" [--port 8796] [--headless] [--out DIR]
Starts web/server.py on --port when nothing listens there. The page POSTs its result to /results (web/results/*.json); this script
waits for it, then prints one JSON line and writes <out>/<label>.json.
CPU is percent of one core, from cumulative process CPU time (ps time=) over the page's steady window, per Chrome process kind
(browser, renderer, gpu, utility) plus WindowServer and coreaudiod (system-wide, so other apps count there too)."""
import argparse, ctypes, ctypes.util, json, os, shutil, signal, socket, subprocess, sys, time, glob
from pathlib import Path

CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
WEB = Path(__file__).resolve().parents[2]

def cpu_s(t):
    # ps time= is [[dd-]hh:]mm:ss.cc
    days = 0
    if '-' in t: d, t = t.split('-', 1); days = int(d)
    p = [float(x) for x in t.split(':')]
    while len(p) < 3: p.insert(0, 0.0)
    return days * 86400 + p[0] * 3600 + p[1] * 60 + p[2]

# Per-thread CPU time from libproc: the only outside view that splits a renderer into its workers, the media threads and the
# main thread. Chrome names its threads; all workers share one name, so they carry their creation order.
_proc = ctypes.CDLL(ctypes.util.find_library('proc'))
class _TI(ctypes.Structure):
    _fields_ = [('user', ctypes.c_uint64), ('system', ctypes.c_uint64), ('pad', ctypes.c_int32 * 8), ('name', ctypes.c_char * 64)]

def threads(pid):
    buf = (ctypes.c_uint64 * 1024)(); out = {}
    n = _proc.proc_pidinfo(pid, 28, ctypes.c_uint64(0), buf, ctypes.sizeof(buf))
    for h in buf[:max(n, 0) // 8]:
        ti = _TI()
        if _proc.proc_pidinfo(pid, 15, ctypes.c_uint64(h), ctypes.byref(ti), ctypes.sizeof(ti)) > 0:
            out[str(h)] = (ti.name.decode('utf8', 'replace') or 'unnamed', (ti.user + ti.system) / 1e9)
    return out

def thread_window(samples, a, b, top=40):
    w = [s for s in samples if a <= s['wall'] <= b and s.get('thr')]
    if len(w) < 2: return None
    first, last = w[0], w[-1]; dt = (last['wall'] - first['wall']) / 1000; rows = []
    for key, th in last['thr'].items():
        for h, (name, t) in th.items():
            t0 = first['thr'].get(key, {}).get(h, (name, 0.0))[1]
            rows.append((round((t - t0) * 100 / dt, 1), key.split(':')[0], name, int(h)))
    # thread ids rise with creation, so workers can be told apart by the order the page created them in
    order = {h: i + 1 for i, h in enumerate(sorted(r[3] for r in rows if r[2] == 'DedicatedWorker thread'))}
    rows = [(r[0], r[1], r[2] + (f' #{order[r[3]]}' if r[3] in order else '')) for r in rows]
    rows.sort(reverse=True)
    return [{'pct': r[0], 'proc': r[1], 'thread': r[2]} for r in rows[:top] if r[0] >= 0.1]

def sample(root, thr=None):
    out = subprocess.run(['ps', '-axo', 'pid=,ppid=,rss=,time=,command='], capture_output=True, text=True).stdout.splitlines()
    rows = {}; kids = {}; sysp = {}
    for l in out:
        p = l.split(None, 4)
        if len(p) < 5: continue
        pid, ppid, rss, t, cmd = int(p[0]), int(p[1]), int(p[2]), cpu_s(p[3]), p[4]
        rows[pid] = (rss, t, cmd); kids.setdefault(ppid, []).append(pid)
        if cmd.endswith('/WindowServer -daemon') or '/WindowServer' in cmd.split(' ')[0]: sysp['windowserver'] = (rss, t)
        elif cmd.split(' ')[0].endswith('/coreaudiod'): sysp['coreaudiod'] = (rss, t)
    tot = {}; stack = [root]; seen = set()
    while stack:
        p = stack.pop()
        if p in seen or p not in rows: continue
        seen.add(p)
        rss, t, cmd = rows[p]
        kind = 'renderer' if '--type=renderer' in cmd else 'gpu' if '--type=gpu-process' in cmd else 'utility' if '--type=' in cmd else 'browser'
        a = tot.setdefault(kind, [0, 0.0, 0]); a[0] += rss; a[1] += t; a[2] = max(a[2], rss)
        if kind == 'renderer': tot[f'renderer:{p}'] = [rss, t, rss]
        if thr is not None: thr[f'{kind}:{p}'] = threads(p)
        stack.extend(kids.get(p, []))
    for k, (rss, t) in sysp.items(): tot[k] = [rss, t, rss]
    return tot

def window(samples, a, b):
    """CPU percent and RSS between wall times a and b (ms)."""
    w = [s for s in samples if a <= s['wall'] <= b]
    if len(w) < 2: return None
    first, last = w[0], w[-1]; dt = (last['wall'] - first['wall']) / 1000
    r = {'seconds': round(dt, 1), 'cpuPct': {}, 'rssMB': {}, 'rssStartMB': {}}
    for k in last['p']:
        if k not in first['p']: continue
        r['cpuPct'][k] = round((last['p'][k][1] - first['p'][k][1]) * 100 / dt, 1)
        r['rssMB'][k] = round(last['p'][k][0] / 1024); r['rssStartMB'][k] = round(first['p'][k][0] / 1024)
    # the page's renderer is the busiest one (a fresh profile also runs a spare renderer and the new tab page's)
    per = [k for k in r['cpuPct'] if k.startswith('renderer:')]
    if per:
        page = max(per, key=lambda k: (r['cpuPct'][k], r['rssMB'][k]))
        for d in (r['cpuPct'], r['rssMB'], r['rssStartMB']):
            d['pageRenderer'] = d[page]
            for k in per: del d[k]
    chrome = [k for k in r['cpuPct'] if k not in ('windowserver', 'coreaudiod', 'pageRenderer')]
    r['cpuPct']['chromeTotal'] = round(sum(r['cpuPct'][k] for k in chrome), 1)
    r['rssMB']['chromeTotal'] = sum(r['rssMB'][k] for k in chrome)
    return r

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--label', required=True); ap.add_argument('--query', required=True)
    ap.add_argument('--port', type=int, default=8796); ap.add_argument('--timeout', type=float, default=420)
    ap.add_argument('--out', default=str(WEB / 'results' / 'perf')); ap.add_argument('--headless', action='store_true')
    ap.add_argument('--profiles', default='/tmp'); ap.add_argument('--extra', default=''); ap.add_argument('--page', default='perf.html')
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    server = None
    with socket.socket() as so:
        if so.connect_ex(('127.0.0.1', a.port)) != 0:
            server = subprocess.Popen([sys.executable, str(WEB / 'server.py'), '--port', str(a.port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.8)
    env = {'load': os.getloadavg(), 'simulator': subprocess.run(['pgrep', '-x', 'Simulator'], capture_output=True).returncode == 0,
           'lidClosed': 'Yes' in subprocess.run('ioreg -r -k AppleClamshellState | grep AppleClamshellState', shell=True, capture_output=True, text=True).stdout,
           'headless': a.headless}
    udd = f'{a.profiles}/frax-perf-chrome-{a.label}'
    shutil.rmtree(udd, ignore_errors=True)
    url = f'http://127.0.0.1:{a.port}/player/{a.page}?label={a.label}&{a.query}'
    flags = [CHROME, f'--user-data-dir={udd}', '--no-first-run', '--no-default-browser-check', '--autoplay-policy=no-user-gesture-required',
             '--mute-audio', '--enable-precise-memory-info', '--window-size=1280,860', '--window-position=60,60',
             '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows']
    if a.headless: flags.append('--headless=new')
    if a.extra: flags += a.extra.split()
    before = set(glob.glob(str(WEB / 'results' / '*.json')))
    proc = subprocess.Popen(flags + [url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    samples = []; res = None; status = 'timeout'; start = time.time()
    try:
        while time.time() - start < a.timeout:
            time.sleep(1.0)
            if proc.poll() is not None: status = 'chrome-exited'; break
            thr = {}; p = sample(proc.pid, thr)
            samples.append({'wall': time.time() * 1000, 'p': p, 'thr': thr})
            for f in set(glob.glob(str(WEB / 'results' / '*.json'))) - before:
                try: d = json.load(open(f))
                except Exception: continue
                before.add(f)
                if d.get('test') == 'perf' and d.get('result', {}).get('label') == a.label: res = d['result']; os.remove(f)
            if res: status = 'done'; break
    finally:
        try: os.killpg(proc.pid, signal.SIGKILL)
        except Exception: proc.kill()
        shutil.rmtree(udd, ignore_errors=True)
        if server: server.terminate()
    env['loadEnd'] = os.getloadavg()
    summ = {'label': a.label, 'url': url, 'status': status, 'env': env, 'result': res}
    if res and res.get('wall', {}).get('steadyStart') and res['wall'].get('steadyEnd'):
        w = res['wall']
        summ['steady'] = window(samples, w['steadyStart'] + 3000, w['steadyEnd'])
        summ['threads'] = thread_window(samples, w['steadyStart'] + 3000, w['steadyEnd'])
        summ['first10'] = window(samples, w['steadyStart'], w['steadyStart'] + 10500)
        if w.get('seeksStart') and w.get('seeksEnd'): summ['seeks'] = window(samples, w['seeksStart'], w['seeksEnd'])
    summ['rssSeries'] = [{'t': round((s['wall'] / 1000 - start), 1), **{k: round(v[0] / 1024) for k, v in s['p'].items() if ':' not in k}, 'rendererMax': round(max([v[0] for k, v in s['p'].items() if k.startswith('renderer:')] or [0]) / 1024)} for s in samples[::5]]
    json.dump(summ, open(f'{a.out}/{a.label}.json', 'w'), indent=1)
    st = (res or {}).get('steady') or {}
    print(json.dumps({'label': a.label, 'status': status, 'load': [round(x, 1) for x in env['load']], 'route': (res or {}).get('route', {}).get('video'),
                      'ttffMs': (res or {}).get('firstFrameMs'), 'shown': st.get('shown'), 'dropped': st.get('dropped'),
                      'cpu': (summ.get('steady') or {}).get('cpuPct'), 'rss': (summ.get('steady') or {}).get('rssMB'), 'errors': (res or {}).get('errors')}))
    return 0 if status == 'done' else 1

sys.exit(main())
