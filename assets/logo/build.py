"""Outline the real IBM Plex glyphs and export the logo studies.

Requires requirements.txt and rsvg-convert. No fonts need to be installed.
"""

from dataclasses import dataclass
from html import escape
from pathlib import Path
import json
import subprocess
import xml.etree.ElementTree as ET

from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
import uharfbuzz as hb


ROOT = Path(__file__).resolve().parent
INK = "#303234"
GREEN = "#22F765"
PAPER = "#F7F8F5"
MUTED = "#737A76"
LINE = "#D8DDD6"
NS = "http://www.w3.org/2000/svg"
MEDIUM = "IBMPlexSans-Medium.ttf"
MONO = "IBMPlexMono-Medium.ttf"


@dataclass(frozen=True)
class Study:
    stem: str
    name: str
    english: str
    description: str


STUDIES = (
    Study("a-stitch", "かがり目", "STITCH", "二つの縁を渡る糸。M の形に関係を残す。"),
    Study("b-weave", "組み合う帯", "WEAVE", "独立した二本の帯が、交互に重なり合う。"),
    Study("c-compare", "照合する印", "COMPARE", "向き合う括弧と、対応を示す二本の線。"),
)

BRACKET_STUDIES = (
    Study("c-compare", "原案", "ORIGINAL", "括弧と二本線。最初のバランスをそのまま。"),
    Study("c2-air", "軽やか", "AIR", "細い線と短い端。内側の余白を広く。"),
    Study("c3-chamfer", "面取り", "CHAMFER", "角と線の端を斜めに切り、方向を添える。"),
    Study("c4-soft", "やわらか", "SOFT", "丸い角と線の端で、親しみのある形に。"),
    Study("c5-wide", "横にひらく", "WIDE", "括弧を左右に広げ、二本線を長くする。"),
    Study("c6-solid", "太い印", "SOLID", "厚い括弧で、小さくても輪郭を強く。"),
)


class Typeface:
    def __init__(self, filename):
        path = ROOT / "fonts" / filename
        self.tt = TTFont(path)
        self.glyphs = self.tt.getGlyphSet()
        self.order = self.tt.getGlyphOrder()
        self.hb = hb.Font(hb.Face(path.read_bytes()))
        self.hb.scale = (self.tt["head"].unitsPerEm,) * 2

    def outline(self, value, x, y, height, color=INK, tracking=0):
        """Place the actual ink bounds at x/y, with a specified ink height."""
        buf = hb.Buffer()
        buf.add_str(value)
        buf.guess_segment_properties()
        hb.shape(self.hb, buf, {"kern": True})
        pen = SVGPathPen(self.glyphs, ntos=lambda n: format(n, ".4f").rstrip("0").rstrip(".") or "0")
        bounds = BoundsPen(self.glyphs)
        cursor = 0
        for info, pos in zip(buf.glyph_infos, buf.glyph_positions):
            transform = (1, 0, 0, 1, cursor + pos.x_offset, pos.y_offset)
            glyph = self.glyphs[self.order[info.codepoint]]
            glyph.draw(TransformPen(pen, transform))
            glyph.draw(TransformPen(bounds, transform))
            cursor += pos.x_advance + tracking
        left, bottom, right, top = bounds.bounds
        scale = height / (top - bottom)
        width = (right - left) * scale
        svg = (
            f'<path fill="{color}" transform="translate({x-left*scale:.5f} '
            f'{y+top*scale:.5f}) scale({scale:.7f} {-scale:.7f})" '
            f'd="{pen.getCommands()}"/>'
        )
        return svg, width


FONTS = {name: Typeface(name) for name in (MEDIUM, "IBMPlexSans-SemiBold.ttf", "IBMPlexMono-Medium.ttf")}


def svg_doc(width, height, content, title, desc=""):
    return (
        f'<svg xmlns="{NS}" width="{width:g}" height="{height:g}" '
        f'viewBox="0 0 {width:g} {height:g}" role="img" aria-labelledby="title desc">\n'
        f'<title id="title">{escape(title)}</title>\n'
        f'<desc id="desc">{escape(desc)}</desc>\n{content}\n</svg>\n'
    )


def mark(study, accented=True, ink=INK):
    tree = ET.parse(ROOT / "source" / f"{study.stem}.svg").getroot()
    for element in tree.iter():
        element.tag = element.tag.removeprefix("{" + NS + "}")
        for attribute in ("stroke", "fill"):
            if element.get(attribute) == INK:
                element.set(attribute, ink)
            elif element.get(attribute) == GREEN:
                element.set(attribute, GREEN if accented else ink)
    return "\n".join(ET.tostring(e, encoding="unicode") for e in tree)


