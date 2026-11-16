// SPDX-License-Identifier: LGPL-3.0-only
// Node 22+ treats `node --test <dir>` as a module path, so this entry point (directory index)
// loads every *.test.mjs in the folder into one test process. `node --test web/engine/test/*.test.mjs`
// works too.
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
(async () => {
  for (const f of fs.readdirSync(__dirname).filter(n => n.endsWith('.test.mjs')).sort()) {
    await import(pathToFileURL(path.join(__dirname, f)).href);
  }
})();
