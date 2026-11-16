# Deploying the player

The player is a static site. Nothing runs on the server; the server only has to send the right headers and
answer byte-range requests for media.

## 1. Produce the site

```sh
sh scripts/build.sh --dist
# same as: sh scripts/build.sh && sh web/dist-tools/build.sh --skip-rust
# --no-verify on web/dist-tools/build.sh skips the headless-Chrome smoke test (needs Chrome)
```

`web/dist-tools/assemble.py` starts at `web/player/index.html` and follows every `<script>`, `<link>`, ES
import, `new URL('x', import.meta.url)` and worker/worklet name; a reference to a missing file fails the build.
No source file is rewritten or minified. The result, `web/dist/`, is self-contained:

```
web/dist/
  index.html  coi.js  coi-sw.js  manifest.json        entry points (must not be cached long)
  _headers  vercel.json  .nojekyll  SOURCE.txt         host configs and notes (not loaded at run time)
  assets/<build id>/                                   player, engine, fel, exact, subs, wasm (immutable)
  legal/  source/                                      licences and the LGPL corresponding source
```

`<build id>` is a hash of every file's contents, so every asset URL is immutable and a new build lives
next to the old one. Copy the whole directory to the host. Keep older `assets/<id>/` directories for a while
so tabs that are still open keep working.

Serving `web/` itself (the development layout: `web/player/index.html` with `web/wasm/`, `web/exact/wasm/`
and the sources next to it) also works with the same headers; `python3 web/server.py --port 8790` does that
locally. `web/dist` is the layout meant for production.

## 2. Required headers

| What | Header | Why |
| --- | --- | --- |
| Every response of the player origin | `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` | cross-origin isolation, which `SharedArrayBuffer` needs (threaded HEVC entropy decode, audio ring buffers, synchronous worker reads) |
| Every response of the player origin | `Cross-Origin-Resource-Policy: same-origin` | resources stay loadable under COEP |
| `*.wasm` | `Content-Type: application/wasm` | `WebAssembly.compileStreaming` rejects any other type |
| `*.js`, `*.mjs` | `Content-Type: text/javascript` | module scripts and workers refuse other types |
| `/assets/*` | `Cache-Control: public, max-age=31536000, immutable` | content-addressed |
| `index.html`, `coi.js`, `coi-sw.js`, `manifest.json` | `Cache-Control: no-cache` | a stale `index.html` points at an old build id |
| all | HTTPS (or `localhost`) | service workers, secure context, mixed content |

Hosts that cannot set headers (GitHub Pages and similar): `coi.js` registers `coi-sw.js`, a service worker
that adds COOP/COEP, and reloads once. That needs HTTPS and a browser that keeps service workers; a hard reload
(Shift) bypasses it, and the page then says it is not cross-origin isolated.

Ready-made configs: `web/dist-tools/hosts/` (`_headers` for Netlify and Cloudflare Pages, `vercel.json`,
`nginx.conf.example`, `Caddyfile.example`).

## 3. Media sources

The player opens a local file (file picker or drag and drop, read in windows by `FileReaderSync`, never loaded
whole) or an HTTP(S) URL (`?src=<url-encoded URL>`). For a URL on a cross-origin isolated page it sends one open-ended
range request (`Range: bytes=0-`):

- `206 Partial Content` with the size in `Content-Range` (or `Content-Length`): that request is the download. It fills a
  64 MiB read-ahead ring (16 MiB kept behind the reader), the last megabyte of the file is fetched once on its own, and a seek
  outside the ring starts a new request at the target.
- otherwise the player probes with `Range: bytes=0-0`: a `206` there reads 4 MiB Range blocks (also the transport of a page
  that is not cross-origin isolated), anything else downloads the whole file first. Always serve media with Range support.

A media server on another origin must answer the player's CORS `fetch`:

| Header | Why |
| --- | --- |
| `Accept-Ranges: bytes`, `206` + `Content-Range` | seeking and partial reads |
| `Access-Control-Allow-Origin: <player origin>` (or `*`; no credentials are sent) | cross-origin `fetch` |
| `Access-Control-Expose-Headers: Content-Range, Content-Length, Accept-Ranges` | the total size is read from `Content-Range`; hidden, the player falls back to a full download |
| `Access-Control-Allow-Headers: Range` on `OPTIONS` | preflight (Chrome does not preflight a single `bytes=a-b` range, other browsers may) |
| `Cross-Origin-Resource-Policy: cross-origin` | not needed for the CORS fetches the player makes, harmless, future-proof |

Media on the same origin as the player needs none of the CORS headers, only Range support.

## 4. Caddy example

Caddy's `file_server` already answers Range requests and sends `application/wasm` and `text/javascript` for
`.wasm`, `.js` and `.mjs`. One site for the player and an optional media site:

```caddyfile
player.example.org {
	root * /srv/player             # contents of web/dist
	encode zstd gzip
	file_server

	header {
		Cross-Origin-Opener-Policy "same-origin"
		Cross-Origin-Embedder-Policy "require-corp"
		Cross-Origin-Resource-Policy "same-origin"
		X-Content-Type-Options "nosniff"
	}
	@wasm path *.wasm
	header @wasm Content-Type "application/wasm"
	@assets path /assets/*
	header @assets Cache-Control "public, max-age=31536000, immutable"
	@entry path / /index.html /coi.js /coi-sw.js /manifest.json
	header @entry Cache-Control "no-cache"
}

media.example.org {
	root * /srv/media
	file_server                    # Range and 206 built in; no encode: media is already compressed

	header {
		Access-Control-Allow-Origin "https://player.example.org"
		Access-Control-Expose-Headers "Content-Range, Content-Length, Accept-Ranges"
		Cross-Origin-Resource-Policy "cross-origin"
	}
	@preflight method OPTIONS
	handle @preflight {
		header Access-Control-Allow-Methods "GET, HEAD, OPTIONS"
		header Access-Control-Allow-Headers "Range"
		header Access-Control-Max-Age "86400"
		respond 204
	}
}
```

Then open `https://player.example.org/?src=https%3A%2F%2Fmedia.example.org%2Fmovie.mkv`.

## 5. Check a deployment

- `python3 web/dist-tools/serve.py --port 8800` serves `web/dist` locally with the production headers and Range
  (`--media-dir DIR --cors` adds a CORS media root, `--no-coi` exercises the service worker fallback).
- `python3 web/dist-tools/verify-dist.py --mode smoke` loads the site in headless Chrome and fails on any 404 or
  load error (needs Chrome).
- In the browser console of the deployed page: `crossOriginIsolated` must be `true`.

## 6. Browser support

Chrome (desktop) with WebGPU and WebCodecs. Dolby Vision, HDR10, HDR10+ and HLG output is developed and
measured on macOS; see the README for the format matrix.
