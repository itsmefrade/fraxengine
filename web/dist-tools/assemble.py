#!/usr/bin/env python3
"""Assemble web/dist from the runtime import graph of web/player/index.html. Python 3 standard library only.

The file set is not a hand-kept list: the page's script/link tags are the roots, ES module imports
(static and dynamic with a string literal), `new URL('x', import.meta.url)` and the player's `wurl('x')` /
`#spawn('x.js', label)` helpers (worker and worklet names) are followed. A reference that resolves to a missing file fails the
build, as does a reached file under a test/tools/fixtures/results directory.

Layout: dist/index.html (+ coi.js, coi-sw.js, host configs, manifest.json) and dist/assets/<build id>/
holding the web/ tree (player, engine, fel, exact, subs, wasm) with its relative layout unchanged, so
no source file is rewritten. The build id is a hash of all file contents, which makes every asset URL
immutable (cache bust by directory name).
"""
import argparse, gzip, hashlib, json, os, re, shutil, subprocess, sys, tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
WEB = HERE.parent
REPO = WEB.parent
FORBIDDEN_DIRS = {'test', 'tests', 'tools', 'fixtures', 'results', 'dev-media', 'refs', 'node_modules'}
SOURCE_EXTS = {'.js', '.mjs'}

def strip_comments(src):
    """Remove // and /* */ comments while leaving string/template literals intact."""
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if c in '"\'`':
            j = i + 1
            while j < n and src[j] != c:
                j += 2 if src[j] == '\\' else 1
            out.append(src[i:j + 1]); i = j + 1
        elif src.startswith('//', i):
            j = src.find('\n', i); i = n if j < 0 else j
        elif src.startswith('/*', i):
            j = src.find('*/', i + 2); i = n if j < 0 else j + 2
        else:
            out.append(c); i += 1
    return ''.join(out)

# The lookbehind keeps a string literal 'import' (e.g. `q !== 'import'`) from being read as an import statement.
RE_IMPORT = re.compile(r'''(?<!['"`.])(?:\bimport|\bexport)\s*(?:[\w$*{}\s,]*?\bfrom\s*)?(['"])([^'"\n]+)\1''')
RE_DYNAMIC = re.compile(r'''\bimport\s*\(\s*(['"])([^'"\n]+)\1\s*\)''')
RE_URL = re.compile(r'''new\s+URL\(\s*(['"])([^'"\n]+)\1\s*,\s*import\.meta\.url''')
RE_WURL = re.compile(r'''(?:\bwurl|#spawn)\(\s*(['"])([^'"\n]+\.m?js)\1''')

def refs_of_js(path):
    src = strip_comments(path.read_text(encoding='utf-8'))
    found = []
    for rx in (RE_IMPORT, RE_DYNAMIC, RE_URL, RE_WURL):
        found += [m.group(2) for m in rx.finditer(src)]
    return found

def refs_of_html(path):
    src = path.read_text(encoding='utf-8')
    found = re.findall(r'''<script[^>]*\ssrc=["']([^"']+)["']''', src)
    found += re.findall(r'''<link[^>]*\shref=["']([^"']+)["']''', src)
    return found

def resolve(base_file, ref):
    if re.match(r'^[a-z][a-z0-9+.-]*:', ref, re.I) or ref.startswith('//'):
        return None  # node:, http:, data: ... not a local runtime file
    if '${' in ref:
        return None
    return (base_file.parent / ref.split('?')[0].split('#')[0]).resolve()

def scan(entry_html):
    """Return (files set of Paths, problems list)."""
    files, problems, seen = set(), [], set()
    stack = [(entry_html, ref) for ref in refs_of_html(entry_html)]
    files.add(entry_html)
    while stack:
        origin, ref = stack.pop()
        target = resolve(origin, ref)
        if target is None:
            continue
        if not target.is_file():
            problems.append(f'{origin.relative_to(WEB)} references missing file: {ref}')
            continue
        try:
            rel = target.relative_to(WEB)
        except ValueError:
            problems.append(f'{origin.relative_to(WEB)} references a file outside web/: {ref}')
            continue
        if rel.parts[0] == 'dist' or any(p in FORBIDDEN_DIRS for p in rel.parts[:-1]):
            problems.append(f'{origin.relative_to(WEB)} reaches non-runtime file {rel} via {ref}')
            continue
        if target in seen:
            continue
        seen.add(target); files.add(target)
        if target.suffix in SOURCE_EXTS:
            stack += [(target, r) for r in refs_of_js(target)]
    return files, problems

