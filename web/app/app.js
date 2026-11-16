// SPDX-License-Identifier: LGPL-3.0-only
// FraxEngine Jellyfin client: sign in, browse, play the original file with the FraxEngine Session, report progress.
import { JellyfinApi, JellyfinError, ProgressReporter, streamPositions, TICKS_PER_US } from './api.js';
import { PRESET_SERVERS } from './config.js';
import { Session } from '../player/js/session.js';

const $ = id => document.getElementById(id);
const api = new JellyfinApi({ defaultServer: `${location.origin}/jellyfin`, presets: PRESET_SERVERS });

const fmt = us => { const s = Math.max(0, Math.floor(us / 1e6)), h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60); return `${h ? h + ':' + String(m).padStart(2, '0') : m}:${String(s % 60).padStart(2, '0')}`; };
const el = (tag, { dataset, ...props } = {}, ...kids) => { const e = Object.assign(document.createElement(tag), props); if (dataset) Object.assign(e.dataset, dataset); e.append(...kids.filter(k => k != null)); return e; };

function toast(text) {
  const t = $('toast'); t.textContent = text; t.hidden = !text;
  clearTimeout(toast.timer); if (text) toast.timer = setTimeout(() => { t.hidden = true; }, 8000);
}

function showView(name) {
  for (const v of document.querySelectorAll('.view')) v.hidden = v.id !== name;
  $('bar').hidden = name === 'login' || name === 'player';
}

// Errors: a 401 means the token is gone (signed out elsewhere or expired); anything else is shown as is (never with a token).
function fail(e) {
  if (e instanceof JellyfinError && e.status === 401) { showLogin('Signed out by the server, please sign in again.'); return; }
  toast(e instanceof TypeError ? `Cannot reach ${api.serverUrl} (network error or CORS)` : (e.message || String(e)));
}

// ---------------------------------------------------------------- sign in and server picker
function renderServers() {
  const list = api.servers();
  $('serverlist').replaceChildren(...list.map(s => el('option', { value: s.url })));
  $('known').replaceChildren(...list.map(s => el('div', { className: 'server' },
    el('span', { className: 'url', textContent: s.url, title: s.url }),
    s.userName ? el('button', { type: 'button', textContent: `Continue as ${s.userName}`, onclick: () => { api.useServer(s.url); start(); } }) : el('button', { type: 'button', textContent: 'Use', onclick: () => { $('server').value = s.url; checkServer(); $('username').focus(); } }),
    s.url !== api.defaultServer && !PRESET_SERVERS.includes(s.url) ? el('button', { type: 'button', className: 'link', textContent: 'Forget', onclick: () => { api.forgetServer(s.url); renderServers(); } }) : null,
  )));
}

function showLogin(message = '') {
  stopPlayback();
  showView('login');
  // A link can name the server in its fragment (https://host/app/#server=URL): the fragment never reaches this site's logs or code.
  const linked = new URLSearchParams(location.hash.slice(1)).get('server');
  $('server').value = linked || api.serverUrl;
  $('loginerror').textContent = message;
  $('password').value = '';
  renderServers();
  checkServer();
}

$('loginform').addEventListener('submit', async e => {
  e.preventDefault();
  const btn = $('signin'), err = $('loginerror');
  btn.disabled = true; err.textContent = '';
  const password = $('password').value;
  $('password').value = '';   // the password lives only for this request; only the token is kept
  try {
    await api.login($('server').value, $('username').value.trim(), password);
    await start();
  } catch (e2) {
    err.textContent = e2 instanceof JellyfinError ? (e2.status === 401 ? 'Wrong username or password.' : e2.message)
      : e2 instanceof TypeError ? 'Cannot reach the server (network error, or it does not allow this origin via CORS).' : (e2.message || String(e2));
  } finally { btn.disabled = false; }
});

