#!/usr/bin/env python3
"""Generate the app's skin textures. All of this art is original and procedural.

    pip install numpy pillow
    python tools/gen_skin.py

Run it again after changing it. The PNGs in assets/skin are committed, so the
app itself doesn't need Python, numpy or Pillow to build.
"""
from __future__ import annotations

from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

OUT = Path(__file__).resolve().parent.parent / "assets" / "skin"
OUT.mkdir(parents=True, exist_ok=True)
RNG = np.random.default_rng(2004)


def periodic_noise(size: int, beta: float, seed_rng=RNG) -> np.ndarray:
    """Seamless fractal noise: random phases shaped by 1/f^beta, normalized to 0..1."""
    white = seed_rng.normal(size=(size, size))
    spectrum = np.fft.fft2(white)
    fy = np.fft.fftfreq(size)[:, None]
    fx = np.fft.fftfreq(size)[None, :]
    f = np.sqrt(fx * fx + fy * fy)
    f[0, 0] = 1.0
    shaped = np.fft.ifft2(spectrum / (f**beta)).real
    shaped -= shaped.min()
    shaped /= shaped.max()
    return shaped


def lerp(a: np.ndarray, b: np.ndarray, t: np.ndarray) -> np.ndarray:
    return a + (b - a) * t[..., None]


def save(arr: np.ndarray, name: str) -> None:
    img = Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))
    img.save(OUT / name, optimize=True)
    print(f"{name}: {img.size[0]}x{img.size[1]}, {(OUT / name).stat().st_size} bytes")


def stone(size: int = 256) -> None:
    """Dark slate for the frame: broad clouds plus fine grain and thin cracks."""
    clouds = periodic_noise(size, 1.5)
    mid = periodic_noise(size, 1.0)
    grain = periodic_noise(size, 0.25)
    t = np.clip(0.55 * clouds + 0.3 * mid + 0.15 * grain, 0, 1)
    t = (t - t.mean()) * 1.5 + 0.5
    t = np.clip(t, 0, 1)
    dark = np.array([17.0, 15.0, 13.0])
    light = np.array([50.0, 45.0, 39.0])
    rgb = lerp(dark, light, t)
    ridges = 1.0 - np.abs(periodic_noise(size, 1.2) * 2.0 - 1.0)
    cracks = np.clip((ridges - 0.985) * 40.0, 0, 1)
    rgb *= 1.0 - 0.3 * cracks[..., None]
    save(rgb, "stone.png")


def parchment(size: int = 512) -> None:
    """Aged paper: warm mottling, blotches, faint fibers."""
    clouds = periodic_noise(size, 1.6)
    mottling = periodic_noise(size, 1.0)
    grain = periodic_noise(size, 0.2)
    blotch = np.clip((periodic_noise(size, 1.9) - 0.62) * 4.0, 0, 1)
    fibers = periodic_noise(size, 0.6)
    stretched = np.roll(np.apply_along_axis(lambda r: np.convolve(np.r_[r, r[:15]], np.ones(16) / 16, "valid"), 1, fibers), 8, axis=1)
    stretched = (stretched - stretched.min()) / (stretched.max() - stretched.min())
    t = np.clip(0.5 * clouds + 0.3 * mottling + 0.08 * grain + 0.12 * stretched, 0, 1)
    t = np.clip((t - t.mean()) * 1.3 + 0.55, 0, 1)
    dark = np.array([196.0, 166.0, 112.0])
    light = np.array([240.0, 222.0, 176.0])
    rgb = lerp(dark, light, t)
    rgb = lerp(rgb, np.array([170.0, 132.0, 82.0]), blotch * 0.35)
    save(rgb, "parchment.png")


def gold_ramp(t: np.ndarray) -> np.ndarray:
    """Polished gold, from shadow through body to highlight. t is 0..1."""
    stops = np.array([0.0, 0.22, 0.45, 0.62, 0.8, 1.0])
    colors = np.array(
        [
            [58, 38, 10],
            [122, 86, 26],
            [190, 146, 54],
            [232, 196, 96],
            [255, 236, 170],
            [255, 250, 225],
        ],
        dtype=float,
    )
    return np.stack([np.interp(t, stops, colors[:, c]) for c in range(3)], axis=-1)


def bevel_shade(height: np.ndarray, strength: float = 2.2) -> np.ndarray:
    """Light from the upper left, the classic UI bevel. Returns 0..1."""
    gy, gx = np.gradient(height)
    shade = 0.5 - (gx + gy) * strength
    return np.clip(shade, 0, 1)


def supersample_mask(size: tuple[int, int], draw_fn, ss: int = 4) -> np.ndarray:
    img = Image.new("L", (size[0] * ss, size[1] * ss), 0)
    draw_fn(ImageDraw.Draw(img), ss)
    img = img.resize(size, Image.LANCZOS)
    return np.asarray(img, dtype=float) / 255.0


def render_gold(mask: np.ndarray, blur: float = 1.2, strength: float = 2.6, base: float = 0.46) -> np.ndarray:
    """Turn a coverage mask into beveled, slightly worn gold with alpha."""
    h, w = mask.shape
    height = np.asarray(
        Image.fromarray((mask * 255).astype(np.uint8)).filter(ImageFilter.GaussianBlur(blur)), dtype=float
    ) / 255.0
    shade = bevel_shade(height, strength)
    wear = (periodic_noise(max(h, w), 0.9)[:h, :w] - 0.5) * 0.10
    t = np.clip(base + (shade - 0.5) * 1.05 + wear, 0, 1)
    rgb = gold_ramp(t)
    return np.dstack([rgb, mask * 255.0])


