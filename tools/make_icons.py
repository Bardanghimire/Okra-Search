#!/usr/bin/env python3
"""Regenerates every logo asset from one geometry definition.

    python tools/make_icons.py            (needs: pip install pillow numpy)

Design: a macOS-style continuous-corner squircle with a soft vertical okra-brown gradient, one
cream magnifying glass, and a single okra-green accent (the handle). No text, no pod illustration.
Small sizes are rendered natively with slightly heavier strokes so they stay crisp in the tray.
"""
import math
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

ASSETS = Path(__file__).resolve().parent.parent / "assets"

TOP = (132, 92, 66)       # gradient top
BOTTOM = (50, 31, 21)     # gradient bottom
CREAM = (251, 244, 228)
GREEN = (142, 176, 74)

SS = 4  # supersampling factor


def squircle_mask(px: int, margin: float, n: float = 5.0) -> Image.Image:
    """Superellipse |x|^n + |y|^n = 1 (n=5 approximates Apple's continuous-corner shape)."""
    s = px * SS
    half = s * (0.5 - margin)
    c = s / 2
    pts = []
    for i in range(720):
        t = 2 * math.pi * i / 720
        ct, st = math.cos(t), math.sin(t)
        x = c + half * math.copysign(abs(ct) ** (2 / n), ct)
        y = c + half * math.copysign(abs(st) ** (2 / n), st)
        pts.append((x, y))
    m = Image.new("L", (s, s), 0)
    ImageDraw.Draw(m).polygon(pts, fill=255)
    return m


def gradient(px: int) -> Image.Image:
    s = px * SS
    t = np.linspace(0, 1, s)[:, None, None]
    top, bot = np.array(TOP)[None, None, :], np.array(BOTTOM)[None, None, :]
    arr = (top * (1 - t) + bot * t).repeat(s, axis=1)
    return Image.fromarray(arr.astype(np.uint8), "RGB").convert("RGBA")


def glyph(px: int, heavy: float) -> Image.Image:
    """Magnifying glass on a transparent layer, geometry expressed as fractions of the canvas."""
    s = px * SS
    layer = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    cx, cy = 0.43 * s, 0.42 * s          # lens centre
    r_out = 0.215 * s                     # lens outer radius
    ring = 0.062 * s * heavy              # ring thickness
    hw = 0.070 * s * heavy                # handle thickness
    ux = uy = math.sqrt(0.5)
    # Handle first, starting inside the ring band, so the cream ring overlaps its end and the
    # joint follows the ring's curve instead of showing a flat cut.
    x0, y0 = cx + ux * (r_out - ring), cy + uy * (r_out - ring)
    x1, y1 = cx + ux * (r_out + 0.235 * s), cy + uy * (r_out + 0.235 * s)
    d.line([(x0, y0), (x1, y1)], fill=GREEN + (255,), width=round(hw))
    d.ellipse([x1 - hw / 2, y1 - hw / 2, x1 + hw / 2, y1 + hw / 2], fill=GREEN + (255,))
    d.ellipse([cx - r_out, cy - r_out, cx + r_out, cy + r_out], outline=CREAM + (255,), width=round(ring))
    return layer


def render(px: int) -> Image.Image:
    small = px <= 48
    margin = 0.02 if small else 0.045     # Windows icons fill more of the canvas than macOS ones
    heavy = 1.18 if px <= 32 else (1.08 if small else 1.0)
    s = px * SS

    base = gradient(px)
    # Gentle top sheen, the one bit of "depth" (skipped on tiny sizes where it only adds mud).
    if px >= 64:
        sheen = np.zeros((s, s, 4), np.uint8)
        a = np.clip(1 - np.linspace(0, 1, s) / 0.55, 0, 1) ** 2 * 34
        sheen[..., :3] = 255
        sheen[..., 3] = a[:, None].astype(np.uint8)
        base = Image.alpha_composite(base, Image.fromarray(sheen, "RGBA"))

    g = glyph(px, heavy)
    if px >= 64:  # soft drop shadow under the glyph
        shadow = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        shadow.putalpha(g.getchannel("A").point(lambda v: int(v * 0.30)))
        shadow = shadow.filter(ImageFilter.GaussianBlur(0.012 * s))
        shadow = Image.alpha_composite(Image.new("RGBA", (s, s), (0, 0, 0, 0)),
                                       Image.eval(shadow, lambda v: v))
        offset = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        offset.paste(shadow, (0, round(0.014 * s)))
        base = Image.alpha_composite(base, offset)
    base = Image.alpha_composite(base, g)

    mask = squircle_mask(px, margin)
    out = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    out.paste(base, (0, 0), mask)
    return out.resize((px, px), Image.LANCZOS)


def main():
    ASSETS.mkdir(exist_ok=True)
    render(1024).save(ASSETS / "okra-logo.png")
    render(256).save(ASSETS / "okra-brown-search.png")       # window icon
    render(128).save(ASSETS / "okra-mark.png")               # in-app header mark
    render(64).save(ASSETS / "okra-search.png")              # logo in the search bar
    render(32).save(ASSETS / "okra-tray.png")                # notification-area icon
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [render(s) for s in sizes]
    frames[-1].save(ASSETS / "okra-brown-search.ico", format="ICO",
                    sizes=[(s, s) for s in sizes], append_images=frames[:-1])
    print("wrote", ", ".join(sorted(p.name for p in ASSETS.iterdir())))


if __name__ == "__main__":
    main()
