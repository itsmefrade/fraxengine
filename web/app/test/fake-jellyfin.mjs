// SPDX-License-Identifier: LGPL-3.0-only
// A tiny fake Jellyfin for tests: the endpoints the FraxEngine client calls, one movie and one episode backed by a local media
// file (served with Range support), and the static web/ tree at the same origin with the isolation headers the player needs.
// A second "media host" on another port stands in for a third-party host that a server redirects streams to.
//
//   node web/app/test/fake-jellyfin.mjs [--port 8090] [--media path/to/file.mkv]
//
// Test users and passwords are fixtures, not secrets: demo / demo-pass.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const WEB_ROOT = path.resolve(HERE, '../..');
export const USERS = { demo: 'demo-pass' };
const MIME = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.wasm': 'application/wasm', '.svg': 'image/svg+xml', '.mp4': 'video/mp4', '.mkv': 'video/x-matroska', '.png': 'image/png' };

const json = (res, status, body, extra = {}) => { const b = Buffer.from(JSON.stringify(body)); res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': b.length, ...extra }); res.end(b); };

// Range-aware file response. `range: false` answers 200 with the whole body, like a server without Range support.
function sendFile(req, res, file, headers = {}, { range = true } = {}) {
  const size = fs.statSync(file).size;
  const type = MIME[path.extname(file)] || 'application/octet-stream';
  const m = range ? /^bytes=(\d*)-(\d*)$/.exec(req.headers.range || '') : null;
  if (!m) {
    res.writeHead(200, { ...headers, 'Content-Type': type, 'Content-Length': size, ...(range ? { 'Accept-Ranges': 'bytes' } : {}) });
    if (req.method === 'HEAD') return res.end();
    return fs.createReadStream(file).pipe(res);
  }
  let start = m[1] ? Number(m[1]) : Math.max(0, size - Number(m[2]));
  let end = m[1] && m[2] ? Math.min(Number(m[2]), size - 1) : size - 1;
  if (start >= size || start > end) { res.writeHead(416, { ...headers, 'Content-Range': `bytes */${size}` }); return res.end(); }
  res.writeHead(206, { ...headers, 'Content-Type': type, 'Content-Length': end - start + 1, 'Content-Range': `bytes ${start}-${end}/${size}`, 'Accept-Ranges': 'bytes' });
  if (req.method === 'HEAD') return res.end();
  fs.createReadStream(file, { start, end }).pipe(res);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const parts = []; req.on('data', c => parts.push(c)); req.on('error', reject);
    req.on('end', () => { const t = Buffer.concat(parts).toString('utf8'); try { resolve(t ? JSON.parse(t) : null); } catch { resolve(t); } });
  });
}

const authParams = header => {
  const m = /^MediaBrowser\s+(.*)$/i.exec(header || '');
  if (!m) return null;
  const out = {};
  for (const p of m[1].matchAll(/(\w+)="([^"]*)"/g)) out[p[1]] = p[2];
  return out;
};

const listen = (server, port) => new Promise((resolve, reject) => { server.once('error', reject); server.listen(port, '127.0.0.1', () => resolve(server.address().port)); });

/**
 * Starts the media host (another origin) and the fake Jellyfin.
 * media: the file every item plays. stream: 'direct' (served by the fake itself) or 'redirect' (302 to the media host).
 * mediaHost.cors / .range / .exposeContentRange toggle what the third-party host supports. All are mutable at run time via
 * `state.config` (tests and the smoke page change them through POST /__fake/config).
 */
