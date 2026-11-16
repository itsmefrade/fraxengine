// Bootstrap: make sure the page is cross-origin isolated, then load the player.
//  - Already isolated (the host sends COOP/COEP): load the player at once.
//  - Not isolated: register coi-sw.js and reload once; the service worker then adds the headers.
//  - Still not isolated after that (no service worker support, hard reload, private mode): say so.
(() => {
  const me = document.currentScript;
  const app = me.dataset.app;
  const warn = text => {
    const d = document.createElement('div');
    d.style.cssText = 'position:fixed;inset:auto 0 0 0;padding:10px 14px;background:#7a1f1f;color:#fff;font:13px system-ui;z-index:9999';
    d.textContent = text; document.body.append(d);
  };
  const start = () => {
    const s = document.createElement('script');
    s.type = 'module'; s.src = app; document.head.append(s);
  };
  const KEY = 'aether-coi-reloaded';
  if (self.crossOriginIsolated) { try { sessionStorage.removeItem(KEY); } catch { /* ignore */ } start(); return; }
  let tried = false;
  try { tried = sessionStorage.getItem(KEY) === '1'; } catch { /* storage blocked */ }
  if (tried) {
    warn('Not cross-origin isolated (SharedArrayBuffer unavailable). Serve with COOP/COEP headers, or allow service workers and reload without Shift.');
    return;
  }
  if (!('serviceWorker' in navigator) || !self.isSecureContext) {
    warn('Needs HTTPS (or localhost) and service worker support, or a host that sends COOP/COEP headers.');
    return;
  }
  const swUrl = new URL('coi-sw.js', me.src);
  navigator.serviceWorker.register(swUrl.href).then(async reg => {
    const sw = reg.installing || reg.waiting || reg.active;
    if (sw && sw.state !== 'activated') {
      await new Promise(res => sw.addEventListener('statechange', () => sw.state === 'activated' && res()));
    }
    try { sessionStorage.setItem(KEY, '1'); } catch { /* storage blocked: the page then reloads at most once per load */ }
    location.reload();
  }).catch(err => warn('Service worker registration failed: ' + err));
})();
