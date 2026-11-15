#!/usr/bin/env python3
"""Regenerates every logo asset from one geometry definition.

    python tools/make_icons.py            (needs: pip install pillow numpy)

Design: a bold, flat magnifying glass on a transparent background. The lens is a solid disc with a
soft diagonal okra-green gradient and a faint glossy highlight; the handle is two-tone brown (a
light collar and a darker grip) with a rounded end. No ring, no text, no container shape.
Small sizes are rendered natively with a smaller lens and a longer, thicker handle so they still
read as a magnifier in the tray.
Change the colours below and re-run to re-tint everything.
"""
import math
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

ASSETS = Path(__file__).resolve().parent.parent / "assets"

LENS_LIGHT = (190, 230, 98)    # lens gradient, top-left
LENS_DARK = (88, 154, 26)      # lens gradient, bottom-right
COLLAR = (201, 146, 94)        # handle, part next to the lens
GRIP = (110, 66, 36)           # handle, outer part

SS = 4  # supersampling factor


def lens_layer(s: int, cx: float, cy: float, r: float) -> Image.Image:
    """Solid disc filled with a diagonal gradient, plus a soft highlight near the top-left."""
    yy, xx = np.mgrid[0:s, 0:s].astype(np.float32)
    # Project every pixel onto the top-left -> bottom-right diagonal of the disc, 0..1.
    t = ((xx - (cx - r)) + (yy - (cy - r))) / (4 * r)
    t = np.clip(t, 0, 1)[..., None]
    light, dark = np.array(LENS_LIGHT, np.float32), np.array(LENS_DARK, np.float32)
    rgb = light * (1 - t) + dark * t
    img = Image.fromarray(rgb.astype(np.uint8), "RGB").convert("RGBA")

    # Glossy highlight: a soft white ellipse up and to the left of centre.
    gloss = Image.new("L", (s, s), 0)
    ImageDraw.Draw(gloss).ellipse(
        [cx - 0.78 * r, cy - 0.80 * r, cx - 0.05 * r, cy - 0.30 * r], fill=95
    )
    gloss = gloss.filter(ImageFilter.GaussianBlur(0.10 * r))
    white = Image.new("RGBA", (s, s), (255, 255, 255, 0))
    white.putalpha(gloss)
    img = Image.alpha_composite(img, white)

    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    out = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    out.paste(img, (0, 0), mask)
    return out


def handle_layer(s: int, cx: float, cy: float, r: float, width: float, reach: float = 1.92) -> Image.Image:
    """Two-tone handle at 45 degrees: collar (starts under the lens) then grip with a round end."""
    layer = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    u = math.sqrt(0.5)

    def at(dist: float):
        return cx + u * dist, cy + u * dist

    start, split, end = at(0.78 * r), at(1.42 * r + 0.0 * r), at(reach * r)
    d.line([start, split], fill=COLLAR + (255,), width=round(width))
    d.line([split, end], fill=GRIP + (255,), width=round(width))
    ex, ey = end
    d.ellipse([ex - width / 2, ey - width / 2, ex + width / 2, ey + width / 2], fill=GRIP + (255,))
    return layer


def render(px: int) -> Image.Image:
    s = px * SS
    if px <= 32:
        # Tray sizes: a smaller lens with a longer, thicker handle, so it still reads as a magnifier.
        centre, radius, thickness, reach = 0.395, 0.295, 0.155, 2.22
    elif px <= 48:
        centre, radius, thickness, reach = 0.405, 0.310, 0.125, 2.05
    else:
        centre, radius, thickness, reach = 0.415, 0.325, 0.105, 1.92
    cx = cy = centre * s          # lens centre; glyph is optically centred on the canvas
    r = radius * s                # lens radius
    width = thickness * s         # handle thickness

    glyph = Image.alpha_composite(handle_layer(s, cx, cy, r, width, reach), lens_layer(s, cx, cy, r))

    canvas = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    if px >= 64:  # soft drop shadow so the glyph also sits well on light backgrounds
        shadow = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        shadow.putalpha(glyph.getchannel("A").point(lambda v: int(v * 0.28)))
        shadow = shadow.filter(ImageFilter.GaussianBlur(0.012 * s))
        shifted = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        shifted.paste(shadow, (0, round(0.016 * s)))
        canvas = Image.alpha_composite(canvas, shifted)
    canvas = Image.alpha_composite(canvas, glyph)
    return canvas.resize((px, px), Image.LANCZOS)


def main():
    ASSETS.mkdir(exist_ok=True)
    render(1024).save(ASSETS / "okra-logo.png")
    render(256).save(ASSETS / "okra-brown-search.png")       # window icon, README banner
    render(128).save(ASSETS / "okra-mark.png")
    render(32).save(ASSETS / "okra-tray.png")                # notification-area icon
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [render(s) for s in sizes]
    frames[-1].save(ASSETS / "okra-brown-search.ico", format="ICO",
                    sizes=[(s, s) for s in sizes], append_images=frames[:-1])
    print("wrote", ", ".join(sorted(p.name for p in ASSETS.iterdir())))


if __name__ == "__main__":
    main()
