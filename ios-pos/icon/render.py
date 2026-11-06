#!/usr/bin/env python3
"""Render icon.svg to the POS's iOS and Android launcher assets.

Run from ios-pos:  python3 icon/render.py
Needs rsvg-convert (brew install librsvg) and Pillow.

iOS gets the full composition, opaque and full-bleed, as one 1024 px image.
Android (android-relay) gets an adaptive icon: the navy gradient as the background layer and
the chip with its badge, recentred and scaled so the badge sits inside the 66 dp safe zone, as
the foreground layer. minSdk is 26, so no legacy mipmaps.
"""
import os, re, subprocess, sys, tempfile
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
SRC = open(os.path.join(HERE, "icon.svg")).read()
IOS = os.path.join(REPO, "ios-pos/FedimintCardPOS/Assets.xcassets/AppIcon.appiconset")
RES = os.path.join(REPO, "android-relay/app/src/main/res")

DENSITIES = {"mdpi": 1, "hdpi": 1.5, "xhdpi": 2, "xxhdpi": 3, "xxxhdpi": 4}
FG_SCALE = 0.66        # the badge's far edge lands at ~31 dp from the centre
FG_CENTRE = (221, 235)  # centre of the chip-plus-badge bounding box


def render(svg: str, px: int) -> Image.Image:
    with tempfile.NamedTemporaryFile("w", suffix=".svg", delete=False) as f:
        f.write(svg)
    try:
        out = subprocess.run(["rsvg-convert", "-w", str(px), "-h", str(px), f.name],
                             check=True, capture_output=True).stdout
    finally:
        os.unlink(f.name)
    with tempfile.NamedTemporaryFile("wb", suffix=".png", delete=False) as g:
        g.write(out)
    try:
        return Image.open(g.name).convert("RGBA").copy()
    finally:
        os.unlink(g.name)


def foreground(svg: str) -> str:
    body = svg.replace('<rect width="460" height="460" fill="url(#navy)"/>', "")
    x, y = FG_CENTRE
    t = f'<g transform="translate(230 230) scale({FG_SCALE}) translate(-{x} -{y})">'
    body = body.replace('  <g id="chip"', t + '\n  <g id="chip"', 1)
    return body.replace("</svg>", "</g>\n</svg>")


def background(svg: str) -> str:
    return re.sub(r'  <g id="chip".*</svg>', "</svg>", svg, flags=re.S)


def main():
    render(SRC, 1024).convert("RGB").save(os.path.join(IOS, "Icon-1024.png"))
    print("ios: Icon-1024")
    for d, k in DENSITIES.items():
        px = int(108 * k)
        out = os.path.join(RES, f"drawable-{d}")
        os.makedirs(out, exist_ok=True)
        render(foreground(SRC), px).save(os.path.join(out, "ic_launcher_foreground.png"))
        render(background(SRC), px).convert("RGB").save(os.path.join(out, "ic_launcher_background.png"))
    print("android:", len(DENSITIES), "densities")


if __name__ == "__main__":
    sys.exit(main())
