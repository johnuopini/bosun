//! Outer-terminal default-color probe + OSC 10/11/12 query answering
//! for the embedded session PTY.
//!
//! Apps like Codex and Neovim send OSC 10 (foreground) / OSC 11
//! (background) / OSC 12 (cursor) queries at startup to choose a light
//! vs dark palette. Inside bosun those queries travel through bosun's
//! tmux server to bosun's embed PTY — whose "terminal" is just a
//! `vt100` parser that never answers them. The app then times out and
//! assumes a dark background, so e.g. Codex renders diffs with its
//! dark palette on a light terminal. See issue #2.
//!
//! The fix has two halves. First, [`probe`] asks the *real* outer
//! terminal for its fg/bg/cursor once at startup (before the input
//! actor takes over stdin). Second, `EmbedTerminal` replays those
//! answers whenever it spots a query in the inner byte stream (see
//! [`QueryScanner`]). If the real terminal doesn't answer, callers
//! fall back to the active theme's colors.

/// A 48-bit RGB color — 16 bits per channel, the precision OSC color
/// responses use (`rgb:RRRR/GGGG/BBBB`).
pub type Rgb16 = (u16, u16, u16);

/// Default colors learned from the outer terminal. Any field may be
/// `None` if the terminal didn't answer that query; callers fill the
/// gap from the active theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TermColors {
    pub fg: Option<Rgb16>,
    pub bg: Option<Rgb16>,
    pub cursor: Option<Rgb16>,
}

/// Concrete default colors handed to an embed so it can answer
/// OSC 10/11/12 queries. Unlike [`TermColors`], every slot is filled
/// — the caller has already substituted theme colors for anything the
/// outer-terminal probe didn't return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultColors {
    pub fg: Rgb16,
    pub bg: Rgb16,
    pub cursor: Rgb16,
}

impl DefaultColors {
    /// The response bytes for a query of `kind`, terminated to match.
    pub fn response(&self, kind: ColorKind, term: Terminator) -> Vec<u8> {
        let rgb = match kind {
            ColorKind::Fg => self.fg,
            ColorKind::Bg => self.bg,
            ColorKind::Cursor => self.cursor,
        };
        format_response(kind, rgb, term)
    }
}

/// Which default-color slot an OSC sequence refers to. The numeric
/// value is the OSC command number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKind {
    Fg = 10,
    Bg = 11,
    Cursor = 12,
}

impl ColorKind {
    fn from_num(n: u16) -> Option<Self> {
        match n {
            10 => Some(ColorKind::Fg),
            11 => Some(ColorKind::Bg),
            12 => Some(ColorKind::Cursor),
            _ => None,
        }
    }
}

/// The string terminator an OSC sequence used: BEL (`\x07`) or
/// ST (`ESC \`). We echo a response back with the same terminator the
/// query used so picky parsers stay happy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    Bel,
    St,
}

impl Terminator {
    fn bytes(self) -> &'static [u8] {
        match self {
            Terminator::Bel => b"\x07",
            Terminator::St => b"\x1b\\",
        }
    }
}

/// One parsed OSC 10/11/12 sequence and where it sat in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OscColor {
    kind: ColorKind,
    /// The payload between the second `;`-field and the terminator —
    /// `b"?"` for a query, or a color spec like `b"rgb:ffff/ffff/ffff"`
    /// for a response.
    spec: Vec<u8>,
    term: Terminator,
    /// Byte index one past the terminator.
    end: usize,
}

