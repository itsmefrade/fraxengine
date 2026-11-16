// SPDX-License-Identifier: LGPL-3.0-only
// Per-stage time counters (docs/perf/CORE-V2.md). Each worker adds the time its own stages take and sends the totals with its
// stats; the perf page turns them into milliseconds per second of playback and sets them against the thread's CPU time measured
// from outside. Two clock reads per call, a few hundred calls a second: below what the measurement can see.
const acc = Object.create(null);
export const stageNow = () => performance.now();
export function stage(name, t0, bytes = 0) { const a = acc[name] || (acc[name] = [0, 0, 0]); a[0] += performance.now() - t0; a[1]++; a[2] += bytes; }
// Time measured elsewhere (a nested worker reports its own totals with its results): added under this worker's name for it.
export function stageAdd(name, ms, n = 1, bytes = 0) { const a = acc[name] || (acc[name] = [0, 0, 0]); a[0] += ms; a[1] += n; a[2] += bytes; }
export const stages = () => acc;
