#!/usr/bin/env python3
"""Generate `themes/banners/opencode.tdf` — a lowercase pixel alphabet in
the style of the opencode CLI logo, as a TheDraw COLOR font for bosun.

Proportions (per the logo):
  * x-height body          = 5 rows
  * ascender ("leg above") = +1 row  (b d f h k l t)
  * descender ("leg below")= +1 row  (g j p q y)
Glyphs are authored on a 7-row canvas:
  row 0      ascender line
  rows 1..5  x-height body (baseline = bottom of row 5)
  row 6      descender line
Square corners only. Rendered with half-block characters (▀ ▄ █) so two
pixel-rows share one terminal row — the glyphs land at the same compact
scale as bosun's other banner fonts, with roughly square pixels.
"""

import struct

# Optional dim background band behind the bottom body rows. Disabled:
# at the compact half-block scale a half-block cell carries only one
# foreground colour, so the band and the bright strokes can't coexist
# in a shared cell and it renders patchily. Kept as a flag for the
# full-block variant (CELLS_PER_COL path) where every pixel is its own
# cell and the band is clean.
BG_BAND = False
BAND_TOP = 3
BAND_BOT = 5
BRIGHT = 0x0F  # fg=15 -> full accent (strokes)
DIM = 0x08     # fg=8  -> ~33% accent (band)

# --- glyph bitmaps (7 rows each) ----------------------------------------
G = {
    # row 0 = ascender leg | rows 1-5 = body (rows 3-5 get the band) | row 6 = descender leg
    "a": ["....", "####", "...#", "####", "#..#", "####", "...."],
    "b": ["#...", "####", "#..#", "#..#", "#..#", "####", "...."],
    "c": ["....", "####", "#...", "#...", "#...", "####", "...."],
    "d": ["...#", "####", "#..#", "#..#", "#..#", "####", "...."],
    "e": ["....", "####", "#..#", "####", "#...", "####", "...."],
    "f": ["....", "####", "#...", "###.", "#..", "#..", "...."],
    "g": ["....", "####", "#..#", "#..#", "####", "...#", "####"],
    "h": ["#...", "###.", "#..#", "#..#", "#..#", "#..#", "...."],
    "i": ["#", ".", "#", "#", "#", "#", "."],
    "j": [".#", "..", ".#", ".#", ".#", ".#", "##"],
    "k": ["#...", "####", "#.#.", "#..#", "#..#", "#..#", "...."],
    "l": ["#.", "#.", "#.", "#.", "#.", "##", "."],
    "m": [".......", ".#####.", "#..#..#", "#..#..#", "#..#..#", "#..#..#", "......."],
    "n": ["....", "###.", "#..#", "#..#", "#..#", "#..#", "...."],
    "o": ["....", "####", "#..#", "#..#", "#..#", "####", "...."],
    "p": ["....", "####", "#..#", "#..#", "#..#", "####", "#..."],
    "q": ["....", "####", "#..#", "#..#", "#..#", "####", "...#"],
    "r": ["...", "###", "#..", "#..", "#..", "#..", "..."],
    "s": ["....", "####", "#...", "####", "...#", "####", "...."],
    "t": ["#..", "##.", "#..", "#..", "#..", "###", "..."],
    "u": ["....", "#..#", "#..#", "#..#", "#..#", "####", "...."],
    "v": [".....", "#...#", "#...#", "#...#", ".#.#.", "..#..", "....."],
    "w": [".......", "#..#..#", "#..#..#", "#..#..#", "#..#..#", ".#####.", "......."],
    "x": ["....", "#..#", "#..#", ".##.", "#..#", "#..#", "...."],
    "y": ["....", "#..#", "#..#", "#..#", "####", "...#", "###."],
    "z": ["....", "####", "...#", ".##.", "#...", "####", "...."],
}

ALPHABET = "abcdefghijklmnopqrstuvwxyz"

# --- CP437 half-block glyphs ---------------------------------------------
FULL = 0xDB   # █  both halves on
UPPER = 0xDF  # ▀  top half on
LOWER = 0xDC  # ▄  bottom half on
SPACE = 0x20


def normalize(rows):
    """Pad rows to the glyph's max width, add a 1-col inter-letter gap.
    Returns (rows_with_gap, content_width)."""
    w = max(len(r) for r in rows)
    out = [r.ljust(w, ".") + "." for r in rows]
    return out, w


