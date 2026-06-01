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
Square corners only. Each design column renders as TWO terminal cells so
the pixels read square (a terminal cell is ~1:2, so 2 wide ≈ 1 tall).
"""

import struct

# Render scale: terminal cells per design column (2 => square pixels).
CELLS_PER_COL = 2
# Subtle background band behind the BOTTOM 3 ROWS of the x-height body
# (rows 3..5 on the 7-row canvas), spanning each glyph's content width
# incl. counters. The top 2 body rows and the dangling leg rows (0, 6)
# get no fill. Painted as a dim foreground block (bosun's bg attribute
# maxes at ~67% accent, too strong; a dim fg block at index 8 is the
# subtlest solid available ≈ 33% accent).
BG_BAND = True
BAND_TOP = 3
BAND_BOT = 5
DIM_FG = 8  # DOS index -> ~33% accent tint in bosun

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

# --- CP437 / attrs -------------------------------------------------------
FULL = 0xDB  # █
SPACE = 0x20
FG_ON = 0x0F  # fg=15 -> full accent
ATTR_OFF = 0x00


def normalize(rows):
    """Pad rows to content width, add a 1-col inter-letter gap.
    Returns (rows_with_gap, content_width)."""
    w = max(len(r) for r in rows)
    out = [r.ljust(w, ".") + "." for r in rows]
    return out, w


def row_cells(row, ry, content_w):
    """One design row -> list of (cp437, attr) cells (CELLS_PER_COL each).
    The subtle band fills non-stroke content cells (not the trailing gap
    column, not the leg rows) on rows BAND_TOP..BAND_BOT."""
    cells = []
    for x, px in enumerate(row):
        if px == "#":
            cell = (FULL, FG_ON)
        elif BG_BAND and BAND_TOP <= ry <= BAND_BOT and x < content_w:
            cell = (FULL, DIM_FG)
        else:
            cell = (SPACE, ATTR_OFF)
        cells.extend([cell] * CELLS_PER_COL)
    return cells


def preview():
    block = {FULL: "█", SPACE: " "}
    dim = "▒"

    def render_word(word):
        glyphs = [normalize(G[c]) for c in word]
        for ry in range(7):
            line = ""
            for rows, cw in glyphs:
                for b, a in row_cells(rows[ry], ry, cw):
                    line += dim if (b == FULL and a == DIM_FG) else block.get(b, " ")
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
    total_w = content_w + 1  # includes the trailing gap column
    out = bytearray()
    out.append(total_w * CELLS_PER_COL)
    out.append(len(norm))  # height in rows (7)
    for i, row in enumerate(norm):
        for b, a in row_cells(row, i, content_w):
            out.append(b)
            out.append(a)
        if i != len(norm) - 1:
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