/// Scan `buf` for OSC 10/11/12 sequences. Used both ways: to read the
/// terminal's *responses* during the startup probe and to spot inner
/// apps' *queries* in the embed stream. Anything that isn't a clean
/// `ESC ] (10|11|12) ; <spec> (BEL|ST)` is skipped.
fn find_osc_colors(buf: &[u8]) -> Vec<OscColor> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b']' {
            if let Some(seq) = parse_one(&buf[i..]) {
                let end = i + seq.end;
                out.push(OscColor { end, ..seq });
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Parse a single OSC sequence assumed to start at `s[0..2] == ESC ]`.
/// Returns `None` if it isn't a complete 10/11/12 color sequence.
fn parse_one(s: &[u8]) -> Option<OscColor> {
    debug_assert!(s.len() >= 2 && s[0] == 0x1b && s[1] == b']');
    // Number field: digits up to the first ';'.
    let mut p = 2;
    let num_start = p;
    while p < s.len() && s[p].is_ascii_digit() {
        p += 1;
    }
    if p == num_start || p >= s.len() || s[p] != b';' {
        return None;
    }
    let num: u16 = std::str::from_utf8(&s[num_start..p]).ok()?.parse().ok()?;
    let kind = ColorKind::from_num(num)?;
    p += 1; // skip ';'

    // Payload runs until BEL or ST. Bail if we hit another ESC that
    // isn't the start of an ST (truncated / interleaved sequence).
    let spec_start = p;
    while p < s.len() {
        match s[p] {
            0x07 => {
                return Some(OscColor {
                    kind,
                    spec: s[spec_start..p].to_vec(),
                    term: Terminator::Bel,
                    end: p + 1,
                });
            }
            0x1b => {
                if p + 1 < s.len() && s[p + 1] == b'\\' {
                    return Some(OscColor {
                        kind,
                        spec: s[spec_start..p].to_vec(),
                        term: Terminator::St,
                        end: p + 2,
                    });
                }
                return None; // lone ESC inside the payload — not ours
            }
            _ => p += 1,
        }
    }
    None // no terminator yet (incomplete)
}

/// Parse a color spec from an OSC response into 16-bit RGB. Handles
/// the common `rgb:RRRR/GGGG/BBBB` form (any 1–4 hex digits per
/// channel, scaled to 16-bit) and the `#RRGGBB` fallback.
fn parse_color_spec(spec: &[u8]) -> Option<Rgb16> {
    let s = std::str::from_utf8(spec).ok()?.trim();
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut it = rest.split('/');
        let r = scale_hex(it.next()?)?;
        let g = scale_hex(it.next()?)?;
        let b = scale_hex(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some((dup8(r), dup8(g), dup8(b)));
        }
    }
    None
}

/// Scale a 1–4 hex-digit channel string to a 16-bit value the way X
/// color parsing does (e.g. `ff` → `0xffff`, `f` → `0xffff`).
fn scale_hex(h: &str) -> Option<u16> {
    if h.is_empty() || h.len() > 4 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    let max = (1u32 << (4 * h.len())) - 1;
    Some(((v * 0xffff + max / 2) / max) as u16)
}

/// Replicate an 8-bit channel into 16 bits (`0xAB` → `0xABAB`).
fn dup8(c: u8) -> u16 {
    ((c as u16) << 8) | c as u16
}

/// Format an OSC color *response* for `kind` with `rgb`, terminated to
/// match the query. This is what we write back into the embed PTY.
pub fn format_response(kind: ColorKind, rgb: Rgb16, term: Terminator) -> Vec<u8> {
    let mut v = format!(
        "\x1b]{};rgb:{:04x}/{:04x}/{:04x}",
        kind as u16, rgb.0, rgb.1, rgb.2
    )
    .into_bytes();
    v.extend_from_slice(term.bytes());
    v
}

/// Pull every OSC 10/11/12 color *response* out of `buf` and merge
/// them into `out`. Used by the startup probe to read the terminal's
/// reply.
pub fn parse_responses(buf: &[u8], out: &mut TermColors) {
    for seq in find_osc_colors(buf) {
        if seq.spec == b"?" {
            continue; // a query, not a response
        }
        if let Some(rgb) = parse_color_spec(&seq.spec) {
            match seq.kind {
                ColorKind::Fg => out.fg = Some(rgb),
                ColorKind::Bg => out.bg = Some(rgb),
                ColorKind::Cursor => out.cursor = Some(rgb),
            }
        }
    }
}

/// Incremental scanner for OSC 10/11/12 *queries* in the embed byte
/// stream. Carries an incomplete trailing sequence between calls so a
/// query split across PTY read chunks is still recognized. Returns the
/// `(kind, terminator)` of every complete query found in this chunk.
#[derive(Debug, Default)]
pub struct QueryScanner {
    carry: Vec<u8>,
}

/// Max bytes we'll hold waiting for a query to complete. A real
/// `ESC ] 11 ; ? ST` is ~8 bytes; anything longer without a
/// terminator isn't a color query, so we don't let the carry grow.
const CARRY_CAP: usize = 64;

impl QueryScanner {
    /// Feed the next chunk of inner PTY output; returns any complete
    /// color queries it contained.
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<(ColorKind, Terminator)> {
        // Cheap exit: no ESC anywhere and nothing pending → nothing to do.
        if self.carry.is_empty() && !chunk.contains(&0x1b) {
            return Vec::new();
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);

        let seqs = find_osc_colors(&buf);
        let mut out = Vec::new();
        let mut consumed = 0;
        for seq in &seqs {
            if seq.spec == b"?" {
                out.push((seq.kind, seq.term));
            }
            consumed = seq.end;
        }

        // Keep a bounded tail: the bytes after the last complete
        // sequence, trimmed to the last `ESC` so a split query can
        // finish on the next call. Drop everything if no `ESC` remains.
        let tail = &buf[consumed..];
        self.carry = match tail.iter().rposition(|&b| b == 0x1b) {
            Some(pos) => tail[pos..].to_vec(),
            None => Vec::new(),
        };
        if self.carry.len() > CARRY_CAP {
            self.carry.clear();
        }
        out
    }
}

/// Probe the outer terminal for its default fg/bg/cursor colors by
/// sending OSC 10/11/12 queries and reading the replies, up to
/// `timeout`. MUST run before the input actor starts reading stdin —
/// it reads raw bytes from fd 0 directly.
///
/// Best-effort: returns whatever the terminal answered within the
/// window (possibly nothing). Type-ahead typed during the probe window
/// is consumed and dropped, but the window is short and startup
/// type-ahead is rare.
#[cfg(unix)]
pub fn probe(timeout: std::time::Duration) -> TermColors {
    use std::io::Write;
    use std::time::Instant;

    let mut out = TermColors::default();

    // Ask for foreground, background, and cursor in one write.
    let query = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]12;?\x1b\\";
    {
        let mut stdout = std::io::stdout();
        if stdout.write_all(query).is_err() || stdout.flush().is_err() {
            return out;
        }
    }

    let deadline = Instant::now() + timeout;
    let mut acc: Vec<u8> = Vec::with_capacity(128);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let mut fds = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, count 1.
        let n = unsafe { libc::poll(&mut fds, 1, ms) };
        if n <= 0 {
            break; // timeout (0) or error (-1)
        }
        if fds.revents & libc::POLLIN == 0 {
            break;
        }
        let mut tmp = [0u8; 256];
        // SAFETY: reading into a valid local buffer.
        let r = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                tmp.as_mut_ptr() as *mut libc::c_void,
                tmp.len(),
            )
        };
        if r <= 0 {
            break;
        }
        acc.extend_from_slice(&tmp[..r as usize]);
        parse_responses(&acc, &mut out);
        // Stop early once we have all three.
        if out.fg.is_some() && out.bg.is_some() && out.cursor.is_some() {
            break;
        }
    }
    out
}