// Quick Connect: the server shows a code to approve from a device already signed in (AIOMetadata: its configuration page).
let qcRun = 0;
$('quickconnect').addEventListener('click', async () => {
  const run = ++qcRun, status = $('qcstatus');
  $('loginerror').textContent = ''; status.textContent = 'Asking the server for a code...';
  try {
    const qc = await api.quickConnectStart($('server').value);
    status.textContent = `Quick Connect code: ${qc.code}. Approve it on the server's Quick Connect page; this page continues by itself.`;
    for (const end = Date.now() + 10 * 60e3; Date.now() < end && run === qcRun; ) {
      await new Promise(r => setTimeout(r, 3000));
      if (run !== qcRun) return;
      if (await api.quickConnectApproved(qc)) {
        await api.quickConnectFinish(qc);
        status.textContent = '';
        await start();
        return;
      }
    }
    if (run === qcRun) status.textContent = 'The code expired, start again.';
  } catch (e) { if (run === qcRun) { status.textContent = ''; $('loginerror').textContent = e instanceof TypeError ? 'Cannot reach the server (network or CORS).' : `Quick Connect failed: ${e.message || e}`; } }
});

// Names the server behind the URL (System/Info/Public needs no sign-in), so a wrong URL shows before the password is typed.
let infoSeq = 0;
async function checkServer() {
  const seq = ++infoSeq, out = $('serverinfo');
  out.textContent = '';
  try {
    const i = await api.publicInfo($('server').value);
    if (seq === infoSeq) out.textContent = `${i.ServerName || 'Server'}: ${i.ProductName || 'Jellyfin'} ${i.Version || ''}`;
    // Offer the listed users. On AIOMetadata the user made on its Jellyfin screen is the one to sign in as (the entry with the server's
    // id is the configuration itself), so a single such user is filled in. Never over what the user typed.
    const users = await api.publicUsers($('server').value), name = $('username');
    if (seq !== infoSeq) return;
    $('userlist').replaceChildren(...users.map(u => el('option', { value: u.name })));
    const created = users.filter(u => u.id !== i.Id);
    const pick = created.length === 1 ? created[0] : users.find(u => u.id === i.Id) || (users.length === 1 ? users[0] : null);
    if (pick && (!name.value || name.dataset.auto === '1')) { name.value = pick.name; name.dataset.auto = '1'; }
  } catch (e) { if (seq === infoSeq) out.textContent = e instanceof TypeError ? 'Not reachable from this page (network or CORS).' : `Not a Jellyfin server here (${e.message}).`; }
}
$('server').addEventListener('change', checkServer);
$('server').addEventListener('input', checkServer);
$('username').addEventListener('input', () => { delete $('username').dataset.auto; });

$('logout').addEventListener('click', async () => { await api.logout(); showLogin(); });
$('switch').addEventListener('click', () => showLogin());

// ---------------------------------------------------------------- browsing
const nav = [];   // [{title, render}] for crumbs and Back
function go(title, render, { root = false } = {}) {
  if (root) nav.length = 0;
  nav.push({ title, render });
  draw();
}
async function draw() {
  stopPlayback();
  showView('browse');
  const top = nav[nav.length - 1];
  $('browsetitle').textContent = top.title;
  $('crumbs').replaceChildren(...nav.slice(0, -1).map((n, i) => el('button', { className: 'link', textContent: n.title, onclick: () => { nav.length = i + 1; draw(); } })));
  $('rows').replaceChildren(el('p', { className: 'muted', textContent: 'Loading...' }));
  try { $('rows').replaceChildren(...await top.render()); } catch (e) { $('rows').replaceChildren(); fail(e); }
}

function poster(item, wide = false) {
  const src = api.imageUrl(item, { maxHeight: wide ? 240 : 360 });
  const box = el('div', { className: 'poster' });
  if (src) {
    const img = el('img', { src, alt: '', loading: 'lazy', decoding: 'async' });
    img.onerror = () => { img.remove(); box.textContent = item.Name; };
    box.append(img);
  } else box.textContent = item.Name;
  const ticks = item.UserData?.PlaybackPositionTicks, total = item.RunTimeTicks;
  if (ticks && total) box.append(el('div', { className: 'bar', style: `width:${Math.min(100, (ticks / total) * 100).toFixed(1)}%` }));
  return box;
}

function subtitleOf(item) {
  if (item.Type === 'Episode') return `${item.SeriesName || ''} S${item.ParentIndexNumber ?? '?'}E${item.IndexNumber ?? '?'}`;
  if (item.ProductionYear) return String(item.ProductionYear);
  return item.Type === 'CollectionFolder' || item.CollectionType ? '' : item.Type;
}

function tile(item, onClick, wide = false) {
  return el('button', { className: 'tile' + (wide ? ' wide' : ''), onclick: onClick, title: item.Name, dataset: { id: item.Id, type: item.Type } },
    poster(item, wide), el('span', { className: 'name', textContent: item.Name }), el('span', { className: 'sub', textContent: subtitleOf(item) }));
}

