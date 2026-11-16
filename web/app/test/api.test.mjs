// SPDX-License-Identifier: LGPL-3.0-only
// The Jellyfin API client against the fake server: node --test web/app/test/*.test.mjs
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startFakeJellyfin } from './fake-jellyfin.mjs';
import { JellyfinApi, JellyfinError, ProgressReporter, streamPositions } from '../api.js';
import { openUrl } from '../../player/js/source.js';

const memory = () => { const m = new Map(); return { getItem: k => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: k => m.delete(k), dump: () => [...m.values()].join('\n') }; };
const MEDIA = path.join(os.tmpdir(), `frax-jf-test-${process.pid}.bin`);
let fake, fake2;

before(async () => {
  fs.writeFileSync(MEDIA, Buffer.from(Array.from({ length: 300_000 }, (_, i) => i & 255)));
  fake = await startFakeJellyfin({ media: MEDIA });
  fake2 = await startFakeJellyfin({ media: MEDIA });
});
after(async () => { await fake.close(); await fake2.close(); });

const newApi = (storage = memory(), extra = {}) => new JellyfinApi({ storage, defaultServer: fake.serverUrl, deviceName: 'node "test", runner', ...extra });

test('login sends a MediaBrowser Authorization header and keeps only the token, never the password', async () => {
  const storage = memory();
  const api = newApi(storage);
  const r = await api.login(fake.serverUrl + '/', 'demo', 'demo-pass');
  assert.equal(r.userName, 'demo');
  assert.ok(api.loggedIn);
  const login = fake.state.requests.find(q => q.url === '/jellyfin/Users/AuthenticateByName');
  assert.equal(login.client, 'FraxEngine');
  assert.equal(login.deviceId, api.deviceId);
  assert.ok(!storage.dump().includes('demo-pass'));
  assert.ok(storage.dump().includes(api.session.token));
  assert.match(api.authorization(), /^MediaBrowser Client="FraxEngine", Device="node  test   runner", DeviceId="[0-9a-f]{32}", Version="[^"]+", Token="[0-9a-f]+"$/);
  assert.equal(newApi(storage).deviceId, api.deviceId, 'the device id is stable across instances');
});

test('a wrong password is a 401 JellyfinError and leaves nothing signed in', async () => {
  const api = newApi();
  await assert.rejects(api.login(fake.serverUrl, 'demo', 'nope'), e => e instanceof JellyfinError && e.status === 401 && !e.message.includes('nope'));
  assert.equal(api.loggedIn, false);
});

test('views, library items, series, seasons, episodes, search and item detail', async () => {
  const api = newApi();
  await api.login(fake.serverUrl, 'demo', 'demo-pass');
  const views = await api.views();
  assert.deepEqual(views.map(v => v.Name), ['Movies', 'Shows']);
  const movies = await api.items({ parentId: 'view-movies', types: ['Movie'] });
  assert.deepEqual(movies.items.map(i => i.Id), ['movie-1']);
  const shows = await api.items({ parentId: 'view-shows', types: ['Series'] });
  assert.equal(shows.items[0].Id, 'series-1');
  const seasons = await api.seasons('series-1');
  assert.equal(seasons[0].Id, 'season-1');
  const eps = await api.episodes('series-1', 'season-1');
  assert.equal(eps[0].Id, 'episode-1');
  assert.deepEqual((await api.search('fixture')).map(i => i.Id).sort(), ['movie-1', 'series-1']);
  const item = await api.item('movie-1');
  assert.equal(item.MediaSources[0].Id, 'ms-movie-1');
  assert.match(api.imageUrl(item, { maxHeight: 300 }), /\/jellyfin\/Items\/movie-1\/Images\/Primary\?maxHeight=300&quality=90&tag=p2$/);
  assert.match(api.imageUrl(eps[0]), /\/Items\/series-1\/Images\/Primary/, 'an episode without its own image uses the series poster');
  const q = fake.state.requests.find(r => r.url.startsWith('/jellyfin/Items?'));
  assert.match(q.url, /IncludeItemTypes=Movie/);
  assert.match(q.url, /Recursive=true/);
});

