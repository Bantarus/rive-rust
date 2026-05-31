#!/usr/bin/env python3
"""M2.0 PNG pixel-diff helper (stdlib + ffmpeg decode).

Decodes two PNGs to raw RGBA via ffmpeg and reports per-channel deltas, mirroring
the M1b close-out method (max-delta / changed-byte counts at a ≤2-LSB tolerance).
Usage: png_diff.py A.png B.png
Exit 0 always; prints a summary line. Pure stdlib + ffmpeg (no PIL/numpy).
"""
import subprocess
import sys
import tempfile
import os


def decode(path):
    """ffmpeg-decode a PNG to (width, height, raw RGBA bytes)."""
    # Probe dimensions.
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v:0",
         "-show_entries", "stream=width,height", "-of", "csv=p=0:s=x", path],
        capture_output=True, text=True, check=True)
    w, h = (int(x) for x in probe.stdout.strip().split("x"))
    with tempfile.NamedTemporaryFile(suffix=".raw", delete=False) as tf:
        raw_path = tf.name
    try:
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-i", path,
             "-f", "rawvideo", "-pix_fmt", "rgba", raw_path],
            check=True)
        with open(raw_path, "rb") as f:
            data = f.read()
    finally:
        os.unlink(raw_path)
    return w, h, data


def main():
    if len(sys.argv) != 3:
        print("usage: png_diff.py A.png B.png", file=sys.stderr)
        return
    a_path, b_path = sys.argv[1], sys.argv[2]
    aw, ah, a = decode(a_path)
    bw, bh, b = decode(b_path)
    if (aw, ah) != (bw, bh):
        print(f"DIMENSION MISMATCH: {a_path} {aw}x{ah} vs {b_path} {bw}x{bh}")
        return
    if len(a) != len(b):
        print(f"BYTE-LEN MISMATCH: {len(a)} vs {len(b)} (same dims?!)")
        return
    n = len(a)
    max_d = 0
    changed = 0
    sum_d = 0
    hist = {}  # delta -> count, for deltas >= 1
    for x, y in zip(a, b):
        d = x - y if x > y else y - x
        if d:
            changed += 1
            sum_d += d
            if d > max_d:
                max_d = d
            hist[d] = hist.get(d, 0) + 1
    px = n // 4
    mean = sum_d / n if n else 0.0
    print(f"{os.path.basename(a_path)} vs {os.path.basename(b_path)}: "
          f"{aw}x{ah} ({px} px, {n} bytes)")
    print(f"  max per-channel delta = {max_d} / 255")
    print(f"  changed bytes = {changed} ({100.0*changed/n:.4f}%), mean delta = {mean:.5f}")
    if hist:
        top = sorted(hist.items())[:8]
        print(f"  delta histogram (delta:count) = {top}")
    verdict = "MATCH (<=2 LSB)" if max_d <= 2 else (
        "CLOSE (<=4 LSB)" if max_d <= 4 else "DIFFERS")
    print(f"  verdict: {verdict}")


if __name__ == "__main__":
    main()
