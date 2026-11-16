#!/usr/bin/env python3
"""Static server for web/dist with the headers a production host must send, plus HTTP Range.
  serve.py [--port 8800] [--root ../dist] [--no-coi] [--media-dir DIR] [--cors] [--log FILE]
--no-coi        send no COOP/COEP (to exercise the service-worker fallback)
--media-dir     serve DIR under /media/ (Range supported, like a media server)
--cors          media answers cross-origin requests the way the docs require (ACAO, expose headers, preflight)
--log FILE      append one JSON line per request (path, status): the dynamic import check reads this
Port 0 picks a free port; the chosen one is printed as 'serving http://127.0.0.1:PORT'."""
import argparse, json, mimetypes, os, re, sys, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import unquote, urlsplit

mimetypes.add_type('application/wasm', '.wasm')
mimetypes.add_type('text/javascript', '.mjs')
mimetypes.add_type('text/javascript', '.js')

class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass
    def _resolve(self):
        path = unquote(urlsplit(self.path).path)
        if path.startswith('/media/') and self.server.media:
            base, rel, media = self.server.media, path[len('/media/'):], True
        else:
            base, rel, media = self.server.root, path.lstrip('/') or 'index.html', False
        if rel.endswith('/'): rel += 'index.html'
        p = (base / rel).resolve()
        if base.resolve() not in p.parents and p != base.resolve(): return None, media
        return (p if p.is_file() else None), media
    def _headers(self, media, extra=None):
        s = self.server
        if s.coi and not media:
            self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
            self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
            self.send_header('Cross-Origin-Resource-Policy', 'same-origin')
        if media and s.cors:
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Access-Control-Expose-Headers', 'Content-Range, Content-Length, Accept-Ranges')
            self.send_header('Cross-Origin-Resource-Policy', 'cross-origin')
        for k, v in (extra or {}).items(): self.send_header(k, v)
    def _note(self, status):
        if self.server.log:
            with self.server.lock, open(self.server.log, 'a') as f:
                f.write(json.dumps({'path': urlsplit(self.path).path, 'status': status, 'method': self.command}) + '\n')
    def do_OPTIONS(self):
        _, media = self._resolve()
        self.send_response(204)
        if media and self.server.cors:
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Access-Control-Allow-Headers', 'Range')
            self.send_header('Access-Control-Allow-Methods', 'GET, HEAD, OPTIONS')
            self.send_header('Access-Control-Max-Age', '600')
        self.send_header('Content-Length', '0'); self.end_headers(); self._note(204)
    def do_HEAD(self): self._serve(False)
    def do_GET(self): self._serve(True)
    def _serve(self, body):
        p, media = self._resolve()
        if p is None:
            msg = b'not found'
            self.send_response(404); self._headers(media); self.send_header('Content-Length', str(len(msg))); self.end_headers()
            if body: self.wfile.write(msg)
            self._note(404); return
        size = p.stat().st_size
        ctype = mimetypes.guess_type(p.name)[0] or 'application/octet-stream'
        start, end, status = 0, size - 1, 200
        m = re.fullmatch(r'bytes=(\d*)-(\d*)', self.headers.get('Range', '').strip())
        if m and (m.group(1) or m.group(2)):
            if m.group(1):
                start = int(m.group(1)); end = min(int(m.group(2)), size - 1) if m.group(2) else size - 1
            else:
                start = max(0, size - int(m.group(2)))
            if start >= size:
                self.send_response(416); self._headers(media, {'Content-Range': f'bytes */{size}'}); self.send_header('Content-Length', '0'); self.end_headers()
                self._note(416); return
            status = 206
        n = end - start + 1
        self.send_response(status)
        self.send_header('Content-Type', ctype); self.send_header('Accept-Ranges', 'bytes'); self.send_header('Content-Length', str(n))
        if status == 206: self.send_header('Content-Range', f'bytes {start}-{end}/{size}')
        immutable = '/assets/' in self.path
        self.send_header('Cache-Control', 'public, max-age=31536000, immutable' if immutable else 'no-cache')
        self._headers(media); self.end_headers()
        if body:
            try:
                with open(p, 'rb') as f:
                    f.seek(start); left = n
                    while left > 0:
                        chunk = f.read(min(1 << 20, left))
                        if not chunk: break
                        self.wfile.write(chunk); left -= len(chunk)
            except (BrokenPipeError, ConnectionResetError): pass
        self._note(status)

class Server(ThreadingHTTPServer):
    request_queue_size = 64
    daemon_threads = True

def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--port', type=int, default=8800); ap.add_argument('--root', default=str(Path(__file__).resolve().parent.parent / 'dist'))
    ap.add_argument('--no-coi', action='store_true'); ap.add_argument('--media-dir'); ap.add_argument('--cors', action='store_true')
    ap.add_argument('--log'); ap.add_argument('--host', default='127.0.0.1')
    a = ap.parse_args()
    s = Server((a.host, a.port), Handler)
    s.root = Path(a.root).resolve(); s.coi = not a.no_coi; s.cors = a.cors; s.log = a.log; s.lock = threading.Lock()
    s.media = Path(a.media_dir).resolve() if a.media_dir else None
    print(f'serving http://{a.host}:{s.server_address[1]}  root={s.root}  coi={s.coi}  media={s.media}  cors={s.cors}', flush=True)
    try: s.serve_forever()
    except KeyboardInterrupt: pass

if __name__ == '__main__':
    main()
