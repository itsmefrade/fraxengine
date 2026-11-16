# Regenerates src/ref_inverse_transform.rs from the vendored rust_h265 (run from the repo root).
# The edits are documented in the header it writes; keep this script and that header in step.
import re, sys
src = open('rust/aether-hevc/vendor/rust_h265/src/inverse_transform.rs').read()
body = src[:src.index('#[cfg(test)]\nmod tests {')]
def rep(a, b):
    global body
    assert body.count(a) == 1, a
    body = body.replace(a, b)
rep('// The butterfly versions of the larger sizes are the reference for the\n// zero-skipping matrix versions below, which decoding uses.\n#[cfg(test)]\nimpl_idct!(idct_8x8, 8, tr_8);\n#[cfg(test)]\nimpl_idct!(idct_16x16, 16, tr_16);\n#[cfg(test)]\nimpl_idct!(idct_32x32, 32, tr_32);',
    '// GPU-RECON-REF: the three `#[cfg(test)]` attributes of the vendored file are removed here so the butterfly\n// versions (the reference for the zero-skipping kernels) are compiled and usable as a cross-check.\nimpl_idct!(idct_8x8, 8, tr_8);\nimpl_idct!(idct_16x16, 16, tr_16);\nimpl_idct!(idct_32x32, 32, tr_32);')
rep('#[cfg(test)]\npub(crate) fn reference_idct', 'pub(crate) fn reference_idct')
i = body.index('    if crate::simd::HAVE_SIMD')
j = body.index('    match log2_size {\n        2 => idct_4x4')
body = body[:i] + '    // GPU-RECON-REF: the SIMD branch of the vendored file (tx_simd::idct, bit identical to the scalar\n    // kernels below per the vendor tests) is removed so the scalar code is the reference on every host.\n' + body[j:]
hdr = '''// GPU-RECON-REF: verbatim copy of rust/aether-hevc/vendor/rust_h265/src/inverse_transform.rs (everything above its
// `mod tests`), because that module is private in the vendored crate. The only edits are the three marked
// GPU-RECON-REF (butterfly kernels un-gated from cfg(test), reference_idct un-gated, SIMD branch removed).
// Regenerate with: python3 rust/gpu-recon-ref/regen_copy.py
#![allow(dead_code, clippy::all)]
'''
open('rust/gpu-recon-ref/src/ref_inverse_transform.rs', 'w').write(hdr + body)
