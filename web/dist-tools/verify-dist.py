#!/usr/bin/env python3
"""Load the built site (web/dist) in a headless Chrome of its own and check it.
Needs the `websockets` package (pip install websockets). Chrome: --headless=new, own --user-data-dir under /tmp.

  verify-dist.py --mode smoke                         # build.sh: load, play a few seconds, fail on any 404 / error
  verify-dist.py --mode full --server plain --seconds 20 --media clip.mp4
                  # plain python3 -m http.server, NO COOP/COEP: isolation must come from coi-sw.js
  verify-dist.py --mode full --server serve --seconds 20 --media clip.mp4 [--cross-origin-media]
                  # dist-tools/serve.py with COOP/COEP (and media on a second origin with CORS)
  verify-dist.py --mode full --server webpy --seconds 20 --media-rel player/fixtures/x.mp4
                  # web/server.py (serves web/, so the site is at /dist/)
  add --source file to feed the media through the file input instead of a URL (local file route)
Exit code 0 only if every check passed. Prints one JSON summary."""
import argparse, asyncio, json, os, re, shutil, socket, subprocess, sys, tempfile, time, urllib.parse, urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
WEB = HERE.parent
DIST = WEB / 'dist'
CHROME = os.environ.get('CHROME', '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome')
FIXTURES = WEB / 'player' / 'fixtures'
AUTO_MEDIA = ['fel-bl-hdr10-aac.mp4', 'h264-aac.mp4', 'hevc-main10.mp4', 'hevc-annexb.ts']

def free_port():
    s = socket.socket(); s.bind(('127.0.0.1', 0)); p = s.getsockname()[1]; s.close(); return p

def wait_http(url, timeout=15):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            urllib.request.urlopen(url, timeout=2).read(1); return True
        except urllib.error.HTTPError: return True
        except Exception: time.sleep(0.2)
    return False

class Cdp:
    def __init__(self, ws): self.ws = ws; self.id = 0; self.pending = {}; self.events = []; self.task = asyncio.create_task(self._read())
    async def _read(self):
        try:
            async for raw in self.ws:
                m = json.loads(raw)
                if 'id' in m and m['id'] in self.pending: self.pending.pop(m['id']).set_result(m)
                else: self.events.append(m)
        except Exception: pass
    async def call(self, method, params=None, timeout=60):
        self.id += 1; i = self.id; fut = asyncio.get_running_loop().create_future(); self.pending[i] = fut
        await self.ws.send(json.dumps({'id': i, 'method': method, 'params': params or {}}))
        m = await asyncio.wait_for(fut, timeout)
        if 'error' in m: raise RuntimeError(f'{method}: {m["error"]}')
        return m.get('result', {})
    async def ev(self, expr):
        try:
            r = await self.call('Runtime.evaluate', {'expression': expr, 'returnByValue': True, 'awaitPromise': True}, timeout=15)
        except Exception: return None
        return r.get('result', {}).get('value')

PROBE = """(() => { const s = window.session; let snap = null; try { snap = s && s.snapshot(); } catch (e) {}
 const v = snap && snap.video;
 return { iso: self.crossOriginIsolated, sab: typeof SharedArrayBuffer, sw: !!navigator.serviceWorker && !!navigator.serviceWorker.controller,
   loaded: typeof window.playerLoad === 'function', state: snap && snap.state, t: snap ? snap.timeUs : null, dur: snap ? snap.durationUs : null,
   shown: v ? v.shown : null, dropped: v ? v.dropped : null, derr: v ? v.decodeErrors : null, err: snap && snap.error, route: snap && snap.audioRoute,
   decoder: v && v.decoder ? v.decoder.codec + ' ' + v.decoder.hardwareAcceleration : null, unsupported: snap && snap.unsupported, srcMode: snap && snap.source && snap.source.mode, status: (document.getElementById('status')||{}).textContent }; })()"""

