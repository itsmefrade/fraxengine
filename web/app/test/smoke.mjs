// SPDX-License-Identifier: LGPL-3.0-only
// Headless Chrome smoke run of the Jellyfin client against the fake server, in a fresh temporary profile (never the user's Chrome):
// sign in, list the movie, play it for 10 s (0 dropped frames), resume point, a cross-origin redirect, and the clear errors for a
// media host without CORS or without Range support.
//
//   node web/app/test/smoke.mjs [media file]   (default web/player/fixtures/zz-2audio.mkv; needs web/wasm etc. in place)
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { startFakeJellyfin, WEB_ROOT } from './fake-jellyfin.mjs';

const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const media = process.argv[2] || process.env.FRAX_JF_MEDIA || path.join(WEB_ROOT, 'player/fixtures/zz-2audio.mkv');
const fake = await startFakeJellyfin({ media });
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'frax-jf-smoke-'));
const chrome = spawn(CHROME, [
  '--headless=new', `--user-data-dir=${profile}`, '--no-first-run', '--no-default-browser-check', '--disable-extensions',
  '--autoplay-policy=no-user-gesture-required', '--window-size=1300,900', `${fake.origin}/app/test/smoke.html`,
], { stdio: 'ignore' });

let result = null;
const deadline = Date.now() + 180_000;
while (!result && Date.now() < deadline) {
  await new Promise(r => setTimeout(r, 500));
  result = fake.state.results[0] ?? null;
}
chrome.kill('SIGTERM');
await fake.close();
console.log(JSON.stringify(result ?? { ok: false, error: 'no result within 180 s' }, null, 2));
console.log(`profile left at ${profile}`);
process.exit(result?.ok ? 0 : 1);