function open(item) {
  if (item.Type === 'Series') return go(item.Name, () => seasonsView(item));
  if (item.Type === 'Season') return go(item.Name, () => episodesView(item));
  if (item.Type === 'Movie' || item.Type === 'Episode' || item.Type === 'Video' || item.MediaType === 'Video') return detail(item);
  return go(item.Name, () => libraryView(item));
}

const grid = items => el('div', { className: 'grid' }, ...items.map(i => tile(i, () => open(i), i.Type === 'Episode')));
const row = (title, items, wide) => el('div', { className: 'row' }, el('h3', { textContent: title }), items.length ? el('div', { className: 'grid' }, ...items.map(i => tile(i, () => open(i), wide))) : el('p', { className: 'muted', textContent: 'Nothing here.' }));

async function homeView() {
  const [resume, views] = await Promise.all([api.resume().catch(() => []), api.views()]);
  const out = [];
  if (resume.length) out.push(row('Continue watching', resume, true));
  out.push(views.length ? row('Libraries', views, true) : el('p', { className: 'muted', textContent: `${new URL(api.serverUrl).host} lists no libraries for the user ${api.userName}. If you picked them for another profile, sign in as that profile.` }));
  return out;
}

async function libraryView(view) {
  const types = view.CollectionType === 'movies' ? ['Movie'] : view.CollectionType === 'tvshows' ? ['Series'] : ['Movie', 'Series'];
  // AIOMetadata: a library that is only listed by genre is empty until one is asked for.
  const genres = view.aiostreams?.genreRequired ? await api.genres(view.Id) : [];
  const box = el('div');
  const list = async genre => {
    box.replaceChildren(el('p', { className: 'muted', textContent: 'Loading...' }));
    const g = el('div', { className: 'grid' }), status = el('p', { className: 'muted' });
    const more = el('button', { type: 'button', textContent: 'More' });
    let shown = 0;
    const page = async () => {
      more.disabled = true;
      const { items, total } = await api.items({ parentId: view.Id, types, genre, startIndex: shown });
      g.append(...items.map(i => tile(i, () => open(i), i.Type === 'Episode')));
      shown += items.length;
      status.textContent = items.length || shown ? (total > shown ? `Showing ${shown} of ${total}.` : '') : 'This library is empty.';
      more.hidden = !items.length || total <= shown; more.disabled = false;
    };
    more.onclick = () => page().catch(fail);
    await page();
    box.replaceChildren(g, status, more);
  };
  await list(genres[0] ?? null);
  if (!genres.length) return [box];
  const pick = el('select', { onchange: e => list(e.target.value).catch(fail) }, ...genres.map(g => el('option', { value: g, textContent: g })));
  return [pick, box];
}

async function seasonsView(series) {
  const seasons = await api.seasons(series.Id);
  if (seasons.length === 1) return episodesView(seasons[0], series.Id);
  return [grid(seasons)];
}

async function episodesView(season, seriesId = season.SeriesId) {
  return [grid(await api.episodes(seriesId, season.Id))];
}

let searchTimer = null;
$('search').addEventListener('input', () => {
  clearTimeout(searchTimer);
  const q = $('search').value.trim();
  if (q.length < 2) return;
  searchTimer = setTimeout(() => {
    const render = async () => { const items = await api.search(q); return items.length ? [grid(items)] : [el('p', { className: 'muted', textContent: 'No results.' })]; };
    if (nav[nav.length - 1]?.search) { nav[nav.length - 1] = { title: `Search: ${q}`, render, search: true }; draw(); } else { nav.push({ title: `Search: ${q}`, render, search: true }); draw(); }
  }, 350);
});
$('searchform').addEventListener('submit', e => e.preventDefault());
$('home').addEventListener('click', () => go('Home', homeView, { root: true }));

