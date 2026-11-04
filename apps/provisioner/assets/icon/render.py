#!/usr/bin/env python3
"""Render icon.svg to every iOS and Android launcher asset.

Run from apps/provisioner:  python3 assets/icon/render.py
Needs rsvg-convert (brew install librsvg) and Pillow.

iOS gets the full composition, opaque and full-bleed (iOS masks the corners itself).
Android gets an adaptive icon: the navy gradient as the background layer and the gold
chip, scaled to ~56 dp so it sits inside the 66 dp safe zone, as the foreground layer.
Legacy mipmaps are the composition with rounded corners, plus a circular crop.
"""
import os, re, subprocess, sys, tempfile
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
APP = os.path.dirname(os.path.dirname(HERE))
SRC = open(os.path.join(HERE, "icon.svg")).read()
IOS = os.path.join(APP, "ios/FedimintProvisioner/Images.xcassets/AppIcon.appiconset")
RES = os.path.join(APP, "android/app/src/main/res")

IOS_SIZES = {"Icon-1024": 1024, "Icon-20@2x": 40, "Icon-20@3x": 60, "Icon-29@2x": 58,
             "Icon-29@3x": 87, "Icon-40@2x": 80, "Icon-40@3x": 120, "Icon-60@2x": 120,
             "Icon-60@3x": 180, "Icon-76@2x": 152, "Icon-83.5@2x": 167}
DENSITIES = {"mdpi": 1, "hdpi": 1.5, "xhdpi": 2, "xxhdpi": 3, "xxxhdpi": 4}
FG_SCALE = 0.887  # 269 px panel * 0.887 / 460 px canvas * 108 dp = 56 dp wide


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


def without_background(svg: str) -> str:
    return svg.replace('<rect width="460" height="460" fill="url(#navy)"/>', "")


def only_background(svg: str) -> str:
    return re.sub(r'<g id="chip">.*?</g>\s*</svg>', "</svg>", svg, flags=re.S)


def scaled_foreground(svg: str) -> str:
    t = f'transform="translate(230 230) scale({FG_SCALE}) translate(-230 -230)"'
    return without_background(svg).replace('<g id="chip">', f'<g id="chip" {t}>')


def rounded(im: Image.Image, radius_frac: float, circle=False) -> Image.Image:
    mask = Image.new("L", im.size, 0)
    d = ImageDraw.Draw(mask)
    w, h = im.size
    if circle:
        d.ellipse((0, 0, w - 1, h - 1), fill=255)
    else:
        d.rounded_rectangle((0, 0, w - 1, h - 1), radius=int(w * radius_frac), fill=255)
    out = im.copy()
    out.putalpha(mask)
    return out


def main():
    full = render(SRC, 1024)
    for name, px in IOS_SIZES.items():
        full.resize((px, px), Image.LANCZOS).convert("RGB").save(os.path.join(IOS, name + ".png"))
    print("ios:", len(IOS_SIZES), "sizes")

    for d, k in DENSITIES.items():
        px = int(108 * k)
        render(scaled_foreground(SRC), px).save(os.path.join(RES, f"drawable-{d}", "ic_launcher_foreground.png"))
        render(only_background(SRC), px).convert("RGB").save(os.path.join(RES, f"drawable-{d}", "ic_launcher_background.png"))
        legacy = full.resize((int(48 * k),) * 2, Image.LANCZOS)
        rounded(legacy, 0.1).save(os.path.join(RES, f"mipmap-{d}", "ic_launcher.png"))
        rounded(legacy, 0, circle=True).save(os.path.join(RES, f"mipmap-{d}", "ic_launcher_round.png"))
    print("android:", len(DENSITIES), "densities")


if __name__ == "__main__":
    sys.exit(main())