/// No-op probe on non-unix targets (bosun ships for unix; this keeps
/// the crate compiling elsewhere — callers just fall back to theme
/// colors).
#[cfg(not(unix))]
pub fn probe(_timeout: std::time::Duration) -> TermColors {
    TermColors::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_hex_matches_x_semantics() {
        assert_eq!(scale_hex("ff"), Some(0xffff));
        assert_eq!(scale_hex("00"), Some(0x0000));
        assert_eq!(scale_hex("f"), Some(0xffff));
        assert_eq!(scale_hex("ffff"), Some(0xffff));
        assert_eq!(scale_hex("80"), Some(0x8080));
        assert_eq!(scale_hex(""), None);
        assert_eq!(scale_hex("xyz"), None);
        assert_eq!(scale_hex("12345"), None);
    }

    #[test]
    fn parse_rgb_and_hex_specs() {
        assert_eq!(
            parse_color_spec(b"rgb:ffff/ffff/ffff"),
            Some((0xffff, 0xffff, 0xffff))
        );
        assert_eq!(parse_color_spec(b"rgb:0000/0000/0000"), Some((0, 0, 0)));
        assert_eq!(parse_color_spec(b"rgb:ff/00/80"), Some((0xffff, 0, 0x8080)));
        assert_eq!(parse_color_spec(b"#ffffff"), Some((0xffff, 0xffff, 0xffff)));
        assert_eq!(parse_color_spec(b"?"), None);
        assert_eq!(parse_color_spec(b"garbage"), None);
    }

    #[test]
    fn parse_responses_reads_fg_bg_cursor() {
        let buf = b"\x1b]10;rgb:1111/2222/3333\x1b\\\x1b]11;rgb:eeee/eeee/eeee\x07\x1b]12;rgb:abab/cdcd/efef\x1b\\";
        let mut c = TermColors::default();
        parse_responses(buf, &mut c);
        assert_eq!(c.fg, Some((0x1111, 0x2222, 0x3333)));
        assert_eq!(c.bg, Some((0xeeee, 0xeeee, 0xeeee)));
        assert_eq!(c.cursor, Some((0xabab, 0xcdcd, 0xefef)));
    }

    #[test]
    fn parse_responses_ignores_unrelated_osc() {
        // OSC 0 title + a CSI, no color responses.
        let buf = b"\x1b]0;my title\x07\x1b[1;31mhi";
        let mut c = TermColors::default();
        parse_responses(buf, &mut c);
        assert_eq!(c, TermColors::default());
    }

    #[test]
    fn scanner_finds_query_with_bel_and_st() {
        let mut s = QueryScanner::default();
        let found = s.scan(b"\x1b]11;?\x07");
        assert_eq!(found, vec![(ColorKind::Bg, Terminator::Bel)]);
        let found = s.scan(b"\x1b]10;?\x1b\\");
        assert_eq!(found, vec![(ColorKind::Fg, Terminator::St)]);
    }

    #[test]
    fn scanner_handles_query_split_across_chunks() {
        let mut s = QueryScanner::default();
        assert!(s.scan(b"data\x1b]11").is_empty());
        let found = s.scan(b";?\x07more");
        assert_eq!(found, vec![(ColorKind::Bg, Terminator::Bel)]);
    }

    #[test]
    fn scanner_ignores_non_color_osc_and_plain_text() {
        let mut s = QueryScanner::default();
        assert!(s.scan(b"\x1b]0;title\x07plain text").is_empty());
        assert!(s.scan(b"no escapes here at all").is_empty());
        // A response (not a query) must not be echoed back as a query.
        assert!(s.scan(b"\x1b]11;rgb:0000/0000/0000\x07").is_empty());
    }

    #[test]
    fn format_response_round_trips_through_parser() {
        let bytes = format_response(ColorKind::Bg, (0xabcd, 0x1234, 0x00ff), Terminator::St);
        let mut c = TermColors::default();
        parse_responses(&bytes, &mut c);
        assert_eq!(c.bg, Some((0xabcd, 0x1234, 0x00ff)));
    }

    #[test]
    fn scanner_does_not_grow_carry_unbounded() {
        let mut s = QueryScanner::default();
        // A dangling ESC] with no terminator and lots of junk must not
        // accumulate past the cap.
        let mut junk = vec![0x1b, b']'];
        junk.extend(std::iter::repeat(b'x').take(1000));
        s.scan(&junk);
        assert!(s.carry.len() <= CARRY_CAP);
    }
}
