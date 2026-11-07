#!/usr/bin/env python3
"""Build every icon file from the hand-editable pixel grids in assets/icon/art.

The grids are the source of truth, so edit those and re-run this script:

    pip install pillow
    python assets/icon/build_icon.py

Sizes come from integer scaling only, which keeps the pixels crisp:
16 and 24 have their own hand-tuned grids, 48 is the 24 grid doubled, and
everything from 64 up is the 32 grid scaled.
"""
from __future__ import annotations

import io
import struct
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
ART = HERE / "art"

PALETTE = {
    "k": (42, 36, 24),      # outline
    "c": (217, 207, 174),   # beige plastic
    "l": (239, 231, 203),   # plastic highlight
    "d": (168, 157, 124),   # plastic shadow
    "e": (74, 68, 51),      # screen bezel
    "s": (31, 111, 184),    # screen
    "S": (26, 95, 160),     # screen, darker scanline
    "n": (12, 53, 96),      # drop shadow on the screen
    "G": (134, 220, 255),   # screen glare
    "w": (245, 233, 201),   # stone W
    "W": (196, 174, 128),   # stone W, shaded edge
    "h": (255, 79, 134),    # heart
    "H": (255, 194, 212),   # heart highlight
    "L": (93, 255, 122),    # power light
    "b": (23, 23, 29),      # bandana
    "p": (217, 217, 217),   # bandana paisley dots
}

# The CRT powered off, for the hidden "cleaning the monitor" gag in the app.
OFF_REMAP = {
    "s": (8, 10, 12),
    "S": (5, 7, 9),
    "n": (8, 10, 12),
    "G": (40, 46, 52),
    "w": (8, 10, 12),
    "W": (8, 10, 12),
    "h": (8, 10, 12),
    "H": (8, 10, 12),
    "L": (40, 60, 44),
}


def load_grid(name: str) -> list[str]:
    rows = [r.rstrip("\n") for r in (ART / name).read_text(encoding="ascii").splitlines()]
    rows = [r for r in rows if r and not r.startswith("#")]
    size = len(rows)
    bad = [i for i, r in enumerate(rows) if len(r) != size]
    if bad:
        raise SystemExit(f"{name}: rows {bad} are not {size} wide")
    unknown = {ch for r in rows for ch in r} - set(PALETTE) - {"."}
    if unknown:
        raise SystemExit(f"{name}: unknown pixels {sorted(unknown)}")
    return rows


def render(rows: list[str], scale: int = 1, remap: dict | None = None) -> Image.Image:
    size = len(rows)
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    px = img.load()
    for y, row in enumerate(rows):
        for x, ch in enumerate(row):
            if ch == ".":
                continue
            rgb = (remap or {}).get(ch, PALETTE[ch])
            px[x, y] = (*rgb, 255)
    if scale != 1:
        img = img.resize((size * scale, size * scale), Image.NEAREST)
    return img


def png_bytes(img: Image.Image) -> bytes:
    buf = io.BytesIO()
    img.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def dib_bytes(img: Image.Image) -> bytes:
    """A 32-bit BMP body for ICO entries under 256px, which every Windows tool reads."""
    w, h = img.size
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = bytearray()
    mask = bytearray()
    row_bytes = ((w + 31) // 32) * 4
    data = img.load()
    for y in range(h - 1, -1, -1):
        bits = 0
        row = bytearray()
        for x in range(w):
            r, g, b, a = data[x, y]
            pixels += bytes((b, g, r, a))
            bits = (bits << 1) | (0 if a else 1)
            if x % 8 == 7:
                row.append(bits & 0xFF)
                bits = 0
        if w % 8:
            row.append((bits << (8 - w % 8)) & 0xFF)
        row += b"\0" * (row_bytes - len(row))
        mask += row
    return header + bytes(pixels) + bytes(mask)


def write_ico(path: Path, images: dict[int, Image.Image]) -> None:
    sizes = sorted(images)
    blobs = [png_bytes(images[s]) if s >= 256 else dib_bytes(images[s]) for s in sizes]
    out = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    for s, blob in zip(sizes, blobs):
        dim = 0 if s >= 256 else s
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    path.write_bytes(out + b"".join(blobs))


def write_icns(path: Path, images: dict[int, Image.Image]) -> None:
    # PNG-based icon types, supported since macOS 10.7.
    types = {
        16: b"icp4",
        32: b"icp5",
        64: b"icp6",
        128: b"ic07",
        256: b"ic08",
        512: b"ic09",
        1024: b"ic10",
    }
    body = b""
    for size, code in types.items():
        blob = png_bytes(images[size])
        body += code + struct.pack(">I", len(blob) + 8) + blob
    path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main() -> None:
    g16 = load_grid("icon16.txt")
    g24 = load_grid("icon24.txt")
    g32 = load_grid("icon32.txt")

    images = {
        16: render(g16),
        24: render(g24),
        32: render(g32),
        48: render(g24, 2),
        64: render(g32, 2),
        128: render(g32, 4),
        256: render(g32, 8),
        512: render(g32, 16),
        1024: render(g32, 32),
    }

    out = HERE / "png"
    out.mkdir(exist_ok=True)
    for size, img in images.items():
        (out / f"icon-{size}.png").write_bytes(png_bytes(img))
    (out / "icon-off-32.png").write_bytes(png_bytes(render(g32, 1, OFF_REMAP)))

    write_ico(HERE / "icon.ico", {s: images[s] for s in (16, 24, 32, 48, 64, 128, 256)})
    write_icns(HERE / "icon.icns", images)
    print("built", sorted(images), "plus icon.ico and icon.icns")


if __name__ == "__main__":
    main()
