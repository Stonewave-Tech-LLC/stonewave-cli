#!/usr/bin/env python3
"""Render an asciicast v2 recording of `stonewave` to a GIF and PNG screenshots.

It replays the recording in a tiny terminal emulator that understands only the
escapes `stonewave` writes (cursor up, column, clear line/below, SGR colours)
and draws each frame with Pillow, in a window like the Stonewave mock-up.
Block characters are drawn as rectangles, so the banner pixels line up exactly.

Usage:
  python cast2gif.py in.cast out.gif [--png-at SECONDS out.png]... [--final-png out.png]
"""

import argparse
import bisect
import functools
import json
import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

try:
    from fontTools.ttLib import TTFont
except ImportError:  # optional: without it the first font is used for everything
    TTFont = None

WIN_BG = (11, 18, 32)
BAR_BG = (17, 26, 43)
PAGE_BG = (246, 248, 250)
FG = (226, 232, 240)
DOTS = (59, 69, 86)
TITLE = (125, 136, 150)

FONT_DIRS = [Path("C:/Windows/Fonts"), Path("/usr/share/fonts/truetype"), Path("/Library/Fonts")]
FONT_FILES = ["CascadiaMono.ttf", "consola.ttf", "seguisym.ttf", "DejaVuSansMono.ttf"]
BOLD_FILES = ["CascadiaMono.ttf", "consolab.ttf"]

TOKEN = re.compile(r"\x1b\[([0-9;?]*)([@-~])|\r|\n|.", re.S)
BASIC = [(0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16),
         (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229)]


