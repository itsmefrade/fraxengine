# Jellyfin client

`web/app/` is a small Jellyfin client for the FraxEngine player: ES modules, no build step, no framework, no CDN. It is served at `/app/`; the default server is `${location.origin}/jellyfin`.

| File | Role |
| --- | --- |
| `api.js` | API client, server list, progress reporter. No DOM, so node tests run it directly. |
| `app.js`, `index.html`, `style.css` | Sign-in, server picker, browsing, search, detail, player screen. |
| `config.js` | Servers listed from the start, next to the same-origin default. |
| `test/` | Fake server, node tests, headless Chrome smoke run. |

## How it works

**Sign-in.** `POST /Users/AuthenticateByName` with `Authorization: MediaBrowser Client="FraxEngine", Device=..., DeviceId=..., Version=...`. The device id is random and kept in localStorage. `X-Emby-Authorization` is not sent, because CORS allow lists often omit it. The password is used for that one request and never stored; localStorage holds the token, user id and user name per server. Log out removes the sign-in of the current server only. Each server you sign in to is remembered in the picker, and the login screen shows what `/System/Info/Public` reports for the URL.

**Library.** `/UserViews`, `/UserItems/Resume` and `/Items/{id}` first, the legacy `/Users/{id}/...` routes on a 404. Movies come from `/Items` with `ParentId` and `Recursive`; series go through `/Shows/{id}/Seasons` and `/Shows/{id}/Episodes`. Posters use `/Items/{id}/Images/Primary`, which needs no token.

**Playback.** `POST /Items/{id}/PlaybackInfo` gives the media source and play session. The player opens `/Videos/{id}/stream?static=true&mediaSourceId=...`, the original file with no transcoding, through `Session({url, headers, size, requireRange: true})`.

The token never goes in a URL. `Session({headers})` is posted to the demux worker and is applied to the Range probe, the helper Worker fetches, the sync XHR fallback and the whole-file download path. No worker path needed a workaround. Only the demux worker reads media, so no other worker needs the headers.

**Redirects.** A stream may redirect to another host. `openUrl` follows it once with the Range probe, then reads from the final URL. If that URL is on another origin the caller's headers are dropped, so the token stays with the Jellyfin server. A media host must then send CORS headers for this page and support Range.

**Clear failures.** With `requireRange`, these show as a message on the player screen instead of a hang or a multi-GB download: no CORS or a network error, no Range support, a hidden `Content-Range` with an unknown size, and no answer to the probe within 20 s. A cross-origin 206 hides `Content-Range` unless the host exposes it, so the client passes `MediaSources[].Size` from Jellyfin.

**Tracks and progress.** Audio and subtitle menus use the tracks the Session reports. `DefaultAudioStreamIndex` and `DefaultSubtitleStreamIndex` map to per-kind positions among embedded streams. Progress goes to `/Sessions/Playing`, `/Sessions/Playing/Progress` every 10 s and on pause or seek, and `/Sessions/Playing/Stopped` (keepalive when the tab closes). Resume starts from `UserData.PlaybackPositionTicks`.

## Reverse proxy headers

The player needs `SharedArrayBuffer`, so the pages and workers that host it must be cross-origin isolated. The Jellyfin server must not get these headers: they would break its own web UI.

| Path | Headers |
| --- | --- |
| `/app/*.html`, `/app/` | `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: credentialless` |
| `/player/`, `/engine/`, `/wasm/`, `/exact/`, `/fel/`, `/gpu-recon/`, `/subs/`, other `/app/` files | `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`, `Cross-Origin-Resource-Policy: same-origin` |
| `/jellyfin/` | None of the above: plain proxy, with Range requests passed through and response buffering off |

`credentialless` on the client page lets posters from a remote server load without a CORP header. With `require-corp` they would be blocked (same-origin posters are fine either way). `.wasm` must be served as `application/wasm`.

A cross-origin Jellyfin-compatible server must allow this origin and the `Authorization` and `Content-Type` request headers on its API.

## Tests

```bash
node --test web/app/test/api.test.mjs
node web/app/test/smoke.mjs [media file]
```

The smoke run needs `web/wasm`, `web/exact/wasm` and `web/player/fixtures` in place. It starts its own fake server and a headless Chrome with a new temporary profile, signs in, lists the movie, plays it for 10 s and requires 0 dropped frames. It also checks the resume point, a cross-origin redirect, a host that hides `Content-Range`, and the errors for a host without Range or CORS. The fake server's user is a fixture (`demo` / `demo-pass`).

## Known limits

- Chrome only, like the player. `credentialless` is not available in every browser.
- Only the original file plays. A file or codec the player cannot handle is an error, with no transcoding fallback.
- External subtitle files are not offered; only subtitles embedded in the file are.
- The first media source is used; there is no version picker.
- A library lists its first 200 items, with a note when there are more.
- A redirect is resolved once when playback opens. If the signed URL expires during playback, reads fail and the item must be reopened.
- The token sits in localStorage, so script injection on this origin could read it.
- `PlayMethod` is reported as `DirectPlay`, since the file is read unchanged.
