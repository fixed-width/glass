#!/usr/bin/env python3
"""Regenerate AppIcon.icns from the Fixed Width app-icon mark (AppIcon.svg).

This box has no iconutil / SVG rasterizer, and AppIcon.svg is solid shapes only,
so we draw the mark directly with Pillow and assemble the .icns container by hand.
Run: python3 make-appicon.py  (needs Pillow). Output: AppIcon.icns beside this script.
"""
import io, os, struct
from PIL import Image, ImageDraw

NAVY, WHITE, AMBER = (21, 32, 43, 255), (250, 250, 250, 255), (255, 159, 28, 255)
# AppIcon.svg mark paths, 64-unit space, before the group transform translate(-32,-32) scale(9).
LEFT  = [(16,14),(26,14),(26,19),(21,19),(21,45),(26,45),(26,50),(16,50)]
RIGHT = [(48,14),(38,14),(38,19),(43,19),(43,45),(38,45),(38,50),(48,50)]
PLUS  = [(30,25),(34,25),(34,30),(39,30),(39,34),(34,34),(34,39),(30,39),(30,34),(25,34),(25,30),(30,30)]

def render(size, ss=4):
    n = size * ss
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    s = n / 512.0  # 512 viewBox -> n px
    d.rounded_rectangle([0, 0, n - 1, n - 1], radius=112 * s, fill=NAVY)
    tf = lambda pts: [((9 * x - 32) * s, (9 * y - 32) * s) for (x, y) in pts]
    d.polygon(tf(LEFT), fill=WHITE)
    d.polygon(tf(RIGHT), fill=WHITE)
    d.polygon(tf(PLUS), fill=AMBER)
    return img.resize((size, size), Image.LANCZOS)

r = {sz: render(sz) for sz in {16, 32, 64, 128, 256, 512, 1024}}
types = [(b'icp4',16),(b'icp5',32),(b'ic11',32),(b'ic12',64),(b'ic07',128),
         (b'ic08',256),(b'ic13',256),(b'ic09',512),(b'ic14',512),(b'ic10',1024)]
chunks = b''
for code, sz in types:
    b = io.BytesIO(); r[sz].save(b, format='PNG'); png = b.getvalue()
    chunks += code + struct.pack('>I', len(png) + 8) + png
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'AppIcon.icns')
open(out, 'wb').write(b'icns' + struct.pack('>I', len(chunks) + 8) + chunks)
print("wrote", out)
