#!/usr/bin/env python3
"""Convert an XWD (X Window Dump) file to PNG without any image library.

Slots into the headless chrome review loop: `xwd -root` captures the whole
Xvfb screen, including popovers and menus, which the in-app widget snapshot
cannot reach. Usage: xwd2png.py <in.xwd> <out.png>
"""

import struct
import sys
import zlib


def read_xwd(path):
    data = open(path, "rb").read()
    # xwd writes the header in the server's byte order; detect it instead of
    # trusting the header_size field, which is itself order dependent.
    for order in (">", "<"):
        header = struct.unpack(order + "25I", data[:100])
        if header[0] >= 100 and header[1] == 7 and 0 < header[4] <= 8192:
            break
    else:
        raise SystemExit("unrecognized XWD header")
    header_size, version, pixmap_format, depth, width, height = header[:6]
    bpp = header[11]
    bytes_per_line = header[12]
    ncolors = header[19]
    assert pixmap_format == 2, f"unsupported pixmap format {pixmap_format}"
    # Rows are padded to bytes_per_line and pixels are bits_per_pixel wide,
    # which is 32 on a depth-24 server: walking width*3 bytes per row shears
    # the image, so both values come from the header.
    stride = bytes_per_line or width * (bpp // 8)
    offset = header_size + ncolors * 12
    rows = []
    for y in range(height):
        start = offset + y * stride
        rows.append(data[start : start + width * (bpp // 8)])
    return width, height, depth, bpp, b"".join(rows)


def to_rgb(width, height, depth, bpp, pixels):
    out = bytearray()
    if bpp == 32:
        for i in range(0, len(pixels), 4):
            b, g, r = pixels[i], pixels[i + 1], pixels[i + 2]
            out += bytes((r, g, b))
    elif bpp == 24:
        for i in range(0, len(pixels), 3):
            b, g, r = pixels[i], pixels[i + 1], pixels[i + 2]
            out += bytes((r, g, b))
    elif bpp == 16:
        for i in range(0, len(pixels), 2):
            (v,) = struct.unpack_from("<H", pixels, i)
            r = ((v >> 11) & 0x1F) * 255 // 31
            g = ((v >> 5) & 0x3F) * 255 // 63
            b = (v & 0x1F) * 255 // 31
            out += bytes((r, g, b))
    else:
        raise SystemExit(f"unsupported bpp {bpp}")
    return bytes(out)


def write_png(path, width, height, rgb):
    raw = b"".join(
        b"\x00" + rgb[y * width * 3 : (y + 1) * width * 3] for y in range(height)
    )

    def chunk(tag, payload):
        body = tag + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 6))
    png += chunk(b"IEND", b"")
    open(path, "wb").write(png)


if __name__ == "__main__":
    src, dst = sys.argv[1], sys.argv[2]
    w, h, depth, bpp, px = read_xwd(src)
    write_png(dst, w, h, to_rgb(w, h, depth, bpp, px))
    print(f"{dst} {w}x{h} depth={depth} bpp={bpp}")