def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()

def git(*args):
    try:
        return subprocess.run(['git', '-C', str(REPO), *args], capture_output=True, text=True, check=True).stdout.strip()
    except Exception:
        return None

SOURCE_EXCLUDE_DIRS = {'target', 'target-asm', 'target-scalar', 'target-simd', 'dev-media', 'dist', 'node_modules', '__pycache__', '.git',
                       'fixtures', 'results', 'lab', 'media', 'draft', 'rawfixtures', 'refs'}
SOURCE_EXCLUDE_SUFFIX = {'.wasm', '.mkv', '.mp4', '.m2ts', '.mov', '.ts', '.hevc', '.yuv', '.out', '.ref', '.wout', '.log'}

def make_source_archive(dest):
    """Corresponding source for the WASM binaries (LGPL): `git archive HEAD` of the committed tree (never the dirty working tree)."""
    roots = ['rust', 'web', 'LICENSE', 'NOTICE.md', 'docs']
    tmp = Path(str(dest) + '.tar.tmp')
    subprocess.run(['git', '-C', str(REPO), 'archive', '--format=tar', '--prefix=aether-web-source/', '-o', str(tmp), 'HEAD', *roots], check=True)
    def keep(ti):
        parts = Path(ti.name).parts
        if any(p in SOURCE_EXCLUDE_DIRS for p in parts):
            return None
        if Path(ti.name).suffix in SOURCE_EXCLUDE_SUFFIX and 'testdata' not in parts:
            return None
        if ti.issym() or ti.islnk():
            return None
        ti.uid = ti.gid = 0; ti.uname = ti.gname = ''; ti.mtime = 0
        return ti
    with tarfile.open(tmp) as src, tarfile.open(dest, 'w:gz', compresslevel=9) as tf:
        for ti in src:
            ti = keep(ti)
            if ti is not None:
                tf.addfile(ti, src.extractfile(ti) if ti.isfile() else None)
    tmp.unlink()


LICENSE_FILE = re.compile(r'^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)', re.I)

def third_party(legal):
    """Licence files of every crate linked into aether_wasm.wasm (normal dependencies for wasm32), plus an index."""
    toolchain = os.environ.get('AETHER_RUST_TOOLCHAIN') or ('/tmp/aether-rust-toolchain' if os.path.isdir('/tmp/aether-rust-toolchain') else '')
    env = dict(os.environ)
    if toolchain:
        env.update(PATH=f'{toolchain}/cargo/bin:' + env['PATH'], RUSTUP_HOME=f'{toolchain}/rustup', CARGO_HOME=f'{toolchain}/cargo')
    try:
        raw = subprocess.run(['cargo', 'metadata', '--format-version', '1', '--locked', '--offline', '--filter-platform', 'wasm32-unknown-unknown'],
                             cwd=REPO / 'rust', env=env, capture_output=True, text=True, check=True).stdout
    except Exception as e:
        print(f'WARNING: third-party licence scan skipped ({e}); legal/third-party is NOT in this build', file=sys.stderr)
        return False
    m = json.loads(raw)
    pk = {p['id']: p for p in m['packages']}
    res = {n['id']: n for n in m['resolve']['nodes']}
    root = next(p['id'] for p in m['packages'] if p['name'] == 'aether-wasm')
    seen, stack = set(), [root]
    while stack:
        i = stack.pop()
        if i in seen: continue
        seen.add(i)
        stack += [d['pkg'] for d in res[i]['deps'] if any(k['kind'] is None for k in d['dep_kinds'])]
    index = []
    for i in sorted(seen, key=lambda i: pk[i]['name']):
        p = pk[i]
        if p['source'] is None:
            continue  # workspace and vendored path crates: covered by their NOTICE files and the repo LICENSE
        d = legal / 'third-party' / f"{p['name']}-{p['version']}"
        d.mkdir(parents=True, exist_ok=True)
        for f in sorted(Path(p['manifest_path']).parent.iterdir()):
            if f.is_file() and LICENSE_FILE.match(f.name):
                shutil.copy2(f, d / f.name)
        index.append({'name': p['name'], 'version': p['version'], 'license': p['license'], 'repository': p['repository'],
                      'source': p['source'].split('#')[0], 'licenseFiles': sorted(x.name for x in d.iterdir())})
    (legal / 'third-party.json').write_text(json.dumps(index, indent=1) + '\n')
    empty = [x['name'] for x in index if not x['licenseFiles']]
    if empty:
        print('note: no licence file shipped in the crate source of: ' + ', '.join(empty) + ' (licence id is recorded in third-party.json)', file=sys.stderr)
    return True

