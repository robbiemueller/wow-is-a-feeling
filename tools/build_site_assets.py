#!/usr/bin/env python3
"""Copy and optimize images for the landing page, and draw the social preview.

    pip install pillow
    python tools/build_site_assets.py [path/to/screenshot.png]
"""
from __future__ import annotations

import shutil
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site" / "assets"
SKIN = ROOT / "assets" / "skin"
ICON = ROOT / "assets" / "icon" / "png"
FONTS = ROOT / "assets" / "fonts"


def tiled(tile: Image.Image, w: int, h: int) -> Image.Image:
    out = Image.new("RGB", (w, h))
    for y in range(0, h, tile.height):
        for x in range(0, w, tile.width):
            out.paste(tile, (x, y))
    return out


def shadowed(d: ImageDraw.ImageDraw, xy, text, font, fill, offset=2):
    d.text((xy[0] + offset, xy[1] + offset), text, font=font, fill=(0, 0, 0))
    d.text(xy, text, font=font, fill=fill)


def social_preview() -> Image.Image:
    w, h = 1280, 640
    stone = Image.open(SKIN / "stone.png").convert("RGB")
    img = tiled(stone, w, h).convert("RGBA")
    logo = Image.open(ICON / "icon-32.png").convert("RGBA").resize((352, 352), Image.NEAREST)
    img.alpha_composite(logo, (70, 144))

    edge = Image.open(SKIN / "edge.png").convert("RGBA")
    for x in range(0, w, edge.width):
        img.alpha_composite(edge, (x, 0))
        img.alpha_composite(edge.transpose(Image.FLIP_TOP_BOTTOM), (x, h - edge.height))
    edge_v = edge.rotate(90, expand=True)
    for y in range(0, h, edge_v.height):
        img.alpha_composite(edge_v, (0, y))
        img.alpha_composite(edge_v.transpose(Image.FLIP_LEFT_RIGHT), (w - edge_v.width, y))
    corner = Image.open(SKIN / "corner.png").convert("RGBA")
    img.alpha_composite(corner, (0, 0))
    img.alpha_composite(corner.transpose(Image.FLIP_LEFT_RIGHT), (w - corner.width, 0))
    img.alpha_composite(corner.transpose(Image.FLIP_TOP_BOTTOM), (0, h - corner.height))
    img.alpha_composite(corner.transpose(Image.ROTATE_180), (w - corner.width, h - corner.height))

    d = ImageDraw.Draw(img)
    title = ImageFont.truetype(str(FONTS / "Almendra-Bold.ttf"), 84)
    sub = ImageFont.truetype(str(FONTS / "Marcellus-Regular.ttf"), 38)
    small = ImageFont.truetype(str(FONTS / "Marcellus-Regular.ttf"), 30)
    shadowed(d, (470, 190), "WoW is a Feeling", title, (255, 209, 0), 3)
    shadowed(d, (472, 296), "A free WoW Forever addon manager", sub, (232, 224, 204))
    shadowed(d, (472, 350), "with a vanilla feel", sub, (232, 224, 204))
    shadowed(d, (472, 420), "Windows, macOS and Linux", small, (201, 162, 39))
    return img.convert("RGB")


def main() -> None:
    SITE.mkdir(parents=True, exist_ok=True)
    Image.open(SKIN / "stone.png").convert("RGB").save(SITE / "stone.jpg", quality=82, optimize=True)
    Image.open(SKIN / "parchment.png").convert("RGB").save(SITE / "parchment.jpg", quality=82, optimize=True)
    for name in ("corner.png", "ring.png", "edge.png"):
        shutil.copy(SKIN / name, SITE / name)
    Image.open(SKIN / "edge.png").rotate(90, expand=True).save(SITE / "edge-v.png", optimize=True)
    for name in ("icon-32.png", "icon-64.png", "icon-128.png"):
        shutil.copy(ICON / name, SITE / name)
    for name in ("Almendra-Bold.ttf", "Marcellus-Regular.ttf", "FiraSansCondensed-Regular.ttf"):
        shutil.copy(FONTS / name, SITE / name)
    social_preview().save(SITE / "social-preview.png", optimize=True)
    if len(sys.argv) > 1:
        Image.open(sys.argv[1]).convert("RGB").save(SITE / "screenshot.png", optimize=True)
    for f in sorted(SITE.iterdir()):
        print(f"{f.name}: {f.stat().st_size // 1024} KB")


if __name__ == "__main__":
    main()
