#!/usr/bin/env python3
"""Drop DWARF (.debug_*) and producers custom sections from a wasm module (keeps `name`). Usage: strip-wasm.py in.wasm out.wasm"""
import sys

def leb(b, i):
    r = s = 0
    while True:
        x = b[i]; i += 1
        r |= (x & 0x7f) << s; s += 7
        if not x & 0x80:
            return r, i

def enc(n):
    o = bytearray()
    while True:
        x = n & 0x7f; n >>= 7
        o.append(x | (0x80 if n else 0))
        if not n:
            return bytes(o)

b = open(sys.argv[1], 'rb').read()
out = bytearray(b[:8]); i = 8
while i < len(b):
    sid = b[i]; size, j = leb(b, i + 1); end = j + size
    if sid == 0:
        nlen, k = leb(b, j); name = b[k:k + nlen].decode()
        if name.startswith('.debug') or name in ('producers', 'target_features'):
            i = end; continue
    out += b[i:end]; i = end
open(sys.argv[2], 'wb').write(out)
print(len(b), '->', len(out))