export async function startFakeJellyfin({ port = 0, media, stream = 'direct', mediaHost = {} } = {}) {
  if (!media || !fs.existsSync(media)) throw new Error(`fake jellyfin: media file not found: ${media}`);
  const size = fs.statSync(media).size;
  const state = {
    config: { stream, cors: mediaHost.cors ?? true, range: mediaHost.range ?? true, exposeContentRange: mediaHost.exposeContentRange ?? true },
    tokens: new Map(),   // token -> {user, deviceId}
    requests: [],        // {method, url, auth: bool}
    mediaRequests: [],   // the media host: {url, range, authorization}
    events: [],          // playback reports
    userData: new Map(), // itemId -> {PlaybackPositionTicks, Played}
    results: [],         // smoke results POSTed by the test page
  };

  // ---------------------------------------------------------------- media host (a different origin)
  const mediaServer = http.createServer((req, res) => {
    const c = state.config;
    state.mediaRequests.push({ method: req.method, url: req.url, range: req.headers.range || null, authorization: req.headers.authorization || null });
    const cors = c.cors ? { 'Access-Control-Allow-Origin': '*', 'Access-Control-Allow-Headers': 'Range, Authorization', 'Access-Control-Allow-Methods': 'GET, HEAD, OPTIONS', ...(c.exposeContentRange ? { 'Access-Control-Expose-Headers': 'Content-Range, Content-Length, Accept-Ranges' } : {}) } : {};
    if (req.method === 'OPTIONS') { res.writeHead(204, cors); return res.end(); }
    if (!req.url.startsWith('/media/')) { res.writeHead(404, cors); return res.end(); }
    sendFile(req, res, media, { ...cors, 'Cross-Origin-Resource-Policy': 'cross-origin' }, { range: c.range });
  });
  const mediaPort = await listen(mediaServer, 0);
  const mediaOrigin = `http://127.0.0.1:${mediaPort}`;

  // ---------------------------------------------------------------- library
  const ms = (id) => ({
    Id: `ms-${id}`, Protocol: 'File', Container: path.extname(media).slice(1), Size: size, Name: path.basename(media), SupportsDirectPlay: true, SupportsDirectStream: true,
    DefaultAudioStreamIndex: 1, DefaultSubtitleStreamIndex: -1,
    MediaStreams: [
      { Index: 0, Type: 'Video', Codec: 'h264' },
      { Index: 1, Type: 'Audio', Codec: 'aac', Language: 'eng', IsDefault: true },
      { Index: 2, Type: 'Audio', Codec: 'eac3', Language: 'deu' },
      { Index: 3, Type: 'Subtitle', Codec: 'srt', IsExternal: true, Language: 'eng' },
    ],
  });
  const lib = {
    'view-movies': { Id: 'view-movies', Name: 'Movies', Type: 'CollectionFolder', CollectionType: 'movies', ImageTags: { Primary: 'p1' } },
    'view-shows': { Id: 'view-shows', Name: 'Shows', Type: 'CollectionFolder', CollectionType: 'tvshows', ImageTags: {} },
    'movie-1': { Id: 'movie-1', Name: 'Fixture Movie', Type: 'Movie', MediaType: 'Video', ParentId: 'view-movies', ProductionYear: 2026, RunTimeTicks: 600_000_000, Overview: 'A local fixture served by the fake server.', ImageTags: { Primary: 'p2' } },
    'series-1': { Id: 'series-1', Name: 'Fixture Show', Type: 'Series', ParentId: 'view-shows', ImageTags: { Primary: 'p3' } },
    'season-1': { Id: 'season-1', Name: 'Season 1', Type: 'Season', SeriesId: 'series-1', IndexNumber: 1, ParentId: 'series-1', ImageTags: {} },
    'episode-1': { Id: 'episode-1', Name: 'Pilot', Type: 'Episode', MediaType: 'Video', SeriesId: 'series-1', SeriesName: 'Fixture Show', SeasonId: 'season-1', ParentIndexNumber: 1, IndexNumber: 1, RunTimeTicks: 600_000_000, ImageTags: {}, SeriesPrimaryImageTag: 'p3' },
  };
  const withUser = item => ({ ...item, UserData: { PlaybackPositionTicks: 0, Played: false, ...(state.userData.get(item.Id) || {}) } });
  const page = items => ({ Items: items.map(withUser), TotalRecordCount: items.length, StartIndex: 0 });
  const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64');

  function api(req, res, url, body, user) {
    const p = url.pathname, q = url.searchParams, M = req.method;
    let m;
    if (M === 'GET' && (p === '/UserViews' || /^\/Users\/[^/]+\/Views$/.test(p))) return json(res, 200, page([lib['view-movies'], lib['view-shows']]));
    if (M === 'GET' && (p === '/UserItems/Resume' || /^\/Users\/[^/]+\/Items\/Resume$/.test(p))) {
      return json(res, 200, page(Object.values(lib).filter(i => (state.userData.get(i.Id)?.PlaybackPositionTicks || 0) > 0)));
    }
    if (M === 'GET' && p === '/Items') {
      const parent = q.get('ParentId') || q.get('parentId'), types = (q.get('IncludeItemTypes') || '').split(',').filter(Boolean), term = (q.get('SearchTerm') || q.get('searchTerm') || '').toLowerCase();
      let items = Object.values(lib).filter(i => i.Type !== 'CollectionFolder');
      if (parent) items = items.filter(i => i.ParentId === parent);
      if (types.length) items = items.filter(i => types.includes(i.Type));
      if (term) items = items.filter(i => i.Name.toLowerCase().includes(term));
      return json(res, 200, page(items));
    }
    if (M === 'GET' && (m = /^\/Shows\/([^/]+)\/Seasons$/.exec(p))) return json(res, 200, page(Object.values(lib).filter(i => i.Type === 'Season' && i.SeriesId === m[1])));
    if (M === 'GET' && (m = /^\/Shows\/([^/]+)\/Episodes$/.exec(p))) return json(res, 200, page(Object.values(lib).filter(i => i.Type === 'Episode' && i.SeriesId === m[1] && (!q.get('seasonId') || i.SeasonId === q.get('seasonId')))));
    if (M === 'GET' && ((m = /^\/Items\/([^/]+)$/.exec(p)) || (m = /^\/Users\/[^/]+\/Items\/([^/]+)$/.exec(p)))) {
      const it = lib[m[1]]; return it ? json(res, 200, { ...withUser(it), MediaSources: it.MediaType === 'Video' ? [ms(it.Id)] : undefined }) : json(res, 404, {});
    }
    if (M === 'POST' && (m = /^\/Items\/([^/]+)\/PlaybackInfo$/.exec(p))) {
      const it = lib[m[1]]; return it ? json(res, 200, { MediaSources: [ms(it.Id)], PlaySessionId: crypto.randomUUID() }) : json(res, 404, {});
    }
    if (M === 'GET' && (m = /^\/Videos\/([^/]+)\/stream$/.exec(p))) {
      if (!lib[m[1]] || q.get('mediaSourceId') !== `ms-${m[1]}`) return json(res, 404, {});
      if (state.config.stream === 'redirect') { res.writeHead(302, { Location: `${mediaOrigin}/media/${m[1]}?sig=${crypto.randomBytes(6).toString('hex')}` }); return res.end(); }
      return sendFile(req, res, media);
    }
    if (M === 'POST' && ['/Sessions/Playing', '/Sessions/Playing/Progress', '/Sessions/Playing/Stopped'].includes(p)) {
      state.events.push({ type: p.split('/').pop(), at: Date.now(), body });
      if (body?.ItemId) state.userData.set(body.ItemId, { PlaybackPositionTicks: p.endsWith('Stopped') && body.PositionTicks >= 0.9 * (lib[body.ItemId]?.RunTimeTicks || Infinity) ? 0 : body.PositionTicks || 0 });
      res.writeHead(204); return res.end();
    }
    if (M === 'POST' && p === '/Sessions/Logout') { state.tokens.delete(user.token); res.writeHead(204); return res.end(); }
    return json(res, 404, { error: `fake jellyfin: no route ${M} ${p}` });
  }

  // ---------------------------------------------------------------- the server: static web/ plus /jellyfin
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://x');
    const p = decodeURIComponent(url.pathname);
    // Test plumbing (not Jellyfin)
    if (p === '/__fake/state' && req.method === 'GET') return json(res, 200, { config: state.config, events: state.events, requests: state.requests, mediaRequests: state.mediaRequests, mediaOrigin });
    if (p === '/__fake/config' && req.method === 'POST') { Object.assign(state.config, await readBody(req) || {}); return json(res, 200, state.config); }
    if (p === '/results' && req.method === 'POST') { state.results.push(await readBody(req)); return json(res, 200, { ok: true }); }

    // Static web/ tree (the client is /app/); /jellyfin/ is the fake server, like the reverse proxy in production.
    const file = path.resolve(WEB_ROOT, '.' + (p.endsWith('/') ? p + 'index.html' : p));
    if (!p.startsWith('/jellyfin/') && file.startsWith(WEB_ROOT + path.sep) && fs.existsSync(file) && fs.statSync(file).isFile() && ['GET', 'HEAD'].includes(req.method)) {
      const isClientPage = /^\/app\/.*\.html$/.test(p.endsWith('/') ? p + 'index.html' : p);
      return sendFile(req, res, file, {
        'Cross-Origin-Opener-Policy': 'same-origin',
        // The client page loads posters from other origins (a remote server): credentialless lets no-cors images through.
        'Cross-Origin-Embedder-Policy': isClientPage ? 'credentialless' : 'require-corp',
        'Cross-Origin-Resource-Policy': 'same-origin', 'Cache-Control': 'no-store',
      });
    }
    if (!p.startsWith('/jellyfin/')) return json(res, 404, {});

    url.pathname = p.slice('/jellyfin'.length);
    const auth = authParams(req.headers.authorization);
    const user = auth?.Token ? state.tokens.get(auth.Token) : null;
    state.requests.push({ method: req.method, url: req.url, auth: !!user, client: auth?.Client || null, deviceId: auth?.DeviceId || null });
    const body = req.method === 'POST' ? await readBody(req) : null;

    if (url.pathname === '/System/Info/Public') return json(res, 200, { ServerName: 'Fake', Version: '12.4.0', ProductName: 'Jellyfin Server', Id: 'fake', StartupWizardCompleted: true });
    if (/^\/Items\/[^/]+\/Images\/\w+$/.test(url.pathname)) { res.writeHead(200, { 'Content-Type': 'image/png', 'Content-Length': PNG.length }); return res.end(PNG); }
    if (url.pathname === '/Users/AuthenticateByName' && req.method === 'POST') {
      if (!auth || !auth.Client || !auth.DeviceId || !auth.Device || !auth.Version) return json(res, 400, { error: 'MediaBrowser authorization with Client, Device, DeviceId and Version required' });
      const name = body?.Username;
      if (!name || USERS[name] !== body?.Pw) return json(res, 401, {});
      const token = crypto.randomBytes(16).toString('hex');
      state.tokens.set(token, { user: name, token, deviceId: auth.DeviceId });
      return json(res, 200, { AccessToken: token, User: { Id: `user-${name}`, Name: name }, ServerId: 'fake' });
    }
    if (!user) return json(res, 401, {});
    try { return api(req, res, url, body, user); } catch (e) { return json(res, 500, { error: String(e) }); }
  });
  const actualPort = await listen(server, port);
  const origin = `http://127.0.0.1:${actualPort}`;
  return {
    origin, serverUrl: `${origin}/jellyfin`, mediaOrigin, state,
    tokens: () => [...state.tokens.keys()],
    close: () => Promise.all([new Promise(r => server.close(r)), new Promise(r => mediaServer.close(r))]).then(() => { server.closeAllConnections?.(); mediaServer.closeAllConnections?.(); }),
  };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const arg = n => { const i = process.argv.indexOf(n); return i > 0 ? process.argv[i + 1] : null; };
  const media = arg('--media') || path.join(WEB_ROOT, 'player/fixtures/zz-2audio.mkv');
  startFakeJellyfin({ port: Number(arg('--port') || 8090), media, stream: arg('--stream') || 'direct' }).then(f => {
    console.log(`fake jellyfin: client ${f.origin}/app/  server ${f.serverUrl}  media host ${f.mediaOrigin}  (user demo)`);
  });
}