def color_of(px, ry, x, content_w):
    """Pixel -> 0 (off), DIM (band fill), or BRIGHT (stroke)."""
    if px == "#":
        return BRIGHT
    if BG_BAND and BAND_TOP <= ry <= BAND_BOT and x < content_w:
        return DIM
    return 0


def cell_for(top, bot):
    """Two stacked pixel colours -> (cp437 glyph, attr=fg). A half-block
    cell carries one fg colour, so a mixed bright/dim cell resolves in
    favour of the bright (stroke) half."""
    if top == 0 and bot == 0:
        return (SPACE, 0x00)
    if top == bot:
        return (FULL, top)
    if bot == 0:
        return (UPPER, top)
    if top == 0:
        return (LOWER, bot)
    return (UPPER, BRIGHT) if top == BRIGHT else (LOWER, BRIGHT)


def half_rows(rows, content_w):
    """7 design rows -> 4 terminal rows of (cp437, attr) cells, packing
    two pixel-rows per row with half-blocks. A blank row is prepended so
    the ascender leg pairs cleanly above the body."""
    width = len(rows[0])  # includes the trailing gap column
    cgrid = [
        [color_of(rows[ry][x], ry, x, content_w) for x in range(width)]
        for ry in range(len(rows))
    ]
    padded = [[0] * width] + cgrid  # 8 rows -> 4 terminal rows
    return [
        [cell_for(padded[2 * t][x], padded[2 * t + 1][x]) for x in range(width)]
        for t in range(4)
    ]


def preview():
    bright = {FULL: "█", UPPER: "▀", LOWER: "▄", SPACE: " "}

    def ch(b, a):
        if b == SPACE:
            return " "
        return "▒" if a == DIM else bright[b]

    def render_word(word):
        glyphs = []
        for c in word:
            norm, cw = normalize(G[c])
            glyphs.append(half_rows(norm, cw))
        for t in range(4):
            line = "".join(ch(b, a) for g in glyphs for (b, a) in g[t])
            print(line.rstrip() if line.strip() else "")
        print()

    render_word("opencode")
    for i in range(0, 26, 13):
        render_word(ALPHABET[i : i + 13])


# --- TDF encoding --------------------------------------------------------
THE_DRAW_FONT_ID = b"TheDraw FONTS file"
FONT_INDICATOR = 0xFF00AA55
FONT_NAME_LEN = 12
FIRST, LAST = ord("!"), ord("~")
TABLE = LAST - FIRST + 1


def build_glyph(rows):
    norm, content_w = normalize(rows)
    cells = half_rows(norm, content_w)
    out = bytearray()
    out.append(len(norm[0]))  # width in cells (1 per column, incl. gap)
    out.append(len(cells))    # height: 4 terminal rows
    for i, row in enumerate(cells):
        for b, a in row:
            out.append(b)
            out.append(a)
        if i != len(cells) - 1:
            out.append(0x0D)
    out.append(0x00)
    return bytes(out)


def encode():
    block = bytearray()
    off = {}
    for ch in ALPHABET:
        off[ch] = len(block)
        block += build_glyph(G[ch])
    lookup = [0xFFFF] * TABLE
    for ch in ALPHABET:
        lookup[ord(ch) - FIRST] = off[ch]
        lookup[ord(ch.upper()) - FIRST] = off[ch]
    out = bytearray()
    out.append(len(THE_DRAW_FONT_ID) + 1)
    out += THE_DRAW_FONT_ID
    out.append(0x1A)
    out += struct.pack("<I", FONT_INDICATOR)
    name = b"opencode"
    out.append(FONT_NAME_LEN)
    out += name + b"\x00" * (FONT_NAME_LEN - len(name))
    out += b"\x00\x00\x00\x00"
    out.append(2)  # color
    out.append(1)  # spacing
    out += struct.pack("<H", len(block))
    for o in lookup:
        out += struct.pack("<H", o)
    out += block
    return bytes(out)


if __name__ == "__main__":
    import sys

    for ch in ALPHABET:
        assert len(G[ch]) == 7, f"{ch} must have 7 rows, has {len(G[ch])}"
    if "--write" in sys.argv:
        data = encode()
        with open("themes/banners/opencode.tdf", "wb") as f:
            f.write(data)
        print(f"wrote themes/banners/opencode.tdf ({len(data)} bytes)")
    else:
        preview()