def cube(n):
    """xterm 256-colour palette entry."""
    if n < 16:
        return BASIC[n % 8]
    if n < 232:
        n -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return (lv[n // 36], lv[(n // 6) % 6], lv[n % 6])
    g = 8 + (n - 232) * 10
    return (g, g, g)


class Screen:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.grid = [[None] * cols for _ in range(rows)]
        self.row = self.col = 0
        self.fg = self.bg = None
        self.bold = self.underline = False
        self.max_row = 0

    def _put(self, ch):
        if self.col < self.cols:
            self.grid[self.row][self.col] = (ch, self.fg, self.bg, self.bold, self.underline)
            self.max_row = max(self.max_row, self.row)
        self.col += 1

    def _newline(self):
        self.row += 1
        if self.row >= self.rows:
            self.grid.pop(0)
            self.grid.append([None] * self.cols)
            self.row = self.rows - 1

    def _clear_line_from(self, col):
        for c in range(col, self.cols):
            self.grid[self.row][c] = None

    def _sgr(self, params):
        ps = [int(p) if p else 0 for p in params.split(";")] if params else [0]
        i = 0
        while i < len(ps):
            p = ps[i]
            if p == 0:
                self.fg = self.bg = None
                self.bold = self.underline = False
            elif p == 1:
                self.bold = True
            elif p == 22:
                self.bold = False
            elif p == 4:
                self.underline = True
            elif p == 24:
                self.underline = False
            elif p == 39:
                self.fg = None
            elif p == 49:
                self.bg = None
            elif p in (38, 48) and i + 1 < len(ps):
                if ps[i + 1] == 2 and i + 4 < len(ps):
                    col = tuple(ps[i + 2:i + 5])
                    i += 4
                elif ps[i + 1] == 5 and i + 2 < len(ps):
                    col = cube(ps[i + 2])
                    i += 2
                else:
                    col = None
                if p == 38:
                    self.fg = col
                else:
                    self.bg = col
            elif 30 <= p <= 37:
                self.fg = BASIC[p - 30]
            elif 90 <= p <= 97:
                self.fg = tuple(min(255, v + 50) for v in BASIC[p - 90])
            i += 1

    def feed(self, data):
        for m in TOKEN.finditer(data):
            tok = m.group(0)
            if m.group(2):
                params, final = m.group(1), m.group(2)
                n = int(params) if params.isdigit() else 1
                if final == "A":
                    self.row = max(0, self.row - n)
                elif final == "B":
                    self.row = min(self.rows - 1, self.row + n)
                elif final == "G":
                    self.col = max(0, n - 1)
                elif final == "K":
                    if params in ("", "0"):
                        self._clear_line_from(self.col)
                    elif params == "2":
                        self._clear_line_from(0)
                elif final == "J":
                    if params in ("", "0"):
                        self._clear_line_from(self.col)
                        for r in range(self.row + 1, self.rows):
                            self.grid[r] = [None] * self.cols
                elif final == "m":
                    self._sgr(params)
                # cursor show/hide and anything else: ignored
            elif tok == "\r":
                self.col = 0
            elif tok == "\n":
                self._newline()
            elif tok == "\x1b":
                continue
            else:
                self._put(tok)

    def snapshot(self):
        return tuple(tuple(r) for r in self.grid)


def find_font(names):
    for name in names:
        for d in FONT_DIRS:
            p = d / name
            if p.exists():
                return p
    return None


@functools.lru_cache(maxsize=None)
def load_fonts(scale):
    """Fonts, bold font and character maps for one scale, loaded once."""
    size = 16 * scale
    paths = [p for p in (find_font([n]) for n in FONT_FILES) if p]
    if not paths:
        sys.exit("no monospace font found")
    fonts = [ImageFont.truetype(str(p), size) for p in paths]
    bold_path = find_font(BOLD_FILES)
    bold = ImageFont.truetype(str(bold_path), size) if bold_path else fonts[0]
    if bold_path and bold_path.name == "CascadiaMono.ttf":
        try:
            bold.set_variation_by_name("Bold")
        except Exception:
            pass
    cmaps = [
        set(TTFont(str(p), fontNumber=0).getBestCmap().keys()) if TTFont else None for p in paths
    ]
    return fonts, bold, cmaps


class Renderer:
    def __init__(self, scale, title, rows_used, cols):
        self.s = scale
        self.fonts, self.bold, self.cmaps = load_fonts(scale)
        self.cw = round(self.fonts[0].getlength("M"))
        self.ch = 20 * scale
        self.title = title
        self.cols = cols
        self.rows = rows_used
        self.pad = 28 * scale
        self.bar = 44 * scale
        self.inner = 18 * scale
        self.margin = 24 * scale
        self.w = self.cols * self.cw + 2 * self.inner + 2 * self.margin
        self.h = self.rows * self.ch + self.bar + 2 * self.inner + 2 * self.margin

    def font_for(self, ch, bold):
        if bold and (self.cmaps[0] is None or ord(ch) in self.cmaps[0]):
            return self.bold
        for f, cmap in zip(self.fonts, self.cmaps):
            if cmap is None or ord(ch) in cmap:
                return f
        return self.fonts[0]

    def chrome(self):
        s = self.s
        img = Image.new("RGB", (self.w, self.h), PAGE_BG)
        d = ImageDraw.Draw(img)
        box = (self.margin, self.margin, self.w - self.margin, self.h - self.margin)
        d.rounded_rectangle(box, radius=16 * s, fill=WIN_BG, outline=(30, 41, 59), width=max(1, s))
        d.rounded_rectangle((box[0], box[1], box[2], box[1] + self.bar), radius=16 * s, fill=BAR_BG)
        d.rectangle((box[0], box[1] + self.bar - 16 * s, box[2], box[1] + self.bar), fill=BAR_BG)
        d.line((box[0], box[1] + self.bar, box[2], box[1] + self.bar), fill=(30, 41, 59), width=max(1, s))
        cy = box[1] + self.bar // 2
        for i in range(3):
            cx = box[0] + 22 * s + i * 20 * s
            d.ellipse((cx - 6 * s, cy - 6 * s, cx + 6 * s, cy + 6 * s), fill=DOTS)
        d.text((box[0] + 92 * s, cy), self.title, font=self.fonts[0], fill=TITLE, anchor="lm")
        return img

    def render(self, snap, base):
        img = base.copy()
        d = ImageDraw.Draw(img)
        x0 = self.margin + self.inner
        y0 = self.margin + self.bar + self.inner
        cw, chh = self.cw, self.ch
        for r in range(min(self.rows, len(snap))):
            for c, cell in enumerate(snap[r]):
                if cell is None:
                    continue
                ch, fg, bg, bold, ul = cell
                fg = fg or FG
                x, y = x0 + c * cw, y0 + r * chh
                if bg:
                    d.rectangle((x, y, x + cw - 1, y + chh - 1), fill=bg)
                if self.block(d, ch, x, y, fg):
                    continue
                if ch != " ":
                    d.text((x, y + chh // 2), ch, font=self.font_for(ch, bold), fill=fg, anchor="lm")
                if ul:
                    d.line((x, y + chh - 3 * self.s, x + cw, y + chh - 3 * self.s), fill=fg, width=self.s)
        return img

    def block(self, d, ch, x, y, fg):
        """Draws block elements as exact rectangles. Returns True when handled."""
        cw, chh, s = self.cw, self.ch, self.s
        o = ord(ch)
        if ch == "█":
            d.rectangle((x, y, x + cw - 1, y + chh - 1), fill=fg)
        elif ch == "▀":
            d.rectangle((x, y, x + cw - 1, y + chh // 2 - 1), fill=fg)
        elif ch == "▄":
            d.rectangle((x, y + chh // 2, x + cw - 1, y + chh - 1), fill=fg)
        elif 0x2581 <= o <= 0x2587:  # lower eighths
            h = round(chh * (o - 0x2580) / 8)
            d.rectangle((x, y + chh - h, x + cw - 1, y + chh - 1), fill=fg)
        elif 0x2589 <= o <= 0x258F:  # left eighths: ▉ 7/8 ... ▏ 1/8
            w = max(1, round(cw * (0x2590 - o) / 8))
            d.rectangle((x, y, x + w - 1, y + chh - 1), fill=fg)
        elif ch == "▕":
            w = max(1, round(cw / 8))
            d.rectangle((x + cw - w, y, x + cw - 1, y + chh - 1), fill=fg)
        elif ch == "━":
            t = 3 * s
            d.rectangle((x, y + chh // 2 - t // 2, x + cw - 1, y + chh // 2 + t - t // 2 - 1), fill=fg)
        elif ch == "─":
            d.rectangle((x, y + chh // 2, x + cw - 1, y + chh // 2 + s - 1), fill=fg)
        else:
            return False
        return True


def load(path):
    lines = Path(path).read_text(encoding="utf-8").splitlines()
    header = json.loads(lines[0])
    events = [json.loads(line) for line in lines[1:] if line.strip()]
    return header, [(t, data) for t, kind, data in events if kind == "o"]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cast")
    ap.add_argument("gif", nargs="?")
    ap.add_argument("--png-at", nargs=2, action="append", default=[], metavar=("SECONDS", "PNG"))
    ap.add_argument("--final-png")
    ap.add_argument("--fps", type=float, default=25)
    ap.add_argument("--hold", type=float, default=3.0, help="seconds to hold the last frame")
    ap.add_argument("--title", default="root@srv-prod-01")
    ap.add_argument("--png-scale", type=int, default=2)
    args = ap.parse_args()

    header, events = load(args.cast)
    cols = header["width"]
    screen = Screen(cols, 80)

    # State after each event, then sampled at a fixed frame rate.
    states = []
    for t, data in events:
        screen.feed(data)
        states.append((t, screen.snapshot()))
    rows_used = screen.max_row + 2
    end = states[-1][0] if states else 0.0

    times = [et for et, _ in states]

    def state_at(t):
        i = bisect.bisect_right(times, t) - 1
        return states[max(i, 0)][1]

    def screenshot(snap, png):
        # Crop each screenshot to its own content, plus one empty row.
        used = max((i for i, row in enumerate(snap) if any(row)), default=0) + 2
        r = Renderer(args.png_scale, args.title, used, cols)
        r.render(snap, r.chrome()).save(png)
        print("wrote", png)

    for t, png in args.png_at:
        screenshot(state_at(float(t)), png)
    if args.final_png:
        screenshot(states[-1][1], args.final_png)

    if not args.gif:
        return
    step = 1.0 / args.fps
    frames, durations = [], []
    t, last = 0.0, None
    while t <= end + step:
        snap = state_at(t)
        if snap == last and durations:
            durations[-1] += step
        else:
            frames.append(snap)
            durations.append(step)
            last = snap
        t += step
    durations[-1] += args.hold

    r = Renderer(1, args.title, rows_used, cols)
    base = r.chrome()
    images = [r.render(f, base) for f in frames]
    # One shared palette, so colours do not flicker between frames.
    sample = images[:: max(1, len(images) // 12)] + [images[-1]]
    strip = Image.new("RGB", (r.w, r.h * len(sample)))
    for i, im in enumerate(sample):
        strip.paste(im, (0, i * r.h))
    palette = strip.quantize(colors=255, method=Image.Quantize.MEDIANCUT)
    pal_frames = [im.quantize(palette=palette, dither=Image.Dither.NONE) for im in images]
    pal_frames[0].save(
        args.gif,
        save_all=True,
        append_images=pal_frames[1:],
        duration=[max(20, round(d * 1000)) for d in durations],
        loop=0,
        optimize=False,
    )
    print(f"wrote {args.gif}: {len(pal_frames)} frames, {end:.1f} s")


if __name__ == "__main__":
    main()
