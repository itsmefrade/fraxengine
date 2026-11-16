// Cross-origin isolation shim for hosts that cannot send response headers (GitHub Pages and similar).
// It re-serves every same-origin response with COOP/COEP/CORP added, so the page becomes
// crossOriginIsolated and SharedArrayBuffer works. Own implementation, no third-party code.
// Limits are listed in docs/deploy.md.
'use strict';
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));

self.addEventListener('fetch', e => {
  const r = e.request;
  // Chrome throws on this combination if it is passed to fetch().
  if (r.cache === 'only-if-cached' && r.mode !== 'same-origin') return;
  // Cross-origin requests are not touched: the media server decides (CORS / CORP), see the docs.
  if (new URL(r.url).origin !== self.location.origin) return;
  e.respondWith((async () => {
    let res;
    try { res = await fetch(r); } catch (err) { return new Response('coi-sw: ' + err, { status: 502 }); }
    if (res.status === 0 || res.type === 'opaque' || res.type === 'opaqueredirect') return res;
    const h = new Headers(res.headers);
    h.set('Cross-Origin-Opener-Policy', 'same-origin');
    h.set('Cross-Origin-Embedder-Policy', 'require-corp');
    h.set('Cross-Origin-Resource-Policy', 'same-origin');
    return new Response(res.body, { status: res.status, statusText: res.statusText, headers: h });
  })());
});