test('legacy /Users/{id}/... routes are used when the newer route answers 404', async () => {
  const seen = [];
  const fetch404 = (url, init) => { seen.push(new URL(url).pathname); return /\/(UserViews|UserItems\/Resume)$/.test(new URL(url).pathname) ? Promise.resolve(new Response('', { status: 404 })) : fetch(url, init); };
  const api = newApi(memory(), { fetch: fetch404 });
  await api.login(fake.serverUrl, 'demo', 'demo-pass');
  assert.equal((await api.views()).length, 2);
  await api.resume();
  assert.ok(seen.includes('/jellyfin/Users/user-demo/Views'));
  assert.ok(seen.includes('/jellyfin/Users/user-demo/Items/Resume'));
});

test('the original file streams with the token in a header only: 206 with it, 401 without, and no URL ever carries it', async () => {
  const api = newApi();
  await api.login(fake.serverUrl, 'demo', 'demo-pass');
  const info = await api.playbackInfo('movie-1');
  const src = info.MediaSources[0];
  const url = api.streamUrl('movie-1', src.Id);
  assert.match(url, /\/Videos\/movie-1\/stream\?static=true&mediaSourceId=ms-movie-1$/);
  const r = await fetch(url, { headers: { ...api.mediaHeaders(), Range: 'bytes=100-199' } });
  assert.equal(r.status, 206);
  assert.equal(r.headers.get('content-range'), `bytes 100-199/${src.Size}`);
  assert.equal((await r.arrayBuffer()).byteLength, 100);
  assert.equal((await fetch(url, { headers: { Range: 'bytes=0-0' } })).status, 401);
  for (const t of fake.tokens()) for (const q of fake.state.requests) assert.ok(!q.url.includes(t) && !/api_key/i.test(q.url), q.url);
});

test('progress reports: start, periodic progress, stopped once; the position becomes the resume point', async () => {
  const api = newApi();
  await api.login(fake.serverUrl, 'demo', 'demo-pass');
  let tick = null, pos = 0;
  const rep = new ProgressReporter(api, { itemId: 'episode-1', mediaSourceId: 'ms-episode-1', playSessionId: 'ps' }, () => ({ positionUs: pos, paused: false }),
    { intervalMs: 10000, setInterval: (fn, ms) => { assert.equal(ms, 10000); tick = fn; return 1; }, clearInterval: () => { tick = null; } });
  const n0 = fake.state.events.length;
  await rep.start();
  pos = 12_345_678; await tick();
  pos = 20_000_000; await rep.stop();
  await rep.stop();
  assert.equal(tick, null);
  const ev = fake.state.events.slice(n0);
  assert.deepEqual(ev.map(e => e.type), ['Playing', 'Progress', 'Stopped']);
  assert.equal(ev[1].body.PositionTicks, 123_456_780);
  assert.equal(ev[2].body.PlaySessionId, 'ps');
  const resume = await api.resume();
  assert.equal(resume[0].Id, 'episode-1');
  assert.equal(resume[0].UserData.PlaybackPositionTicks, 200_000_000);
});

test('several servers are remembered with a sign-in each; logout and forget only touch the server they name', async () => {
  const storage = memory();
  const api = new JellyfinApi({ storage, defaultServer: fake.serverUrl, presets: ['https://preset.example/jellyfin/abc'] });
  await api.login(fake.serverUrl, 'demo', 'demo-pass');
  await api.login(fake2.serverUrl, 'demo', 'demo-pass');
  assert.equal(api.serverUrl, fake2.serverUrl);
  const list = api.servers();
  assert.deepEqual(list.map(s => s.url), [fake.serverUrl, 'https://preset.example/jellyfin/abc', fake2.serverUrl]);
  assert.deepEqual(list.map(s => s.userName), ['demo', null, 'demo']);
  api.useServer(fake.serverUrl);
  assert.equal((await api.views()).length, 2, 'the first server still has its token');
  await api.logout();
  assert.equal(api.loggedIn, false);
  api.useServer(fake2.serverUrl);
  assert.equal(api.loggedIn, true);
  api.forgetServer(fake2.serverUrl);
  assert.equal(api.loggedIn, false);
  assert.ok(!api.servers().some(s => s.url === fake2.serverUrl));
  assert.throws(() => api.useServer('ftp://x'), /http or https/);
});

test('a 401 on an API call signs the active server out', async () => {
  const api = newApi();
  await api.login(fake.serverUrl, 'demo', 'demo-pass');
  fake.state.tokens.delete(api.session.token);
  await assert.rejects(api.views(), e => e.status === 401);
  assert.equal(api.loggedIn, false);
});

