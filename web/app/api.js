// SPDX-License-Identifier: LGPL-3.0-only
// Jellyfin API client for the FraxEngine web client. No DOM: storage and fetch are injectable, so node tests run it as is.
// The access token travels in the Authorization header only; it never goes into a URL, an error message or the console.

const KEY_SESSIONS = 'frax.jellyfin.sessions';   // {serverUrl: {token, userId, userName}}; never a password
const KEY_ACTIVE = 'frax.jellyfin.active';       // the server in use
const KEY_SERVERS = 'frax.jellyfin.servers';     // remembered server URLs, most recent first
const KEY_DEVICE = 'frax.jellyfin.deviceId';
export const CLIENT_NAME = 'FraxEngine';
// AIOMetadata lists its genre-only libraries solely to clients named like this: they promise to send a genre (see libraryView).
const AIO_CLIENT_NAME = 'AIOStreams FraxEngine';
export const CLIENT_VERSION = '0.1.0';
export const TICKS_PER_US = 10;   // Jellyfin ticks are 100 ns

const memoryStorage = () => { const m = new Map(); return { getItem: k => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: k => m.delete(k) }; };
// Header values are quoted; a quote, comma or control character in a device name would break the parameter list.
const clean = v => String(v).replace(/["\\,\r\n]/g, ' ').trim() || 'unknown';
const randomId = () => {
  const b = new Uint8Array(16);
  globalThis.crypto.getRandomValues(b);
  return [...b].map(x => x.toString(16).padStart(2, '0')).join('');
};

export class JellyfinError extends Error {
  constructor(status, path) { super(`Jellyfin HTTP ${status} on ${path}`); this.status = status; this.path = path; }
}

export class JellyfinApi {
  constructor({ storage = typeof globalThis.localStorage?.getItem === 'function' ? globalThis.localStorage : memoryStorage(), fetch = globalThis.fetch.bind(globalThis), defaultServer = '', presets = [], deviceName = null } = {}) {
    this.storage = storage;
    this.fetch = fetch;
    this.defaultServer = defaultServer;
    this.presets = presets;
    this.deviceName = clean(deviceName || guessDevice());
    let id = storage.getItem(KEY_DEVICE);
    if (!id) { id = randomId(); storage.setItem(KEY_DEVICE, id); }
    this.deviceId = id;
    this.active = storage.getItem(KEY_ACTIVE) || defaultServer;
    this.aio = false;   // the active server is AIOMetadata (System/Info/Public carries an aiostreams block)
  }

  #read(key, fallback) { try { return JSON.parse(this.storage.getItem(key) || 'null') ?? fallback; } catch { return fallback; } }
  #sessions() { return this.#read(KEY_SESSIONS, {}); }

  get session() {
    const s = this.#sessions()[this.active];
    return s && s.token && s.userId ? { serverUrl: this.active, ...s } : null;
  }
  get loggedIn() { return !!this.session; }
  get userId() { return this.session?.userId ?? null; }
  get userName() { return this.session?.userName ?? null; }
  get serverUrl() { return this.active; }

  /** Remembered servers: the default, presets and every server used, each with the user signed in there (if any). */
  servers() {
    const sessions = this.#sessions();
    const urls = [...new Set([this.defaultServer, ...this.presets, ...this.#read(KEY_SERVERS, [])].filter(Boolean))];
    return urls.map(url => ({ url, userName: sessions[url]?.userName ?? null, active: url === this.active }));
  }

  /** Switches to a server (a URL is normalised and remembered); the sign-in there, if any, is kept per server. */
  useServer(url) {
    const server = JellyfinApi.normalizeServer(url);
    this.active = server;
    this.storage.setItem(KEY_ACTIVE, server);
    const list = this.#read(KEY_SERVERS, []).filter(u => u !== server);
    this.storage.setItem(KEY_SERVERS, JSON.stringify([server, ...list].slice(0, 20)));
    return server;
  }

  /** Drops a remembered server and its sign-in (the default and presets stay listed). */
  forgetServer(url) {
    this.storage.setItem(KEY_SERVERS, JSON.stringify(this.#read(KEY_SERVERS, []).filter(u => u !== url)));
    this.#clearSession(url);
  }

  static normalizeServer(url) {
    const u = new URL(String(url).trim());
    if (!/^https?:$/.test(u.protocol)) throw new Error('server URL must be http or https');
    u.search = ''; u.hash = '';
    return u.href.replace(/\/+$/, '');
  }

  /** MediaBrowser authorization header value; with a token once logged in. */
  authorization(token = this.session?.token) {
    const parts = [`Client="${this.aio ? AIO_CLIENT_NAME : CLIENT_NAME}"`, `Device="${this.deviceName}"`, `DeviceId="${this.deviceId}"`, `Version="${CLIENT_VERSION}"`];
    if (token) parts.push(`Token="${clean(token)}"`);
    return 'MediaBrowser ' + parts.join(', ');
  }

  /** Headers for media requests made by the player (the stream URL carries no token). */
  mediaHeaders() { return { Authorization: this.authorization() }; }

  url(path, query = null, server = this.serverUrl) {
    const u = new URL(server + path);
    if (query) for (const [k, v] of Object.entries(query)) if (v != null && v !== '') u.searchParams.set(k, Array.isArray(v) ? v.join(',') : String(v));
    return u.href;
  }

  async request(path, { method = 'GET', query = null, body = undefined, server = this.serverUrl, auth = true, keepalive = false, json = true } = {}) {
    const headers = { Authorization: auth ? this.authorization() : this.authorization(null), Accept: 'application/json' };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const r = await this.fetch(this.url(path, query, server), { method, headers, body: body === undefined ? undefined : JSON.stringify(body), keepalive, credentials: 'omit' });
    if (r.status === 401 && auth && this.session && server === this.active) this.#clearSession(server);
    if (!r.ok) { try { await r.body?.cancel(); } catch {} throw new JellyfinError(r.status, path); }
    if (!json || r.status === 204) { try { await r.body?.cancel(); } catch {} return null; }
    const text = await r.text();
    return text ? JSON.parse(text) : null;
  }

  // A newer route first, the legacy /Users/{id}/... route when the server does not know it (404).
  async #either(newer, legacy) {
    try { return await newer(); } catch (e) { if (e instanceof JellyfinError && e.status === 404) return legacy(); throw e; }
  }

  /** Unauthenticated server identity (name, product, version): checks a URL before signing in. */
  publicInfo(serverUrl) { return this.request('/System/Info/Public', { server: JellyfinApi.normalizeServer(serverUrl), auth: false }); }

  /** The users the server lists without a sign-in, as {name, id} ([] when it lists none). */
  async publicUsers(serverUrl) {
    try { return ((await this.request('/Users/Public', { server: JellyfinApi.normalizeServer(serverUrl), auth: false })) || []).filter(u => u.Name).map(u => ({ name: u.Name, id: u.Id })); }
    catch { return []; }
  }

  /** Learns what kind of server the URL is; a failure leaves the plain Jellyfin behaviour. */
  async detectServer(serverUrl = this.active) {
    this.aio = !!(await this.publicInfo(serverUrl).catch(() => null))?.aiostreams;
    return this.aio;
  }

  async login(serverUrl, username, password) {
    const server = JellyfinApi.normalizeServer(serverUrl);
    await this.detectServer(server);
    const r = await this.request('/Users/AuthenticateByName', { method: 'POST', server, auth: false, body: { Username: String(username), Pw: String(password) } });
    return this.#signedIn(server, r, String(username));
  }

  #signedIn(server, r, fallbackName) {
    if (!r?.AccessToken || !r?.User?.Id) throw new Error('login answer without a token');
    const entry = { token: r.AccessToken, userId: r.User.Id, userName: r.User.Name || fallbackName };
    this.storage.setItem(KEY_SESSIONS, JSON.stringify({ ...this.#sessions(), [server]: entry }));
    this.useServer(server);
    return { userId: entry.userId, userName: entry.userName };
  }

  /** Starts a Quick Connect sign-in: the returned Code is approved elsewhere, the Secret stays here and is never shown. */
  async quickConnectStart(serverUrl) {
    const server = JellyfinApi.normalizeServer(serverUrl);
    await this.detectServer(server);
    if (await this.request('/QuickConnect/Enabled', { server, auth: false }) !== true) throw new Error('Quick Connect is turned off on this server');
    const r = await this.request('/QuickConnect/Initiate', { method: 'POST', server, auth: false });
    if (!r?.Code || !r?.Secret) throw new Error('Quick Connect answer without a code');
    return { server, code: r.Code, secret: r.Secret };
  }

  /** True once the code has been approved. */
  async quickConnectApproved({ server, secret }) {
    return (await this.request('/QuickConnect/Connect', { server, auth: false, query: { Secret: secret } }))?.Authenticated === true;
  }

  async quickConnectFinish({ server, secret }) {
    const r = await this.request('/Users/AuthenticateWithQuickConnect', { method: 'POST', server, auth: false, body: { Secret: secret } });
    return this.#signedIn(server, r, 'Quick Connect');
  }

  async logout() {
    if (this.session) { try { await this.request('/Sessions/Logout', { method: 'POST', json: false }); } catch { /* the local session goes either way */ } }
    this.#clearSession();
  }

  #clearSession(server = this.active) {
    const all = this.#sessions(); delete all[server];
    this.storage.setItem(KEY_SESSIONS, JSON.stringify(all));
  }

  views() {
    const uid = this.userId;
    return this.#either(() => this.request('/UserViews', { query: { userId: uid } }), () => this.request(`/Users/${uid}/Views`)).then(r => r?.Items ?? []);
  }

  /** Items of a library (movies and series; recursive so folder layouts do not matter). */
  items({ parentId = null, types = ['Movie', 'Series', 'Episode'], recursive = true, searchTerm = null, genre = null, startIndex = 0, limit = 100 } = {}) {
    return this.request('/Items', { query: {
      userId: this.userId, ParentId: parentId, Recursive: recursive, IncludeItemTypes: types, SearchTerm: searchTerm, Genres: genre,
      SortBy: 'SortName', SortOrder: 'Ascending', Fields: 'PrimaryImageAspectRatio,Overview', ImageTypeLimit: 1, EnableImageTypes: 'Primary',
      StartIndex: startIndex, Limit: limit,
    } }).then(r => ({ items: r?.Items ?? [], total: r?.TotalRecordCount ?? 0 }));
  }

  /** The genres a library can be listed by. */
  genres(parentId) { return this.request('/Genres', { query: { userId: this.userId, ParentId: parentId } }).then(r => (r?.Items ?? []).map(g => g.Name)); }

  search(term, limit = 60) { return this.items({ searchTerm: term, types: ['Movie', 'Series', 'Episode'], limit }).then(r => r.items); }

  resume(limit = 24) {
    const uid = this.userId, query = { userId: uid, Limit: limit, MediaTypes: 'Video', Fields: 'PrimaryImageAspectRatio' };
    return this.#either(() => this.request('/UserItems/Resume', { query }), () => this.request(`/Users/${uid}/Items/Resume`, { query })).then(r => r?.Items ?? []);
  }

  seasons(seriesId) { return this.request(`/Shows/${seriesId}/Seasons`, { query: { userId: this.userId } }).then(r => r?.Items ?? []); }

  episodes(seriesId, seasonId) {
    return this.request(`/Shows/${seriesId}/Episodes`, { query: { userId: this.userId, seasonId, Fields: 'Overview' } }).then(r => r?.Items ?? []);
  }

  item(id) {
    const uid = this.userId;
    return this.#either(() => this.request(`/Items/${id}`, { query: { userId: uid } }), () => this.request(`/Users/${uid}/Items/${id}`));
  }

  /** Media sources and a play session id. The profile asks for nothing: the client always plays the original file. */
  playbackInfo(id) {
    return this.request(`/Items/${id}/PlaybackInfo`, { method: 'POST', query: { userId: this.userId }, body: { UserId: this.userId, AutoOpenLiveStream: false, EnableDirectPlay: true, EnableDirectStream: true, EnableTranscoding: false } });
  }

  /** Poster URL. Jellyfin serves images without authentication, so no token is needed (or added). */
  imageUrl(item, { maxHeight = 360, type = 'Primary' } = {}) {
    const tag = item.ImageTags?.[type];
    const id = tag ? item.Id : type === 'Primary' && item.SeriesPrimaryImageTag ? item.SeriesId : null;
    if (!id) return null;
    return this.url(`/Items/${id}/Images/${type}`, { maxHeight, quality: 90, tag: tag || item.SeriesPrimaryImageTag });
  }

  /** Where a media source is really played from: AIOMetadata hands out the stream addon's own URL, which needs no token. */
  mediaRequest(itemId, ms) {
    if (this.aio && /^https?:\/\//i.test(ms.Path || '')) return { url: ms.Path, headers: {} };
    return { url: this.streamUrl(itemId, ms.Id), headers: this.mediaHeaders() };
  }

  /** The original file, byte for byte (static=true: no remux, no transcode). */
  streamUrl(itemId, mediaSourceId) { return this.url(`/Videos/${itemId}/stream`, { static: 'true', mediaSourceId }); }

  // ---------------------------------------------------------------- playback reporting
  #playState({ itemId, mediaSourceId, playSessionId, positionUs = 0, paused = false, audioStreamIndex = null, subtitleStreamIndex = null }) {
    return {
      ItemId: itemId, MediaSourceId: mediaSourceId, PlaySessionId: playSessionId, PositionTicks: Math.max(0, Math.round(positionUs * TICKS_PER_US)),
      IsPaused: !!paused, IsMuted: false, CanSeek: true, PlayMethod: 'DirectPlay', AudioStreamIndex: audioStreamIndex, SubtitleStreamIndex: subtitleStreamIndex,
    };
  }
  reportStart(s) { return this.request('/Sessions/Playing', { method: 'POST', body: this.#playState(s), json: false }); }
  reportProgress(s) { return this.request('/Sessions/Playing/Progress', { method: 'POST', body: { ...this.#playState(s), EventName: 'timeupdate' }, json: false }); }
  reportStopped(s, { keepalive = false } = {}) { return this.request('/Sessions/Playing/Stopped', { method: 'POST', body: this.#playState(s), json: false, keepalive }); }
}

function guessDevice() {
  const ua = globalThis.navigator?.userAgent || '';
  const browser = /Edg\//.test(ua) ? 'Edge' : /Chrome\//.test(ua) ? 'Chrome' : /Firefox\//.test(ua) ? 'Firefox' : /Safari\//.test(ua) ? 'Safari' : 'Browser';
  const os = /Mac OS X/.test(ua) ? 'macOS' : /Windows/.test(ua) ? 'Windows' : /Android/.test(ua) ? 'Android' : /Linux/.test(ua) ? 'Linux' : '';
  return os ? `${browser} on ${os}` : browser;
}

/** Map Jellyfin stream indexes to the player's per-kind positions (embedded streams only, in file order). */
export function streamPositions(mediaSource) {
  const streams = (mediaSource?.MediaStreams ?? []).filter(s => !s.IsExternal);
  const audio = streams.filter(s => s.Type === 'Audio').sort((a, b) => a.Index - b.Index);
  const subs = streams.filter(s => s.Type === 'Subtitle').sort((a, b) => a.Index - b.Index);
  const pos = (list, idx) => { const i = list.findIndex(s => s.Index === idx); return i < 0 ? null : i; };
  return {
    audioIndex: pos(audio, mediaSource?.DefaultAudioStreamIndex),
    subtitleIndex: mediaSource?.DefaultSubtitleStreamIndex == null || mediaSource.DefaultSubtitleStreamIndex < 0 ? null : pos(subs, mediaSource.DefaultSubtitleStreamIndex),
    audioStreamIndex: i => audio[i]?.Index ?? null,
    subtitleStreamIndex: i => (i == null ? -1 : subs[i]?.Index ?? null),
  };
}

/** Periodic progress reports for one playback: start, every `intervalMs`, stopped once. `state()` returns {positionUs, paused, ...}. */
export class ProgressReporter {
  // The timer functions are wrapped: a browser's setInterval called as a method of another object throws (Illegal invocation).
  constructor(api, base, state, { intervalMs = 10000, setInterval: si = (f, ms) => globalThis.setInterval(f, ms), clearInterval: ci = t => globalThis.clearInterval(t) } = {}) {
    this.api = api; this.base = base; this.state = state; this.intervalMs = intervalMs; this.si = si; this.ci = ci;
    this.timer = null; this.stopped = false; this.started = false;
  }
  #payload() { return { ...this.base, ...this.state() }; }
  async start() {
    this.started = true;
    try { await this.api.reportStart(this.#payload()); } catch { /* reporting is best effort; playback goes on */ }
    if (!this.stopped) this.timer = this.si(() => this.tick(), this.intervalMs);
  }
  async tick() { if (this.stopped) return; try { await this.api.reportProgress(this.#payload()); } catch {} }
  async stop({ keepalive = false } = {}) {
    if (this.stopped || !this.started) return;
    this.stopped = true;
    this.ci(this.timer); this.timer = null;
    try { await this.api.reportStopped(this.#payload(), { keepalive }); } catch {}
  }
}
