"""Compose the supplied logo and exact tagline on a 16:9 slide.

Uses ../requirements.txt, macOS Hiragino Sans W4, IBM Plex Sans JP, and rsvg-convert.
The delivered SVG includes the original PNG and outlined tagline glyphs.
"""

import base64
from html import escape
from pathlib import Path
import subprocess
import unicodedata

from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
import uharfbuzz as hb

ROOT = Path(__file__).resolve().parent
TAGLINE = "AIエージェントのための知識管理ツールチェイン"
FONT = Path("/System/Library/Fonts") / unicodedata.normalize("NFD", "ヒラギノ角ゴシック W4.ttc")
PLEX_JP = ROOT.parent / "fonts" / "IBMPlexSansJP-Regular.ttf"
WIDTH, HEIGHT = 1920, 1080
BACKGROUND = "#F7F8F5"
INK = "#303234"


def tagline_outline(font_path, font_size, tracking_px, proportional):
    font = TTFont(font_path, fontNumber=0)
    glyphs = font.getGlyphSet()
    order = font.getGlyphOrder()
    face = hb.Font(hb.Face(font_path.read_bytes(), 0))
    upem = font["head"].unitsPerEm
    face.scale = (upem, upem)
    buffer = hb.Buffer()
    buffer.add_str(TAGLINE)
    buffer.guess_segment_properties()
    hb.shape(face, buffer, {"kern": True, "palt": proportional})
    paths = SVGPathPen(glyphs)
    bounds = BoundsPen(glyphs)
    scale = font_size / upem
    tracking = tracking_px / scale
    cursor = 0
    for info, pos in zip(buffer.glyph_infos, buffer.glyph_positions):
        if info.codepoint == 0:
            raise ValueError("The selected font is missing a tagline glyph.")
        transform = (1, 0, 0, 1, cursor + pos.x_offset, pos.y_offset)
        glyph = glyphs[order[info.codepoint]]
        glyph.draw(TransformPen(paths, transform))
        glyph.draw(TransformPen(bounds, transform))
        cursor += pos.x_advance + tracking
    left, bottom, right, top = bounds.bounds
    x = WIDTH / 2 - (left + right) * scale / 2
    # Place the visible top of the tagline at y=655.
    baseline = 655 + top * scale
    return f'<path fill="{INK}" transform="translate({x:.6f} {baseline:.6f}) scale({scale:.8f} {-scale:.8f})" d="{paths.getCommands()}"/>'


def render(stem, font_path, font_size, tracking_px, proportional=False):
    logo = base64.b64encode((ROOT / "logo-selected.png").read_bytes()).decode("ascii")
    # Supplied PNG is 1723 × 448, including its original transparent margins.
    logo_width = 1380
    logo_height = logo_width * 448 / 1723
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-labelledby="title desc">
<title id="title">Monika</title>
<desc id="desc">{escape(TAGLINE)}</desc>
<rect width="{WIDTH}" height="{HEIGHT}" fill="{BACKGROUND}"/>
<image x="270" y="300" width="{logo_width}" height="{logo_height:.8f}" xlink:href="data:image/png;base64,{logo}"/>
{tagline_outline(font_path, font_size, tracking_px, proportional)}
</svg>
'''
    destination = ROOT / f"{stem}.svg"
    destination.write_text(svg, encoding="utf-8")
    subprocess.run(["rsvg-convert", "--output", str(destination.with_suffix(".png")), str(destination)], check=True)
    print(f"Created {stem}.png and {stem}.svg (1920 × 1080).")


def main():
    render("monika-eyecatch", FONT, 42, 1)
    render("monika-eyecatch-v2", PLEX_JP, 38, .3, proportional=True)


if __name__ == "__main__":
    main()
