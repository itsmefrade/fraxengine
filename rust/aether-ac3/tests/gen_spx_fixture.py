#!/usr/bin/env python3
"""Writes tiny synthetic E-AC-3 streams that use spectral extension (SPX).

No encoder available to us produces SPX, and none of the sample media uses it,
so this writes valid E-AC-3 frames directly. Every channel is coded with
snroffset = -960, which forces all bit allocation pointers to zero: there are
no mantissas, the baseband is pure dither noise shaped by the exponents, and
the spectral extension region is built from it. That still exercises SPX
strategy/coordinate parsing, band structures, copy/wrap logic, the notch
filter, noise blending and the shared dither generator, and it can be checked
against FFmpeg like any other fixture.

Usage: gen_spx_fixture.py <out dir>
"""
import struct
import sys


class Bits:
    def __init__(self):
        self.bits = []

    def put(self, value, n):
        for i in range(n - 1, -1, -1):
            self.bits.append((value >> i) & 1)

    def pad_to(self, nbits):
        assert len(self.bits) <= nbits, (len(self.bits), nbits)
        self.bits.extend([0] * (nbits - len(self.bits)))

    def to_bytes(self):
        out = bytearray()
        for i in range(0, len(self.bits), 8):
            chunk = self.bits[i : i + 8]
            chunk += [0] * (8 - len(chunk))
            v = 0
            for b in chunk:
                v = (v << 1) | b
            out.append(v)
        return bytes(out)


def crc16(data):
    crc = 0
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x8005) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def frame(acmod, index, words, variant):
    nfch = {1: 1, 2: 2}[acmod]
    b = Bits()
    b.put(0x0B77, 16)
    b.put(0, 2)  # strmtyp: independent
    b.put(0, 3)  # substreamid
    b.put(words - 1, 11)
    b.put(0, 2)  # fscod: 48 kHz
    b.put(3, 2)  # numblkscod: 6 blocks
    b.put(acmod, 3)
    b.put(0, 1)  # lfeon
    b.put(16, 5)  # bsid
    b.put(27, 5)  # dialnorm
    b.put(0, 1)  # compre
    b.put(0, 1)  # mixmdate
    b.put(0, 1)  # infomdate
    b.put(0, 1)  # addbsie
    # audfrm
    b.put(1, 1)  # expstre: AC-3 style exponent strategy
    b.put(0, 1)  # ahte
    b.put(0, 2)  # snroffststr: one offset for the frame
    b.put(0, 1)  # transproce
    b.put(0, 1)  # blkswe
    b.put(0, 1)  # dithflage: dither always on
    b.put(0, 1)  # bamode: default bit allocation parameters
    b.put(0, 1)  # frmfgaincode
    b.put(0, 1)  # dbaflde
    b.put(0, 1)  # skipflde
    b.put(1, 1)  # spxattene
    if acmod > 1:
        for blk in range(6):
            if blk:
                b.put(0, 1)  # cplstre
            if blk == 0:
                b.put(0, 1)  # cplinu
    for blk in range(6):
        for ch in range(nfch):
            b.put(1 if blk == 0 else 0, 2)  # chexpstr: D15 then reuse
    b.put(0, 5 * nfch)  # convexpstr
    b.put(0, 6)  # csnroffst -> snr offset -960 with fsnroffst 0
    b.put(0, 4)
    for ch in range(nfch):
        b.put(1, 1)  # spxattencod present
        b.put((index + 3 * ch + variant) % 32, 5)
    b.put(0, 1)  # blkstrtinfoe

    first_coords = [True] * nfch
    for blk in range(6):
        b.put(0, 1)  # dynrnge
        if blk == 0:
            b.put(1, 1)  # spxinu
            if acmod > 1:
                for ch in range(nfch):
                    b.put(1, 1)  # chinspx
            b.put(variant % 3, 2)  # spxstrtf -> copy start 25 + 12 * n
            b.put(variant % 4, 3)  # spxbegf -> start subband 2 + n
            b.put(3 + variant % 3, 3)  # spxendf -> end subband 5 + n (before >7 fixup)
            b.put(0, 1)  # spxbndstrce: default band structure
        else:
            b.put(0, 1)  # spxstre: keep strategy
        nbands = None
        # Number of bands is derived by the decoder; replicate its arithmetic.
        start_sb = (variant % 4) + 2
        if start_sb > 7:
            start_sb += start_sb - 7
        end_sb = 3 + variant % 3 + 5
        if end_sb > 7:
            end_sb += end_sb - 7
        default = [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1]
        n_sub = end_sb - start_sb
        nbands = n_sub
        for sub in range(1, n_sub):
            if default[start_sb + sub]:
                nbands -= 1
        for ch in range(nfch):
            if first_coords[ch]:
                first_coords[ch] = False
                new = True
            else:
                new = (blk + ch + index) % 3 == 0
                b.put(1 if new else 0, 1)  # spxcoe
            if new:
                b.put((index * 5 + blk + 7 * ch) % 32, 5)  # spxblnd
                b.put((blk + ch) % 4, 2)  # mstrspxco
                for band in range(nbands):
                    b.put((band * 2 + blk + index) % 16, 4)  # spxcoexp
                    b.put((band + ch + index) % 4, 2)  # spxcomant
        if acmod == 2:
            if blk == 0:
                for _ in range(3 if start_sb * 12 + 25 <= 61 else 4):
                    b.put(0, 1)  # rematflg
            else:
                b.put(0, 1)  # rematstr
        if blk == 0:
            end_freq = start_sb * 12 + 25
            groups = (end_freq + 3 - 4) // 3
            for ch in range(nfch):
                b.put(5 + (index + ch) % 5, 4)  # exps[0]
                for _ in range(groups):
                    b.put(62, 7)  # three zero deltas
                b.put(0, 2)  # gainrng
        b.put(0, 1)  # convsnroffste
    b.pad_to(words * 16 - 16)
    data = b.to_bytes()
    crc = crc16(data[2:])
    return data + struct.pack(">H", crc)


def main():
    out = sys.argv[1]
    for name, acmod in (("spx-synth-mono", 1), ("spx-synth-stereo", 2)):
        stream = bytearray()
        for variant in range(4):
            for i in range(15):
                stream += frame(acmod, i, 160, variant)
        open(f"{out}/{name}.eac3", "wb").write(stream)


main()
