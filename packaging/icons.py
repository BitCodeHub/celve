#!/usr/bin/env python3
"""Regenerate every app icon from assets/app-icon/celve-master.png (1024 px, transparent corners).

Needs Pillow; on macOS, iconutil also writes the .icns. The outputs are committed, so builds and
packaging never need these tools.

    python3 packaging/icons.py
"""
import base64, io, os, shutil, subprocess, sys, tempfile
from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, "assets", "app-icon")
ID = "cloud.agentlumi.celve"

src = Image.open(os.path.join(DIR, "celve-master.png")).convert("RGBA")
# Full-bleed tile: crop to the artwork's bounds, square it, scale to 1024.
box = src.getchannel("A").point(lambda a: 255 if a > 8 else 0).getbbox() or (0, 0, src.width, src.height)
side = max(box[2] - box[0], box[3] - box[1])
cx, cy = (box[0] + box[2]) / 2, (box[1] + box[3]) / 2
tile = src.crop((round(cx - side / 2), round(cy - side / 2), round(cx + side / 2), round(cy + side / 2))).resize((1024, 1024), Image.LANCZOS)

def full(size):
    return tile.resize((size, size), Image.LANCZOS)

def mac(size):
    # Apple's grid: an 824/1024 body centred on a transparent canvas.
    body = round(size * 824 / 1024)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    off = (size - body) // 2
    canvas.paste(tile.resize((body, body), Image.LANCZOS), (off, off))
    return canvas

def save(img, *parts):
    path = os.path.join(DIR, *parts)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    img.save(path, optimize=True)

def svg_wrapper(px):
    buf = io.BytesIO()
    full(px).save(buf, "PNG", optimize=True)
    data = base64.b64encode(buf.getvalue()).decode()
    return (f'<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 512 512" width="512" height="512">'
            f'<image width="512" height="512" xlink:href="data:image/png;base64,{data}"/></svg>\n')

save(full(1024), "celve-1024.png")
save(mac(512), "celve-macos-512.png")
for s in (16, 24, 32, 48, 64, 128, 256, 512):
    save(full(s), "hicolor", f"{s}x{s}", "apps", f"{ID}.png")
svg = svg_wrapper(512)
open(os.path.join(DIR, "celve.svg"), "w").write(svg)
os.makedirs(os.path.join(DIR, "hicolor", "scalable", "apps"), exist_ok=True)
open(os.path.join(DIR, "hicolor", "scalable", "apps", f"{ID}.svg"), "w").write(svg)
full(256).save(os.path.join(DIR, "celve.ico"), sizes=[(s, s) for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)])
if shutil.which("iconutil"):
    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "celve.iconset")
        os.makedirs(iconset)
        for s in (16, 32, 128, 256, 512):
            mac(s).save(os.path.join(iconset, f"icon_{s}x{s}.png"))
            mac(s * 2).save(os.path.join(iconset, f"icon_{s}x{s}@2x.png"))
        subprocess.run(["iconutil", "-c", "icns", "-o", os.path.join(DIR, "celve.icns"), iconset], check=True)
else:
    print("warning: iconutil not found (macOS only); celve.icns not regenerated", file=sys.stderr)
# The web app's favicon and touch icon.
web = os.path.join(ROOT, "apps", "celve-web", "assets")
os.makedirs(web, exist_ok=True)
full(192).save(os.path.join(web, "icon-192.png"), optimize=True)
full(32).save(os.path.join(web, "favicon.png"), optimize=True)
print("icons written to", DIR)