// ---------------------------------------------------------------- detail
let current = null;   // the item shown in detail
async function detail(item) {
  stopPlayback();
  showView('detail');
  current = item;
  $('dtitle').textContent = item.Type === 'Episode' ? `${item.SeriesName || ''}: ${item.Name}` : item.Name;
  $('dposter').src = api.imageUrl(item, { maxHeight: 480 }) || '';
  $('dmeta').textContent = [subtitleOf(item), item.RunTimeTicks ? fmt(item.RunTimeTicks / TICKS_PER_US) : ''].filter(Boolean).join('  ');
  $('doverview').textContent = item.Overview || '';
  $('dresume').hidden = true; $('dinfo').textContent = '';
  try {
    const full = await api.item(item.Id);
    if (current !== item) return;
    current = full;
    $('doverview').textContent = full.Overview || '';
    const pos = full.UserData?.PlaybackPositionTicks || 0;
    if (pos > 0) { $('dresume').hidden = false; $('dresume').textContent = `Resume from ${fmt(pos / TICKS_PER_US)}`; }
  } catch (e) { fail(e); }
}
$('dplay').addEventListener('click', () => current && play(current, 0));
$('dresume').addEventListener('click', () => current && play(current, (current.UserData?.PlaybackPositionTicks || 0) / TICKS_PER_US));
$('dback').addEventListener('click', () => draw());

// ---------------------------------------------------------------- playback
let playing = null;   // {session, reporter, item, positions, uiTimer}

function stopPlayback({ keepalive = false } = {}) {
  const p = playing; playing = null;
  if (!p) return Promise.resolve();
  clearInterval(p.uiTimer);
  const reported = p.reporter?.stop({ keepalive }) ?? Promise.resolve();
  p.session.stop().catch(() => {});
  $('stage').replaceChildren();
  return reported;
}

function setControls(s) {
  const live = ['paused', 'playing', 'ended', 'seeking', 'buffering'].includes(s.state);
  $('pplay').disabled = !['paused', 'playing'].includes(s.state);
  $('pplay').textContent = s.state === 'playing' ? 'Pause' : 'Play';
  $('pseek').disabled = !live || s.state === 'ended';
}

function fillTracks(s) {
  const aud = $('paudio'), sub = $('psubs');
  aud.replaceChildren(...(s.audioTracks || []).map((t, i) => el('option', { value: String(i), textContent: `${t.language || '?'} ${t.name ? t.name + ' ' : ''}${t.codec} ${t.channels || '?'}ch` })));
  aud.value = String(s.audioTracks?.indexOf(s.aTrack) ?? 0); aud.disabled = (s.audioTracks || []).length < 2;
  sub.replaceChildren(el('option', { value: '', textContent: 'Subtitles off' }), ...s.subtitleTracks().map((t, i) => el('option', { value: String(i), textContent: `${t.language || '?'} ${t.name ? t.name + ' ' : ''}${t.codec}` })));
  sub.value = s.subIndex == null ? '' : String(s.subIndex); sub.disabled = s.subtitleTracks().length === 0;
}

// Some stream addons send no CORS headers on their redirect, so the browser cannot open them: start with the first source this page can read.
async function firstReadable(sources) {
  for (let i = 0; i < Math.min(sources.length, 8); i++) {
    const ac = new AbortController(), t = setTimeout(() => ac.abort(), 8000);
    try { if ((await fetch(sources[i].Path, { headers: { Range: 'bytes=0-0' }, signal: ac.signal })).ok) return i; } catch {} finally { clearTimeout(t); ac.abort(); }
  }
  return 0;
}