async def run(a):
    res = {'mode': a.mode, 'server': a.server, 'source': a.source, 'checks': {}, 'notes': []}
    tmp = Path(tempfile.mkdtemp(prefix='aether-dist-verify-'))
    procs = []
    udd = Path(f'/tmp/aether-dist-chrome-{os.getpid()}')
    shutil.rmtree(udd, ignore_errors=True)
    try:
        manifest = json.loads((DIST / 'manifest.json').read_text())
        media = Path(a.media).resolve() if a.media else None
        if a.media_rel: media = (WEB / a.media_rel).resolve()
        if media is None and a.mode == 'smoke':
            for n in AUTO_MEDIA:
                if (FIXTURES / n).is_file(): media = (FIXTURES / n).resolve(); break
        if media is not None and not media.is_file(): raise SystemExit(f'media not found: {media}')
        res['media'] = str(media) if media else None
        logf = tmp / 'requests.jsonl'
        # ---- server
        port = free_port(); page_path = '/index.html'; media_url = None; cross = None
        if a.server == 'serve':
            cmd = [sys.executable, str(HERE / 'serve.py'), '--port', str(port), '--log', str(logf)]
            if media and not a.cross_origin_media: cmd += ['--media-dir', str(media.parent), '--cors']
            procs.append(subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True))
            if media: media_url = f'/media/{media.name}'
            if media and a.cross_origin_media:
                cross = free_port()
                procs.append(subprocess.Popen([sys.executable, str(HERE / 'serve.py'), '--port', str(cross), '--root', str(tmp), '--no-coi',
                                               '--media-dir', str(media.parent), *([] if a.media_no_cors else ['--cors']), '--log', str(tmp / 'media.jsonl')], stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT))
                media_url = f'http://127.0.0.1:{cross}/media/{media.name}'
        elif a.server == 'plain':
            root = tmp / 'site'; root.mkdir()
            for p in DIST.iterdir():
                if p.name != '.gitignore': (root / p.name).symlink_to(p)
            if media:
                (root / 'media').mkdir(); (root / 'media' / media.name).symlink_to(media); media_url = f'/media/{media.name}'
            errf = open(tmp / 'plain.err', 'w')
            procs.append(subprocess.Popen([sys.executable, '-m', 'http.server', str(port), '--bind', '127.0.0.1', '--directory', str(root)], stdout=errf, stderr=errf))
            res['notes'].append('python3 -m http.server sends no COOP/COEP and ignores Range')
        elif a.server == 'webpy':
            procs.append(subprocess.Popen([sys.executable, str(WEB / 'server.py'), '--port', str(port)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT))
            page_path = '/dist/index.html'
            if media:
                if WEB not in media.parents: raise SystemExit('--server webpy serves web/ only: use --media-rel')
                media_url = '/' + media.relative_to(WEB).as_posix()
        if not wait_http(f'http://127.0.0.1:{port}{page_path}'): raise SystemExit('server did not start')
        base = f'http://127.0.0.1:{port}'
        # ---- chrome
        flags = [CHROME, '--headless=new', f'--user-data-dir={udd}', '--remote-debugging-port=0', '--no-first-run', '--no-default-browser-check',
                 '--autoplay-policy=no-user-gesture-required', '--mute-audio', '--window-size=1280,800', '--disable-background-timer-throttling',
                 '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows', 'about:blank']
        chrome = subprocess.Popen(flags, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True); procs.append(chrome)
        t0 = time.time(); cport = None
        while time.time() - t0 < 25 and not cport:
            try: cport = int((udd / 'DevToolsActivePort').read_text().split()[0])
            except Exception: await asyncio.sleep(0.2)
        if not cport: raise SystemExit('no devtools port')
        import websockets
        pages = [t for t in json.load(urllib.request.urlopen(f'http://127.0.0.1:{cport}/json/list')) if t['type'] == 'page']
        cdp = Cdp(await websockets.connect(pages[0]['webSocketDebuggerUrl'], max_size=None))
        for d in ('Runtime', 'Page', 'Log', 'DOM'): await cdp.call(d + '.enable')
        url = base + page_path
        if media_url and a.source == 'url': url += '?src=' + urllib.parse.quote(media_url, safe='')
        res['url'] = url
        nav_t = time.time()
        await cdp.call('Page.navigate', {'url': url})
        # isolation (the service worker route reloads once)
        iso = None
        for _ in range(100):
            await asyncio.sleep(0.3)
            p = await cdp.ev(PROBE)
            if p and p.get('iso') is not None and p['loaded']: iso = p; break
        res['isolation'] = {'crossOriginIsolated': bool(iso and iso['iso']), 'SharedArrayBuffer': iso and iso['sab'],
                            'controlledByServiceWorker': bool(iso and iso['sw']), 'secondsToReady': round(time.time() - nav_t, 1)}
        res['checks']['crossOriginIsolated'] = bool(iso and iso['iso'])
        if a.server == 'plain': res['checks']['isolation came from the service worker'] = bool(iso and iso['sw'] and iso['iso'])
        # ---- source
        if media and a.source == 'file':
            doc = await cdp.call('DOM.getDocument', {'depth': 0})
            q = await cdp.call('DOM.querySelector', {'nodeId': doc['root']['nodeId'], 'selector': '#file'})
            await cdp.call('DOM.setFileInputFiles', {'nodeId': q['nodeId'], 'files': [str(media)]})
        # ---- play
        samples = []; first_play = None; t_start = None
        if media:
            deadline = time.time() + 90
            while time.time() < deadline:
                p = await cdp.ev(PROBE)
                if p and p['state'] in ('playing', 'ended'): first_play = p; break
                if p and p['state'] == 'error': break
                await asyncio.sleep(0.3)
            if first_play and a.quality:
                await cdp.ev(f"(() => {{ const q = document.getElementById('quality'); q.value = '{a.quality}'; q.dispatchEvent(new Event('change')); }})()")
                await asyncio.sleep(3)
                first_play = None
                for _ in range(200):
                    p = await cdp.ev(PROBE)
                    if p and p['state'] in ('playing', 'ended'): first_play = p; break
                    await asyncio.sleep(0.3)
            if first_play:
                t_start = time.time(); t_media0 = first_play['t'] or 0
                while time.time() - t_start < a.seconds:
                    await asyncio.sleep(1); p = await cdp.ev(PROBE)
                    if p: samples.append({k: p.get(k) for k in ('state', 't', 'shown', 'dropped', 'derr')})
                    if p and p['state'] in ('error', 'ended'): break
            last = (await cdp.ev(PROBE)) or {}
            adv = ((last.get('t') or 0) - (first_play['t'] if first_play else 0)) / 1e6 if first_play else 0
            res['playback'] = {'requestedSeconds': a.seconds, 'wallSeconds': round(time.time() - t_start, 1) if t_start else 0, 'mediaSecondsAdvanced': round(adv, 2),
                               'finalState': last.get('state'), 'durationS': round((last.get('dur') or 0) / 1e6, 1), 'framesShown': last.get('shown'),
                               'framesDropped': last.get('dropped'), 'decodeErrors': last.get('derr'), 'decoder': last.get('decoder'), 'sourceMode': last.get('srcMode'), 'audioRoute': last.get('route'),
                               'error': last.get('err'), 'unsupported': last.get('unsupported')}
            dur = (last.get('dur') or 0) / 1e6
            need = min(a.seconds, max(dur - 1, 1)) * 0.8
            res['checks']['reached playing'] = first_play is not None
            res['checks'][f'media time advanced >= {need:.1f}s'] = adv >= need
            res['checks']['frames shown'] = (last.get('shown') or 0) > 0
            res['checks']['no player error'] = not last.get('err') and last.get('state') != 'error'
            res['checks']['no decode errors'] = not (last.get('derr') or 0)
        # ---- errors seen by the page
        errs = []
        for e in cdp.events:
            if e.get('method') == 'Runtime.exceptionThrown': errs.append(e['params']['exceptionDetails'].get('text', '') + ' ' + json.dumps(e['params']['exceptionDetails'].get('exception', {}).get('description', ''))[:300])
            elif e.get('method') == 'Log.entryAdded' and e['params']['entry']['level'] == 'error': errs.append(e['params']['entry']['text'] + ' ' + e['params']['entry'].get('url', ''))
        res['pageErrors'] = errs[:20]
        res['checks']['no page errors'] = not errs
        # ---- dynamic import check: what the browser really requested
        await asyncio.sleep(0.5)
        reqs = []
        if a.server == 'serve' and logf.exists(): reqs = [json.loads(l) for l in logf.read_text().splitlines() if l.strip()]
        elif a.server == 'plain':
            for l in (tmp / 'plain.err').read_text().splitlines():
                m = re.search(r'"(GET|HEAD) (\S+) HTTP/[\d.]+" (\d+)', l)
                if m: reqs.append({'path': m.group(2).split('?')[0], 'status': int(m.group(3)), 'method': m.group(1)})
        elif a.server == 'webpy': res['notes'].append('web/server.py keeps no access log; 404s are caught by the page-error and playback checks only')
        mj = tmp / 'media.jsonl'
        if mj.exists():
            ml = [json.loads(l) for l in mj.read_text().splitlines() if l.strip()]
            res['mediaOriginRequests'] = {'total': len(ml), 'methods': sorted({r['method'] for r in ml}), 'statuses': sorted({r['status'] for r in ml})}
        if reqs:
            bad = [r for r in reqs if r['status'] >= 400 and r['path'] != '/favicon.ico']
            res['checks']['no 4xx/5xx for any request'] = not bad
            res['badRequests'] = bad[:20]
            asked = {r['path'].lstrip('/') for r in reqs}
            if a.server == 'webpy': asked = {p.removeprefix('dist/') for p in asked}
            known = {e['path'] for e in manifest['files']}
            res['requestedNotInManifest'] = sorted(p for p in asked if p and p not in known and not p.startswith('media/') and p != 'favicon.ico')
            res['manifestNeverRequested'] = sorted(p for p in known if p.startswith('assets/') and p not in asked)
            res['checks']['every requested asset is in the manifest'] = not res['requestedNotInManifest']
            res['requestCount'] = len(reqs)
        await cdp.ws.close()
    finally:
        for p in procs:
            try:
                p.terminate(); p.wait(timeout=5)
            except Exception:
                try: p.kill()
                except Exception: pass
        shutil.rmtree(udd, ignore_errors=True); shutil.rmtree(tmp, ignore_errors=True)
    res['pass'] = all(res['checks'].values()) and bool(res['checks'])
    return res

def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--mode', choices=['smoke', 'full'], default='full')
    ap.add_argument('--server', choices=['serve', 'plain', 'webpy'], default='serve')
    ap.add_argument('--media'); ap.add_argument('--media-rel', help='path under web/ (for --server webpy)')
    ap.add_argument('--source', choices=['url', 'file'], default='url')
    ap.add_argument('--seconds', type=float, default=None)
    ap.add_argument('--cross-origin-media', action='store_true', help='serve the media from a second origin (CORS + CORP path)')
    ap.add_argument('--media-no-cors', action='store_true', help='with --cross-origin-media: the media origin sends no CORS headers (negative test, expect failure)')
    ap.add_argument('--quality', choices=['auto', 'native', 'fast', 'high', 'reference'], help='set the HDR quality select after the start (exercises the Dolby Vision paths)')
    ap.add_argument('--json-out')
    a = ap.parse_args()
    if a.seconds is None: a.seconds = 6 if a.mode == 'smoke' else 20
    if not (DIST / 'manifest.json').is_file(): print('web/dist missing: run build.sh first', file=sys.stderr); return 2
    if not Path(CHROME).exists(): print(f'Chrome not found at {CHROME} (set CHROME=...)', file=sys.stderr); return 2
    try: import websockets  # noqa
    except ImportError: print('pip install websockets', file=sys.stderr); return 2
    res = asyncio.run(run(a))
    out = json.dumps(res, indent=1)
    print(out)
    if a.json_out: Path(a.json_out).write_text(out + '\n')
    for k, v in res['checks'].items(): print(('PASS ' if v else 'FAIL ') + k, file=sys.stderr)
    return 0 if res['pass'] else 1

if __name__ == '__main__':
    sys.exit(main())
