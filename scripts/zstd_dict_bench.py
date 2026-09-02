#!/usr/bin/env python3
"""WO6-8: plain zstd vs per-project trained-dictionary zstd on project-file-like data.

Question (WO6-8): do per-project trained zstd dictionaries buy enough bytes to
justify the machinery (ADR-0013: dictionaries as CAS objects fetched on demand),
given cairn already dedups at chunk granularity?

Method (honest, fixed):
- Four synthetic corpus classes shaped like real NLE project payloads:
  blend-like binary (DNA-style struct records, 60% repetitive), XML project text
  (prproj-style), float64 arrays (incompressible), random (already compressed).
- Files are split TRAIN/TEST deterministically (seed 42): the dictionary is
  trained ONLY on TRAIN files (the per-project distribution scenario — the dict
  ships with the project and every device shares it), then TEST files are
  compressed with (a) plain zstd -3 and (b) zstd -3 + dict.
- Small files (<16 KiB) are reported separately: that is where dictionaries
  plausibly matter most, and also where cairn's chunk-reuse does NOT help
  (sub-chunk files carry no cross-file reuse).
- File-level compression only: cairn compresses per CHUNK (ADR-0004), so
  large-file ratios here are an UPPER bound for dict benefit at chunk level;
  the dedup side of the ledger is the golden-corpus reuse ratios (STATUS).

Requires: python3 + zstandard (pip install zstandard). No repo code touched.
"""
import os
import struct
import time

import zstandard as zstd

SEED = 42
TRAIN_FRAC = 0.5
SMALL = 16 * 1024


def blend_like(n, seed=0):
    """Synthetic .blend-ish: SDNA blocks + mesh records; structure repeats, vertex
    data is per-file noise (same NLE, different shots)."""
    import random
    rng = random.Random(9000 + seed)
    out = bytearray(b"BLENDER")
    out += b"_V001"
    rec = 0
    while len(out) < n:
        rec += 1
        out += struct.pack("<4sI", b"ME\x00\x00", 96 + (rec % 7) * 12)
        out += b"OBMesh" + bytes([(rec * 7 + seed) & 0xFF]) * 8
        for v in range(64):
            out += struct.pack("<fff", rng.random() * 97, rng.random() * 89, rng.random() * 83)
        out += b"DNA1" + b"struct Mesh { int totvert; int totface; }" * 3
        if rec % 5 == 0:
            out += rng.randbytes(256)  # packed preview image
    return bytes(out[:n])


def xml_text(n, seed=0):
    """Synthetic prproj-ish XML: shared element vocabulary, per-file edit values."""
    import random
    rng = random.Random(500 + seed)
    parts = []
    i = 0
    while sum(len(p) for p in parts) < n:
        i += 1
        parts.append(
            f'<Clip id="c{i}" path="sequences/A001/shot_{rng.randint(0, 39)}.mov" '
            f'inPoint="{rng.randint(0, 997)}.3" outPoint="{rng.randint(0, 997)}.7" '
            f'opacity="1.0" speed="1.0"><Effect name="Transform" '
            f'params="{rng.randint(0, 13)},{rng.randint(0, 7)},1.0"/></Clip>'.encode()
        )
    return b"".join(parts)[:n]


def floats(n, seed=0):
    """True random-mantissa float64s (PCM/mesh-vertex-like): incompressible bits,
    distinct per file."""
    import random
    rng = random.Random(1234 + seed)
    out = bytearray()
    while len(out) < n:
        out += struct.pack("<d", rng.random() * 1e6)
    return bytes(out[:n])


def rand_bytes(n, seed=0):
    import random
    rng = random.Random(7777 + seed)
    return rng.randbytes(n)


CLASSES = {
    "blend-like": blend_like,
    "xml-project": xml_text,
    "float64-raw": floats,
    "random": rand_bytes,
}
FILE_SIZE = 64 * 1024
FILES_PER_CLASS = 24


def main():
    rows = []
    small_rows = []
    for cname, gen in CLASSES.items():
        files = [gen(FILE_SIZE + (i * 997) % 8192, seed=i) for i in range(FILES_PER_CLASS)]
        files = [f[:FILE_SIZE] for f in files]
        split = max(1, int(len(files) * TRAIN_FRAC))
        train, test = files[:split], files[split:]

        # dictionary: trained on the TRAIN half only (per-project scenario)
        dict_bytes = zstd.train_dictionary(110 * 1024, train, level=3).as_bytes()
        dc = zstd.ZstdCompressor(
            dict_data=zstd.ZstdCompressionDict(dict_bytes, dict_type=zstd.DICT_TYPE_RAWCONTENT),
            level=3,
        )
        plain = zstd.ZstdCompressor(level=3)

        t0 = time.perf_counter()
        plain_bytes = sum(len(plain.compress(f)) for f in test)
        t_plain = time.perf_counter() - t0
        t0 = time.perf_counter()
        dict_bytes_total = sum(len(dc.compress(f)) for f in test)
        t_dict = time.perf_counter() - t0
        raw = sum(len(f) for f in test)

        rows.append((cname, raw, plain_bytes, dict_bytes_total, t_plain, t_dict))

        # small-file subset: dict-trained on the same class, tested on fresh smalls
        smalls = [gen(2048, seed=1000 + i) for i in range(40)]
        p_small = sum(len(plain.compress(s)) for s in smalls)
        d_small = sum(len(dc.compress(s)) for s in smalls)
        small_rows.append((cname, len(smalls) * 2048, p_small, d_small))

    print(f"{'class':<13} {'raw KiB':>9} {'plain KiB':>10} {'dict KiB':>10} {'plain ratio':>12} {'dict ratio':>11} {'saving':>8}")
    tot_p = tot_d = tot_r = 0
    for cname, raw, p, d, tp, td in rows:
        tot_p += p; tot_d += d; tot_r += raw
        print(f"{cname:<13} {raw//1024:>9} {p//1024:>10} {d//1024:>10} {raw/p:>12.2f}x {raw/d:>10.2f}x {100*(p-d)/p:>7.1f}%")
    print(f"{'TOTAL':<13} {tot_r//1024:>9} {tot_p//1024:>10} {tot_d//1024:>10} {tot_r/tot_p:>12.2f}x {tot_r/tot_d:>10.2f}x {100*(tot_p-tot_d)/tot_p:>7.1f}%")
    print()
    print("small files (<16KiB) — where chunk-reuse does not help:")
    print(f"{'class':<13} {'raw KiB':>9} {'plain KiB':>10} {'dict KiB':>10} {'saving':>8}")
    for cname, raw, p, d in small_rows:
        print(f"{cname:<13} {raw//1024:>9} {p//1024:>10} {d//1024:>10} {100*(p-d)/p:>7.1f}%")


if __name__ == "__main__":
    main()