async function play(item, startUs, sourceIndex = null) {
  stopPlayback();
  showView('player');
  $('pstatus').textContent = 'Opening...';
  $('ptime').textContent = '0:00 / 0:00';
  let info;
  try { info = await api.playbackInfo(item.Id); } catch (e) { $('pstatus').textContent = 'Playback info failed: ' + (e.message || e); return; }
  const sources = info?.MediaSources ?? [];
  if (sourceIndex === null) sourceIndex = api.aio && sources.length > 1 ? await firstReadable(sources) : 0;
  const ms = sources[sourceIndex];
  // A stream addon offers many files for one title; the list stays open to switch while watching.
  const picker = $('psource');
  picker.replaceChildren(...sources.map((m, i) => el('option', { value: String(i), textContent: m.Name || m.Path?.split('/').pop() || `Source ${i + 1}` })));
  picker.value = String(sourceIndex); picker.hidden = sources.length < 2;
  picker.onchange = () => play(item, playing?.session.currentTimeUs() ?? startUs, Number(picker.value));
  if (!ms) { $('pstatus').textContent = 'The server returned no media source for this item.'; return; }
  const positions = streamPositions(ms);
  const { url, headers } = api.mediaRequest(item.Id, ms);
  const token = {};
  const s = new Session({
    url, host: $('stage'), headers, wantPlay: true, startUs: Math.max(0, startUs || 0),
    audioIndex: positions.audioIndex, subtitle: positions.subtitleIndex, size: ms.Size || null, requireRange: true,
    onEvent: ev => {
      if (playing?.token !== token) return;
      if (ev.type === 'state') {
        setControls(s);
        if (ev.state === 'error') $('pstatus').textContent = 'Playback failed: ' + (s.error || 'unknown error');
        else if (ev.state === 'ended') { $('pstatus').textContent = 'Ended.'; playing.reporter?.stop(); }
        else $('pstatus').textContent = ev.state === 'playing' ? '' : ev.state;
      } else if (ev.type === 'error') $('pstatus').textContent = 'Playback failed: ' + ev.message;
    },
  });
  playing = { token, session: s, item, positions, reporter: null, uiTimer: 0, mediaSourceId: ms.Id };
  window.fraxJellyfin.session = s;
  await s.open();
  if (playing?.token !== token) return;
  if (s.state === 'error' || s.state === 'stopped') return;
  fillTracks(s);
  setControls(s);
  playing.reporter = new ProgressReporter(api, { itemId: item.Id, mediaSourceId: ms.Id, playSessionId: info.PlaySessionId }, () => ({
    positionUs: s.currentTimeUs(), paused: s.state !== 'playing',
    audioStreamIndex: positions.audioStreamIndex(s.audioTracks?.indexOf(s.aTrack) ?? 0),
    subtitleStreamIndex: positions.subtitleStreamIndex(s.subIndex),
  }));
  playing.reporter.start();
  playing.uiTimer = setInterval(() => {
    if (!['playing', 'paused', 'seeking', 'buffering'].includes(s.state)) return;
    const t = s.currentTimeUs(), d = s.durationUs;
    const text = `${fmt(t)} / ${fmt(d)}`;
    if ($('ptime').textContent !== text) $('ptime').textContent = text;
    if (!scrubbing && d) $('pseek').value = String(Math.round((t / d) * 1000));
  }, 250);
}

let scrubbing = false;
$('pseek').addEventListener('input', () => { scrubbing = true; const s = playing?.session; if (s) $('ptime').textContent = `${fmt(($('pseek').value / 1000) * s.durationUs)} / ${fmt(s.durationUs)}`; });
$('pseek').addEventListener('change', () => { scrubbing = false; const s = playing?.session; if (s) { s.seek(($('pseek').value / 1000) * s.durationUs); playing.reporter?.tick(); } });
$('pplay').addEventListener('click', () => { const s = playing?.session; if (!s) return; s.state === 'playing' ? s.pause() : s.play(); playing.reporter?.tick(); });
$('paudio').addEventListener('change', e => playing?.session.switchAudio(Number(e.target.value)));
$('psubs').addEventListener('change', e => playing?.session.setSubtitle(e.target.value === '' ? null : Number(e.target.value)));
// The stopped report goes first, so the detail view reads the new resume position.
$('pback').addEventListener('click', async () => { const item = playing?.item; showView(item ? 'detail' : 'browse'); await stopPlayback(); if (item) detail(item); else draw(); });
window.addEventListener('keydown', e => {
  const s = playing?.session;
  if (!s || e.target.tagName === 'INPUT' || e.target.tagName === 'SELECT') return;
  if (e.key === ' ') { e.preventDefault(); $('pplay').click(); }
  else if (e.key === 'ArrowRight') s.seek(s.currentTimeUs() + 10e6);
  else if (e.key === 'ArrowLeft') s.seek(s.currentTimeUs() - 10e6);
  else if (e.key === 'Escape') $('pback').click();
});
// Closing the tab: a keepalive request still delivers the stopped report (with the position) after the page is gone.
window.addEventListener('pagehide', () => stopPlayback({ keepalive: true }));

// ---------------------------------------------------------------- start
async function start() {
  if (!api.loggedIn) { showLogin(); return; }
  await api.detectServer();
  $('who').textContent = `${api.userName} @ ${new URL(api.serverUrl).host}`;
  go('Home', homeView, { root: true });
}

window.fraxJellyfin = { api, session: null, play: id => api.item(id).then(i => play(i, 0)) };
if (!crossOriginIsolated) toast('This page is not cross-origin isolated (COOP/COEP missing): the player needs SharedArrayBuffer.');
start();