def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--out', default=str(WEB / 'dist'))
    ap.add_argument('--no-source', action='store_true', help='skip the corresponding-source archive (legal/source tarball)')
    ap.add_argument('--check-only', action='store_true', help='only run the static import scan')
    a = ap.parse_args()

    entry = WEB / 'player' / 'index.html'
    files, problems = scan(entry)
    for w in sorted((WEB / 'wasm').glob('*.wasm')):
        files.add(w.resolve())
    if not (WEB / 'wasm' / 'aether_wasm.wasm').is_file():
        problems.append('web/wasm/aether_wasm.wasm missing (run sh rust/build.sh)')
    if problems:
        print('STATIC SCAN FAILED:', *problems, sep='\n  ', file=sys.stderr)
        return 1
    rels = sorted(f.relative_to(WEB).as_posix() for f in files)
    print(f'static scan: {len(rels)} runtime files, every import / worker URL resolves')
    if a.check_only:
        print(*rels, sep='\n')
        return 0

    hashes = {r: sha256(WEB / r) for r in rels}
    build_id = hashlib.sha256('\n'.join(f'{r}:{hashes[r]}' for r in rels).encode()).hexdigest()[:12]
    out = Path(a.out)
    if out.exists():
        shutil.rmtree(out)
    asset_root = out / 'assets' / build_id
    for r in rels:
        dst = asset_root / r
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(WEB / r, dst)
    prefix = f'assets/{build_id}/'

    # Entry page: the player's index.html with its asset paths moved under the build directory and the
    # isolation bootstrap in front of the app script.
    html = entry.read_text(encoding='utf-8')
    html = html.replace('href="player.css"', f'href="{prefix}player/player.css"')
    new = f'<script src="coi.js" data-app="{prefix}player/js/app.js"></script>'
    html, n = re.subn(r'<script type="module" src="js/app\.js"></script>', new, html)
    if n != 1 or 'src="js/' in html or 'href="player.css"' in html:
        print('index.html rewrite failed: the page changed shape, update assemble.py', file=sys.stderr)
        return 1
    html = html.replace('<title>', '<link rel="icon" href="data:,">\n<title>', 1)  # no favicon request
    # The source tree links ../LICENSES.md; in the built site the page sits at the root and the file under legal/.
    html = html.replace('href="../LICENSES.md"', 'href="legal/LICENSES.md"')
    html = html.replace('</body>', '<a href="legal/LICENSES.md" style="position:fixed;right:8px;bottom:4px;font:11px system-ui;color:#8b909a">Licences and source</a>\n</body>', 1)
    (out / 'index.html').write_text(html, encoding='utf-8')
    (asset_root / 'player' / 'index.html').unlink()  # the rewritten root copy replaces it; the original would carry broken relative paths

    for name in ('coi.js', 'coi-sw.js'):
        shutil.copy2(HERE / 'templates' / name, out / name)
    shutil.copy2(HERE / 'hosts' / '_headers', out / '_headers')
    shutil.copy2(HERE / 'hosts' / 'vercel.json', out / 'vercel.json')
    (out / '.nojekyll').write_text('')
    (out / '.gitignore').write_text('*\n')

    # Legal: licence texts, inventory, per-crate notices, and the corresponding source.
    legal = out / 'legal'; legal.mkdir()
    shutil.copy2(REPO / 'LICENSE', legal / 'LICENSE')
    shutil.copy2(WEB / 'LICENSES.md', legal / 'LICENSES.md')
    notices = {
        'aether-ac3': REPO / 'rust/aether-ac3/NOTICE', 'aether-vc1': REPO / 'rust/aether-vc1/NOTICE',
        'aether-vc1-LICENSE': REPO / 'rust/aether-vc1/LICENSE', 'aether-joc': REPO / 'rust/aether-joc/NOTICE',
        'aether-hevc': REPO / 'rust/aether-hevc/NOTICE', 'rust_h265-MIT': REPO / 'rust/aether-hevc/vendor/rust_h265/LICENSE-MIT',
        'rust_h265-APACHE': REPO / 'rust/aether-hevc/vendor/rust_h265/LICENSE-APACHE',
        'dca': REPO / 'rust/aether-audio/vendor/dca/NOTICE', 'dca-LICENSE': REPO / 'rust/aether-audio/vendor/dca/LICENSE',
        'hrtf-kemar': REPO / 'rust/aether-audio/src/output/NOTICE-HRTF',
    }
    for k, p in notices.items():
        if p.is_file():
            shutil.copy2(p, legal / f'NOTICE-{k}.txt')
        else:
            print(f'warning: notice file missing: {p}', file=sys.stderr)
    tp = third_party(legal)
    commit = git('rev-parse', 'HEAD'); dirty = bool(git('status', '--porcelain', '--', 'rust', 'web'))
    if dirty:
        print('WARNING: working tree under rust/ or web/ has uncommitted or untracked changes; the source archive is built from the committed HEAD and will not contain them', file=sys.stderr)
    if not a.no_source:
        src_name = f'aether-web-source-{build_id}.tar.gz'
        (out / 'source').mkdir()
        make_source_archive(out / 'source' / src_name)
    (out / 'SOURCE.txt').write_text(
        'Corresponding source of the WebAssembly modules and JavaScript in this site (LGPL).\n'
        f'Build id: {build_id}\nRepository commit: {commit} (working tree {"had uncommitted changes, which are NOT in the archive" if dirty else "clean"})\n'
        + ('Archive: source/' + src_name + ' (git archive of that commit; rust/ workspace, web/ tree, LICENSE, docs). ' if not a.no_source else 'Archive: not included in this build. ')
        + '\nRebuild the WASM from the archive (its test fixtures are left out, so skip `cargo test`):\n'
        '  cd rust && cargo build -p aether-wasm --release --locked --target wasm32-unknown-unknown   # -> web/wasm/aether_wasm.wasm\n'
        '  cd rust/aether-hevc && sh build-wasm.sh   # dist/aether_hevc_simd.min.wasm -> web/exact/wasm/aether_hevc_simd.wasm\n'
        'Then python3 web/dist-tools/assemble.py. Full flow: web/README.md and docs/deploy.md.\n'
        'The JavaScript under assets/ is shipped unminified and unmodified from web/.\n')

    # Manifest of every file in the site (except itself).
    entries = []
    for p in sorted(out.rglob('*')):
        if p.is_file() and p.name not in ('manifest.json',):
            rel = p.relative_to(out).as_posix()
            entries.append({'path': rel, 'size': p.stat().st_size, 'sha256': sha256(p)})
    runtime = [e for e in entries if e['path'].startswith('assets/') or e['path'] in ('index.html', 'coi.js', 'coi-sw.js')]
    manifest = {'buildId': build_id, 'gitCommit': commit, 'gitDirty': dirty, 'thirdPartyLicenses': tp, 'assetBase': prefix,
                'totalBytes': sum(e['size'] for e in entries), 'runtimeBytes': sum(e['size'] for e in runtime), 'files': entries}
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=1) + '\n')

    # Size report.
    def gz(path):
        return len(gzip.compress((out / path).read_bytes(), 6)) if not path.endswith(('.gz',)) else (out / path).stat().st_size
    rt_gz = sum(gz(e['path']) for e in runtime)
    print(f'build id {build_id}   commit {commit and commit[:10]}{" (dirty)" if dirty else ""}')
    for e in sorted(runtime, key=lambda e: -e['size'])[:8]:
        print(f'  {e["size"]:>10,}  {e["path"]}')
    print(f'runtime: {len(runtime)} files, {manifest["runtimeBytes"]:,} bytes ({manifest["runtimeBytes"]/1048576:.2f} MiB), about {rt_gz/1048576:.2f} MiB gzip-6')
    print(f'whole site incl. legal and source: {len(entries)} files, {manifest["totalBytes"]:,} bytes ({manifest["totalBytes"]/1048576:.2f} MiB)')
    return 0

if __name__ == '__main__':
    sys.exit(main())