def frame_corner(size: int = 96) -> None:
    """Top-left corner ornament, mirrored at runtime for the other three."""

    def shape(d: ImageDraw.ImageDraw, ss: int) -> None:
        s = ss
        t = 12 * s
        # The L of the frame itself.
        d.rectangle([0, 0, size * s, t], fill=255)
        d.rectangle([0, 0, t, size * s], fill=255)
        # A square corner plate with a clipped inner edge.
        p = 34 * s
        d.polygon([(0, 0), (p, 0), (p, p - 10 * s), (p - 10 * s, p), (0, p)], fill=255)
        # Spur pointing along each edge.
        d.polygon([(p, 3 * s), (p + 26 * s, 6 * s), (p, 12 * s)], fill=255)
        d.polygon([(3 * s, p), (6 * s, p + 26 * s), (12 * s, p)], fill=255)

    def holes(d: ImageDraw.ImageDraw, ss: int) -> None:
        s = ss
        d.ellipse([9 * s, 9 * s, 25 * s, 25 * s], fill=255)

    mask = supersample_mask((size, size), shape)
    gem_area = supersample_mask((size, size), holes)
    gold = render_gold(mask)

    # A dark inset gem in the plate, so the corner reads as an ornament.
    yy, xx = np.mgrid[0:size, 0:size]
    cx, cy, r = 17.0, 17.0, 8.0
    dist = np.sqrt((xx - cx) ** 2 + (yy - cy) ** 2)
    glint = np.clip(1.0 - np.sqrt((xx - cx + 2.5) ** 2 + (yy - cy + 2.5) ** 2) / r, 0, 1)
    gem = np.dstack(
        [
            40 + 150 * glint**2,
            14 + 40 * glint**2,
            16 + 40 * glint**2,
        ]
    )
    ring_shadow = np.clip(1.0 - np.abs(dist - r) / 1.2, 0, 1)
    rgb = gold[..., :3] * (1 - gem_area[..., None]) + gem * gem_area[..., None]
    rgb *= 1 - 0.45 * ring_shadow[..., None]
    save(np.dstack([rgb, gold[..., 3]]), "corner.png")


def frame_edge(length: int = 128, thick: int = 12) -> None:
    """A horizontal length of gold trim that tiles. Rotated at runtime for verticals."""
    height = np.zeros((thick, length))
    profile = np.sin(np.linspace(0.08, np.pi - 0.08, thick)) ** 0.6
    height[:] = profile[:, None]
    engrave = np.zeros_like(height)
    engrave[thick // 2 - 1 : thick // 2 + 1, :] = 0.28
    height -= engrave
    shade = bevel_shade(height, 1.15)
    wear = (periodic_noise(length, 0.8)[: thick, :] - 0.5) * 0.12
    t = np.clip(0.48 + (shade - 0.5) * 1.1 + wear, 0, 1)
    rgb = gold_ramp(t)
    alpha = np.full((thick, length), 255.0)
    alpha[0, :] = 200
    alpha[-1, :] = 200
    save(np.dstack([rgb, alpha]), "edge.png")


def portrait_ring(size: int = 128) -> None:
    """Round gold bezel for the portrait, drawn over the logo."""
    yy, xx = np.mgrid[0:size, 0:size]
    c = (size - 1) / 2
    r = np.sqrt((xx - c) ** 2 + (yy - c) ** 2)
    outer, inner = size / 2 - 1, size / 2 - 13
    mask = np.clip(outer - r + 0.5, 0, 1) * np.clip(r - inner + 0.5, 0, 1)
    height = np.sin(np.clip((r - inner) / (outer - inner), 0, 1) * np.pi) ** 0.7
    shade = bevel_shade(height, 2.0)
    wear = (periodic_noise(size, 0.9) - 0.5) * 0.10
    t = np.clip(0.40 + (shade - 0.5) * 0.95 + wear, 0, 1)
    rgb = gold_ramp(t)
    # Rivets at the four diagonals.
    for ang in (45, 135, 225, 315):
        a = np.deg2rad(ang)
        rx, ry = c + np.cos(a) * (outer + inner) / 2, c + np.sin(a) * (outer + inner) / 2
        dist = np.sqrt((xx - rx) ** 2 + (yy - ry) ** 2)
        cover = np.clip(3.4 - dist, 0, 1)
        hl = np.clip(1 - np.sqrt((xx - rx + 1) ** 2 + (yy - ry + 1) ** 2) / 3.2, 0, 1)
        rivet = gold_ramp(np.clip(0.3 + hl * 0.8, 0, 1))
        rgb = rgb * (1 - cover[..., None]) + rivet * cover[..., None]
    # An inner shadow lip, which sinks the portrait into the frame.
    lip = np.clip(1 - np.abs(r - inner) / 1.5, 0, 1)
    rgb = rgb * (1 - 0.5 * lip[..., None])
    save(np.dstack([rgb, mask * 255]), "ring.png")


def spark(w: int = 32, h: int = 64) -> None:
    """The bright spark at the head of the casting bar."""
    yy, xx = np.mgrid[0:h, 0:w]
    cx, cy = (w - 1) / 2, (h - 1) / 2
    v = np.exp(-(((xx - cx) / (w * 0.14)) ** 2) - ((yy - cy) / (h * 0.42)) ** 2)
    a = np.clip(v * 1.2, 0, 1)
    rgb = np.dstack([np.full_like(v, 255), 225 + 30 * v, 150 + 105 * v])
    save(np.dstack([rgb, a * 255]), "spark.png")


def main() -> None:
    stone()
    parchment()
    frame_corner()
    frame_edge()
    portrait_ring()
    spark()


if __name__ == "__main__":
    main()