def logo(study, accented=True, font=MEDIUM, ink=INK):
    lettering, width = FONTS[font].outline("Monika", 112, 24, 64, ink)
    return f'<g transform="translate(8 8)">{mark(study, accented, ink)}</g>{lettering}', width + 136, 112


def placed(content, x, y, scale=1):
    return f'<g transform="translate({x:g} {y:g}) scale({scale:g})">{content}</g>'


def latin(value, x, y, height=12, color=INK, font=MEDIUM, tracking=0):
    return FONTS[font].outline(value, x, y, height, color, tracking)[0]


def japanese(value, x, y, size=16, color=INK):
    return f'<text x="{x}" y="{y}" font-family="Hiragino Sans, sans-serif" font-size="{size}" fill="{color}">{escape(value)}</text>'


def render(path, scale=1):
    subprocess.run(
        ["rsvg-convert", "--zoom", str(scale), "--output", str(path.with_suffix(".png")), str(path)],
        check=True,
    )


def save(directory, filename, width, height, content, title, desc="", scale=1):
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / filename
    path.write_text(svg_doc(width, height, content, title, desc), encoding="utf-8")
    render(path, scale)


def comparison():
    w, h = 1520, 1080
    parts = [f'<rect width="{w}" height="{h}" fill="{PAPER}"/>']
    parts += [latin("Monika", 64, 51, 42), latin("LOGO STUDIES / 01", 1112, 69, 12, MUTED, tracking=70)]
    parts += [japanese("IBM Plex と、Link を表す三つの案。", 66, 126, 16, MUTED)]
    parts += [f'<path d="M64 158H1456M64 989H1456" stroke="{LINE}"/>']
    for i, study in enumerate(STUDIES):
        x = 64 + 480*i
        letter = study.stem[0].upper()
        parts += [latin(letter, x, 190, 15), latin(study.english, x+30, 192, 11, MUTED, tracking=60)]
        parts += [japanese(study.name, x, 242, 25)]
        parts += [placed(mark(study), x+116, 275, 2)]
        content, lw, lh = logo(study)
        parts += [placed(content, x+(432-lw*.96)/2, 493, .96)]
        parts += [japanese(study.description, x, 637, 14, MUTED)]
        parts += [f'<path d="M{x} 671H{x+432}" stroke="{LINE}"/>']
        parts += [latin("ONE COLOR", x, 698, 10, MUTED, tracking=60)]
        content, lw, lh = logo(study, False)
        parts += [placed(content, x+4, 733, .72)]
        parts += [latin("SMALL SIZES", x, 868, 10, MUTED, tracking=60)]
        for j, size in enumerate((16, 24, 32, 48)):
            left = x + j*96
            parts += [placed(mark(study, False), left, 938-size, size/96)]
            parts += [latin(str(size), left+size+9, 926, 9, MUTED)]
    parts += [f'<rect x="64" y="1020" width="14" height="14" rx="3" fill="{INK}"/>']
    parts += [latin("#303234", 88, 1022, 10, MUTED)]
    parts += [f'<rect x="228" y="1020" width="14" height="14" rx="3" fill="{GREEN}"/>']
    parts += [latin("OKLCH 85.4% / 0.2441 / 147.4", 252, 1022, 10, MUTED)]
    parts += [japanese("台紙のみ背景色あり。ロゴ素材は透明背景。", 1110, 1032, 12, MUTED)]
    save(ROOT, "comparison.svg", w, h, "".join(parts), "Monika ロゴの三案", "左から A かがり目、B 組み合う帯、C 照合する印。上はアクセント入り、下は単色。")


def typography():
    parts = [f'<rect width="1280" height="740" fill="{PAPER}"/>']
    parts += [latin("Monika / IBM Plex", 64, 50, 31), japanese("同じ文字の高さで比較", 64, 118, 16, MUTED)]
    entries = ((MEDIUM, "Sans Medium"), ("IBMPlexSans-SemiBold.ttf", "Sans SemiBold"), ("IBMPlexMono-Medium.ttf", "Mono Medium"))
    for i, (font, label) in enumerate(entries):
        y = 179+i*170
        parts += [f'<path d="M64 {y-24}H1216" stroke="{LINE}"/>']
        parts += [latin(label, 64, y+28, 17, MUTED)]
        parts += [FONTS[font].outline("Monika", 398, y+4, 74)[0]]
        parts += [FONTS[font].outline("Monika", 1002, y+45, 20)[0]]
    parts += [japanese("三案のロゴは Sans Medium。ウェイトと Sans / Mono は、意匠と合わせて検討。", 64, 706, 14, MUTED)]
    save(ROOT, "typography.svg", 1280, 740, "".join(parts), "IBM Plex のロゴタイプ比較")


