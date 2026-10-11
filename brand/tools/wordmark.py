"""Build brand/fortis-wordmark.svg: the lockup mark + "fortis" in JetBrains Mono
Medium, converted to outlines so the SVG needs no font.

usage: wordmark.py FONT.ttf OUT.svg
"""
import sys

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont

font_path, out = sys.argv[1], sys.argv[2]
font = TTFont(font_path)
glyphs = font.getGlyphSet()
cmap = font.getBestCmap()
upm = font["head"].unitsPerEm
ascent = font["hhea"].ascent

SIZE = 40.0  # px font size in the 64-high artboard
scale = SIZE / upm
baseline = 44.0  # matches the mark's optical centre (keyhole ~y27–42)
x = 76.0  # after the 64-wide mark + 12 gap

pen = SVGPathPen(glyphs)
for ch in "fortis":
    name = cmap[ord(ch)]
    tp = TransformPen(pen, (scale, 0, 0, -scale, x, baseline))
    glyphs[name].draw(tp)
    x += glyphs[name].width * scale
width = round(x + 4)

shield = "M32 7 53 14.5V30c0 13.5-8.8 21.6-21 26.5C19.8 51.6 11 43.5 11 30V14.5Z"
svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} 64" width="{width}" height="64">
  <title>fortis</title>
  <path d="{shield}" fill="none" stroke="#E9ECEF" stroke-width="3.5" stroke-linejoin="round"/>
  <g fill="#00FF41"><circle cx="32" cy="27" r="5.5"/><path d="M29 31h6l1.8 11h-9.6Z"/></g>
  <path fill="#E9ECEF" d="{pen.getCommands()}"/>
</svg>
'''
open(out, "w").write(svg)
print(out, width)
