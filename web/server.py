#!/usr/bin/env python3
"""Development-only static server for the AetherWeb app (COOP/COEP for SharedArrayBuffer). No media processing."""
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlsplit
import argparse, json, datetime, hashlib, time
ROOT = Path(__file__).resolve().parent

class HarnessServer(ThreadingHTTPServer):
    # Video and audio workers fetch their module graphs concurrently.
    request_queue_size = 32

class Handler(SimpleHTTPRequestHandler):
    def __init__(self,*args,**kwargs): super().__init__(*args,directory=str(ROOT),**kwargs)
    def parse_request(self):
        if not super().parse_request(): return False
        port=self.server.server_address[1]
        if self.headers.get('Host') not in {f'127.0.0.1:{port}',f'localhost:{port}'}:
            self.send_error(403,'Localhost Host required'); return False
        return True
    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy','same-origin')
        self.send_header('Cross-Origin-Embedder-Policy','require-corp')
        self.send_header('Cross-Origin-Resource-Policy','same-origin')
        self.send_header('Cache-Control','no-store')
        super().end_headers()
    def send_head(self):
        # SimpleHTTPRequestHandler ignores Range; without this the player downloads whole files (507 MB for one MKV).
        rng=self.headers.get('Range','')
        path=Path(self.translate_path(self.path))
        if not rng.startswith('bytes=') or not path.is_file() or ',' in rng:
            return super().send_head()
        size=path.stat().st_size
        start_s,_,end_s=rng[6:].partition('-')
        try:
            if start_s: start=int(start_s); end=int(end_s) if end_s else size-1
            else: start=max(0,size-int(end_s)); end=size-1
        except ValueError: return super().send_head()
        if start>=size or start>end:
            self.send_response(416); self.send_header('Content-Range',f'bytes */{size}'); self.send_header('Content-Length','0'); self.end_headers(); return None
        end=min(end,size-1)
        f=open(path,'rb'); f.seek(start)
        self.send_response(206)
        self.send_header('Content-Type',self.guess_type(str(path)))
        self.send_header('Accept-Ranges','bytes')
        self.send_header('Content-Range',f'bytes {start}-{end}/{size}')
        self.send_header('Content-Length',str(end-start+1))
        self.end_headers()
        self.range_left=end-start+1
        return f
    def copyfile(self, source, outputfile):
        left=getattr(self,'range_left',None)
        if left is None: return super().copyfile(source,outputfile)
        while left>0:
            chunk=source.read(min(1<<20,left))
            if not chunk: break
            outputfile.write(chunk); left-=len(chunk)
        self.range_left=None
    def do_POST(self):
        # Local evidence sink for runs that execute without a debugger attached.
        if self.headers.get('Origin','') != 'http://'+self.headers.get('Host','') or urlsplit(self.path).path != '/results':
            self.send_error(403); return
        length=int(self.headers.get('Content-Length','0') or 0)
        if not 0<length<8_000_000: self.send_error(413); return
        data=json.loads(self.rfile.read(length))
        name=datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')+'.json'
        out=ROOT/'results'/name; out.parent.mkdir(exist_ok=True)
        out.write_text(json.dumps(data,indent=2)+'\n')
        self.send_response(201); self.send_header('Content-Length','0'); self.end_headers()
if __name__=='__main__':
    parser=argparse.ArgumentParser(); parser.add_argument('--port',type=int,default=8790); args=parser.parse_args()
    print(f'Static harness: http://127.0.0.1:{args.port}',flush=True)
    server=HarnessServer(('127.0.0.1',args.port),Handler)
    server.serve_forever()