def bracket_comparison():
    w, h = 1520, 1300
    parts = [f'<rect width="{w}" height="{h}" fill="{PAPER}"/>']
    parts += [latin("Monika", 64, 51, 42, font=MONO), latin("BRACKET STUDIES / 02", 1080, 69, 12, MUTED, tracking=60)]
    parts += [japanese("[=] から、六つの表情。原案と五つのバリエーション。", 66, 127, 16, MUTED)]
    parts += [f'<path d="M64 158H1456M64 1230H1456" stroke="{LINE}"/>']
    for i, study in enumerate(BRACKET_STUDIES):
        x, y = 64+(i%3)*480, 194+(i//3)*530
        parts += [latin(f"C{i+1}", x, y, 15), latin(study.english, x+40, y+2, 11, MUTED, tracking=60)]
        parts += [japanese(study.name, x, y+52, 24)]
        parts += [placed(mark(study), x+127, y+79, 1.8)]
        content, lw, lh = logo(study, font=MONO)
        parts += [placed(content, x+(432-lw*.94)/2, y+263, .94)]
        parts += [japanese(study.description, x, y+403, 14, MUTED)]
        for j, size in enumerate((16, 24, 32)):
            parts += [placed(mark(study, False), x+j*70, y+469-size, size/96)]
            parts += [latin(str(size), x+j*70+size+6, y+457, 9, MUTED)]
        content, lw, lh = logo(study, False, font=MONO)
        parts += [placed(content, x+230, y+424, .46)]
        if i < 3:
            parts += [f'<path d="M{x} {y+503}H{x+432}" stroke="{LINE}"/>']
    parts += [latin("IBM PLEX MONO MEDIUM", 64, 1260, 11, MUTED, tracking=40)]
    parts += [japanese("#303234 + #22F765 ｜ ロゴ素材は透明背景", 1100, 1270, 12, MUTED)]
    save(ROOT, "brackets.svg", w, h, "".join(parts), "Monika [=] の六案", "C1 原案、C2 軽やか、C3 面取り、C4 やわらか、C5 横にひらく、C6 太い印。")


def preview_data():
    studies = []
    for i, study in enumerate(BRACKET_STUDIES):
        studies.append({
            "id": study.stem, "label": f"C{i+1}", "name": study.name,
            "english": study.english, "description": study.description, "mark": mark(study),
        })
    fonts = []
    for key, filename, label in (("medium", MEDIUM, "Sans Medium"),
                                ("semibold", "IBMPlexSans-SemiBold.ttf", "Sans SemiBold"),
                                ("mono", "IBMPlexMono-Medium.ttf", "Mono Medium")):
        lettering, width = FONTS[filename].outline("Monika", 112, 24, 64)
        fonts.append({"id": key, "label": label, "lettering": lettering, "width": width+136})
    data = {"studies": studies, "fonts": fonts}
    (ROOT / "preview-data.js").write_text("// Generated by build.py; edit source SVGs or build.py.\nwindow.MONIKA_DATA = "+json.dumps(data, ensure_ascii=False)+";\n", encoding="utf-8")


def main():
    for study in STUDIES:
        for accented, variant in ((True, "accent"), (False, "mono")):
            content, w, h = logo(study, accented)
            save(ROOT / "exports", f"{study.stem}-{variant}.svg", w, h, content,
                 f"Monika — {study.name}", "IBM Plex Sans Medium。背景は透明。", scale=4)
            save(ROOT / "exports", f"{study.stem}-mark-{variant}.svg", 96, 96,
                 mark(study, accented), f"Monika — {study.name}のマーク", "背景は透明。", scale=4)
    for i, study in enumerate(BRACKET_STUDIES, 1):
        for accented, variant in ((True, "accent"), (False, "mono")):
            content, w, h = logo(study, accented, font=MONO)
            save(ROOT / "exports" / "brackets", f"c{i}-{variant}.svg", w, h, content,
                 f"Monika C{i} — {study.name}", "IBM Plex Mono Medium。背景は透明。", scale=4)
            save(ROOT / "exports" / "brackets", f"c{i}-mark-{variant}.svg", 96, 96,
                 mark(study, accented), f"Monika C{i} — {study.name}のマーク", "背景は透明。", scale=4)
    comparison()
    typography()
    bracket_comparison()
    preview_data()
    colors = {
        "ink": INK,
        "accent": {"requested": "oklch(85.4% 0.2441 147.4)", "srgb": GREEN,
                   "srgb_float": [0.1346251961, 0.9698161228, 0.3974225043],
                   "note": "指定色は sRGB 色域内。各チャンネルを 8 bit に丸める。"},
    }
    (ROOT / "colors.json").write_text(json.dumps(colors, ensure_ascii=False, indent=2)+"\n", encoding="utf-8")
    print("Exported original studies, six bracket studies, transparent SVG/PNG assets and preview data.")


if __name__ == "__main__":
    main()
