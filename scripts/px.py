#!/usr/bin/env python3
"""Sample pixels from a PNG without any image library.

Usage:
  px.py <png> <x> <y> [<x> <y> ...]      # print hex colors
  px.py <png> --row <y> <x0> <x1>        # print a horizontal scan (runs collapsed)
  px.py <png> --box <x0> <y0> <x1> <y1>  # print the most common colors in a box

Built for headless chrome review: it turns "looks off" into a hex value that
can be compared against a theme token.
"""

import collections
import struct
import sys
import zlib


def load(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos = 8
    idat = b""
    w = h = depth = ctype = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        ctag = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        if ctag == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", chunk[:10])
        elif ctag == b"IDAT":
            idat += chunk
        elif ctag == b"IEND":
            break
        pos += 12 + length
    assert depth == 8, f"unsupported bit depth {depth}"
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    raw = zlib.decompress(idat)
    stride = w * channels
    out = bytearray()
    prev = bytearray(stride)
    p = 0
    for _ in range(h):
        f = raw[p]
        p += 1
        line = bytearray(raw[p : p + stride])
        p += stride
        if f == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif f == 3:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif f == 4:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                b = prev[i]
                c = prev[i - channels] if i >= channels else 0
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out += line
        prev = line
    return w, h, channels, bytes(out)


def hexdump(path, points):
    w, h, ch, px = load(path)
    print(f"{path} {w}x{h} channels={ch}")
    for x, y in points:
        if not (0 <= x < w and 0 <= y < h):
            print(f"  ({x},{y}) out of bounds")
            continue
        i = (y * w + x) * ch
        r, g, b = px[i], px[i + 1], px[i + 2]
        print(f"  ({x},{y}) #{r:02x}{g:02x}{b:02x} rgb({r},{g},{b})")


def scan_row(path, y, x0, x1):
    w, h, ch, px = load(path)
    runs = []
    last = None
    for x in range(x0, min(x1, w)):
        i = (y * w + x) * ch
        cur = (px[i], px[i + 1], px[i + 2])
        if cur != last:
            runs.append([1, cur])
            last = cur
        else:
            runs[-1][0] += 1
    print(f"{path} row y={y}")
    for count, (r, g, b) in runs[:40]:
        print(f"  x{count:>4}  #{r:02x}{g:02x}{b:02x}")


def box(path, x0, y0, x1, y1):
    w, h, ch, px = load(path)
    counts = collections.Counter()
    for y in range(y0, min(y1, h)):
        for x in range(x0, min(x1, w)):
            i = (y * w + x) * ch
            counts[(px[i], px[i + 1], px[i + 2])] += 1
    print(f"{path} box ({x0},{y0})-({x1},{y1})")
    for (r, g, b), n in counts.most_common(6):
        print(f"  {n:>6}  #{r:02x}{g:02x}{b:02x}")


if __name__ == "__main__":
    args = sys.argv[1:]
    path = args[0]
    if args[1] == "--row":
        scan_row(path, int(args[2]), int(args[3]), int(args[4]))
    elif args[1] == "--box":
        box(path, *(int(v) for v in args[2:6]))
    else:
        pts = [(int(args[i]), int(args[i + 1])) for i in range(1, len(args), 2)]
        hexdump(path, pts)