test('Jellyfin stream indexes map to the player per-kind positions, external subtitles skipped', () => {
  const p = streamPositions({ DefaultAudioStreamIndex: 2, DefaultSubtitleStreamIndex: 5, MediaStreams: [
    { Index: 0, Type: 'Video' }, { Index: 1, Type: 'Audio' }, { Index: 2, Type: 'Audio' }, { Index: 3, Type: 'Subtitle', IsExternal: true }, { Index: 4, Type: 'Subtitle' }, { Index: 5, Type: 'Subtitle' }] });
  assert.equal(p.audioIndex, 1);
  assert.equal(p.subtitleIndex, 1);
  assert.equal(p.audioStreamIndex(0), 1);
  assert.equal(p.subtitleStreamIndex(0), 4);
  assert.equal(p.subtitleStreamIndex(null), -1);
  assert.equal(streamPositions({ DefaultSubtitleStreamIndex: -1, MediaStreams: [] }).subtitleIndex, null);
});

test('a media host without Range support is a clear error when Range is required (no whole-file download)', async () => {
  fake.state.config.range = false;
  try {
    await assert.rejects(openUrl(`${fake.mediaOrigin}/media/movie-1`, {}, { requireRange: true }), /does not support HTTP Range requests/);
  } finally { fake.state.config.range = true; }
  await assert.rejects(openUrl('http://127.0.0.1:1/none', {}, { requireRange: true }), /failed: it \(or the host it redirects to\) sends no CORS headers/);
});

test('AIOMetadata: detected from public info, named so genre libraries are listed, plays the addon URL without a token', async () => {
  const seen = [];
  const fetch = async (url, init) => {
    seen.push({ url, auth: init.headers.Authorization });
    if (url.endsWith('/System/Info/Public')) return new Response(JSON.stringify({ ServerName: 'AIOMetadata', aiostreams: { version: '1' } }), { status: 200 });
    return new Response(JSON.stringify({ Items: [{ Name: 'Action' }] }), { status: 200 });
  };
  const api = new JellyfinApi({ fetch, defaultServer: 'https://aio.test/jellyfin/x', presets: [] });
  assert.equal(api.authorization().startsWith('MediaBrowser Client="FraxEngine"'), true);
  assert.equal(await api.detectServer(), true);
  assert.equal(api.authorization().startsWith('MediaBrowser Client="AIOStreams FraxEngine"'), true);
  assert.deepEqual(await api.genres('view1'), ['Action']);
  assert.deepEqual(api.mediaRequest('i', { Id: 'm', Path: 'https://addon.test/play/file.mkv' }), { url: 'https://addon.test/play/file.mkv', headers: {} });
  const own = api.mediaRequest('i', { Id: 'm', Path: '/media/x.mkv' });
  assert.match(own.url, /\/Videos\/i\/stream\?/);
  assert.ok(own.headers.Authorization);
});

test('Quick Connect: code, approval, sign-in; the secret is only sent to the server', async () => {
  const calls = [];
  let approved = false;
  const fetch = async (url, init) => {
    const u = new URL(url); calls.push(u.pathname);
    const json = v => new Response(JSON.stringify(v), { status: 200 });
    if (u.pathname.endsWith('/System/Info/Public')) return json({ ServerName: 'x' });
    if (u.pathname.endsWith('/QuickConnect/Enabled')) return json(true);
    if (u.pathname.endsWith('/QuickConnect/Initiate')) return json({ Code: '123456', Secret: 's3', Authenticated: false });
    if (u.pathname.endsWith('/QuickConnect/Connect')) { assert.equal(u.searchParams.get('Secret'), 's3'); return json({ Authenticated: approved }); }
    if (u.pathname.endsWith('/Users/AuthenticateWithQuickConnect')) { assert.equal(JSON.parse(init.body).Secret, 's3'); return json({ AccessToken: 't', User: { Id: 'u', Name: 'Me' } }); }
    throw new Error('unexpected ' + u.pathname);
  };
  const api = new JellyfinApi({ storage: memory(), fetch, defaultServer: 'https://j.test/jellyfin', presets: [] });
  const qc = await api.quickConnectStart('https://j.test/jellyfin');
  assert.equal(qc.code, '123456');
  assert.equal(await api.quickConnectApproved(qc), false);
  approved = true;
  assert.equal(await api.quickConnectApproved(qc), true);
  assert.deepEqual(await api.quickConnectFinish(qc), { userId: 'u', userName: 'Me' });
  assert.equal(api.loggedIn, true);
});
