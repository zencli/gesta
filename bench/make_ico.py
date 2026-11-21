#!/usr/bin/env python3
"""Pack the rendered PNGs into one .ico.

An ICO is a small directory followed by the images, and each entry names its width and height as a
single byte - 256 is written as 0, which is the only value that does not fit. Windows picks the
closest size from the directory, so shipping several is what makes the icon sharp in the taskbar, in
Explorer's list view and on the desktop.
"""
import struct
import sys

# 16 through 256: below 32 is the taskbar, 256 is what Explorer uses for large thumbnails.
SIZES = [16, 32, 48, 64, 128, 256]


def build(paths_with_sizes, out_path):
    images = []
    for size, path in paths_with_sizes:
        with open(path, "rb") as fh:
            images.append((size, fh.read()))

    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    directory = b""
    payload = b""
    for size, data in images:
        dim = 0 if size >= 256 else size
        directory += struct.pack(
            "<BBBBHHII",
            dim, dim,      # width, height; 0 means 256
            0,             # palette count
            0,             # reserved
            1,             # colour planes
            32,            # bits per pixel
            len(data),
            offset,
        )
        payload += data
        offset += len(data)

    with open(out_path, "wb") as fh:
        fh.write(header + directory + payload)
    print(f"{out_path}: {len(images)} sizes, {offset} bytes")


if __name__ == "__main__":
    base = sys.argv[1] if len(sys.argv) > 1 else "assets"
    pairs = [(s, f"{base}/gesta-{s}.png") for s in SIZES]
    build(pairs, f"{base}/gesta.ico")
