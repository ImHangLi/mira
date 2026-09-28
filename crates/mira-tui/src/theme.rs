//! The TUI palette and state glyphs. Colors come from the product illustrations (terracotta
//! roofs, sky, leaves, warm stone). No single tone reads well as text on both light and
//! dark terminals, so each tone has a light and a dark variant, chosen from the terminal
//! background (see [`Background`]). When the background is unknown, a middle set that
//! passes as marks on both is used, and colored words fall back to the default foreground.
//! Truecolor terminals get the exact tones, 256-color terminals a checked table entry,
//! 16-color terminals a named color, and `NO_COLOR` or `TERM=dumb` get none. The DIM
//! modifier is for decoration only; secondary text uses the `Muted` tone. State is never
//! shown by color alone: see [`Mark`].

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::{Color, Modifier, Style};

/// How many colors the terminal can show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMode {
    None,
    Basic,
    Indexed,
    TrueColor,
}

impl ColorMode {
    /// Reads `NO_COLOR`, `COLORTERM`, and `TERM` once at start.
    pub fn detect() -> Self {
        Self::from_env(|k| std::env::var(k).ok())
    }

    /// `NO_COLOR` counts only when it is set and not empty (no-color.org).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::None;
        }
        let low = |k: &str| get(k).unwrap_or_default().to_lowercase();
        let term = low("TERM");
        if term == "dumb" {
            return Self::None;
        }
        let colorterm = low("COLORTERM");
        if colorterm == "truecolor" || colorterm == "24bit" {
            return Self::TrueColor;
        }
        if term.contains("256") || term.contains("direct") {
            Self::Indexed
        } else {
            Self::Basic
        }
    }

    pub fn enabled(self) -> bool {
        self != Self::None
    }
}

/// `TERM=dumb`: the terminal cannot place the cursor, so the full-screen TUI cannot work.
pub fn term_is_dumb() -> bool {
    std::env::var("TERM").is_ok_and(|t| t.eq_ignore_ascii_case("dumb"))
}

/// The terminal background, as far as Mira can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Background {
    Light,
    Dark,
    Unknown,
}

impl Background {
    /// `MIRA_THEME=light|dark`; any other value means "detect".
    pub fn from_override(v: Option<&str>) -> Option<Self> {
        match v?.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// Light when black text has more contrast on the color than white text has.
    pub fn from_rgb(rgb: (u8, u8, u8)) -> Self {
        let l = luminance(rgb);
        if contrast_l(l, 0.0) > contrast_l(l, 1.0) {
            Self::Light
        } else {
            Self::Dark
        }
    }

    /// `COLORFGBG` (`fg;bg` or `fg;other;bg`): the last field is an ANSI color index.
    pub fn from_colorfgbg(v: Option<&str>) -> Option<Self> {
        let bg: u8 = v?.rsplit(';').next()?.trim().parse().ok()?;
        match bg {
            7 | 9..=15 => Some(Self::Light),
            0..=6 | 8 => Some(Self::Dark),
            _ => None,
        }
    }
}

/// Parses an OSC 11 reply (`ESC ] 11 ; rgb:RRRR/GGGG/BBBB` ended by BEL or ST) found
/// anywhere in `buf`. Each channel has 1 to 4 hex digits.
pub fn parse_osc11(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let start = buf.windows(5).position(|w| w == b"\x1b]11;")? + 5;
    let rest = &buf[start..];
    let end = rest.iter().position(|&b| b == 0x07 || b == 0x1b)?;
    let body = std::str::from_utf8(&rest[..end]).ok()?;
    let spec = body
        .strip_prefix("rgb:")
        .or_else(|| body.strip_prefix("rgba:"))?;
    let mut parts = spec.split('/').map(|p| {
        let n = p.len();
        let v = u32::from_str_radix(p, 16).ok()?;
        if !(1..=4).contains(&n) {
            return None;
        }
        let max = (1u32 << (4 * n)) - 1;
        u8::try_from((v * 255 + max / 2) / max).ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// WCAG 2.2 relative luminance of an sRGB color.
pub fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast_l(a: f64, b: f64) -> f64 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// WCAG 2.2 contrast ratio of two colors, from 1 to 21.
#[cfg(test)]
pub fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    contrast_l(luminance(a), luminance(b))
}

/// A named tone of the palette.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Terracotta: brand, selection, key chips, the active tab.
    Accent,
    /// A deeper terracotta for group labels.
    AccentDeep,
    /// Sky: branch, info, links.
    Sky,
    /// Leaf: running, succeeded.
    Leaf,
    /// Amber: starting, stopping, stale, background.
    Amber,
    /// Rose: failed, errors.
    Rose,
    /// Gray for secondary text: hints, ages, labels.
    Muted,
    /// Dark text on chip backgrounds.
    Ink,
}

impl Tone {
    pub const ALL: [Tone; 8] = [
        Tone::Accent,
        Tone::AccentDeep,
        Tone::Sky,
        Tone::Leaf,
        Tone::Amber,
        Tone::Rose,
        Tone::Muted,
        Tone::Ink,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// The truecolor value on `bg`. `None`: the default foreground.
    pub fn rgb(self, bg: Background) -> Option<(u8, u8, u8)> {
        use Background::*;
        Some(match (self, bg) {
            (Tone::Ink, _) => (0x1C, 0x1C, 0x1C),
            // "Meadow": the light tones come from the daylight painting (terracotta, moss, sky,
            // leaf, ochre, brick, and olive gray).
            (Tone::Accent, Light) => (0xB0, 0x42, 0x16),
            (Tone::AccentDeep, Light) => (0x3E, 0x5E, 0x28),
            (Tone::Sky, Light) => (0x0A, 0x5F, 0xA8),
            (Tone::Leaf, Light) => (0x3A, 0x6B, 0x22),
            (Tone::Amber, Light) => (0x85, 0x5F, 0x1C),
            (Tone::Rose, Light) => (0xAE, 0x3C, 0x27),
            (Tone::Muted, Light) => (0x5A, 0x5E, 0x4C),
            // The dark tones come from the blue-hour version of the painting: terracotta,
            // lamplit gold, dusk blue, moonlit green, amber, warm rose, and blue gray.
            (Tone::Accent, Dark) => (0xF2, 0x6B, 0x3A),
            (Tone::AccentDeep, Dark) => (0xD8, 0xAE, 0x78),
            (Tone::Sky, Dark) => (0x7F, 0xA6, 0xE0),
            (Tone::Leaf, Dark) => (0x86, 0xB3, 0x6C),
            (Tone::Amber, Dark) => (0xE2, 0xB0, 0x5A),
            (Tone::Rose, Dark) => (0xE8, 0x7E, 0x6E),
            (Tone::Muted, Dark) => (0x9C, 0xA4, 0xA8),
            // Marks that pass 3:1 on light and dark backgrounds alike.
            (Tone::Accent, Unknown) => (0xEC, 0x4A, 0x10),
            (Tone::AccentDeep, Unknown) => (0xC8, 0x5A, 0x30),
            (Tone::Sky, Unknown) => (0x3B, 0x82, 0xC4),
            (Tone::Leaf, Unknown) => (0x4A, 0x93, 0x55),
            (Tone::Amber, Unknown) => (0xAE, 0x7A, 0x1F),
            (Tone::Rose, Unknown) => (0xD9, 0x53, 0x4F),
            (Tone::Muted, Unknown) => return None,
        })
    }

    /// The xterm 256-color entry on `bg`: the nearest entry of the 6x6x6 cube or the gray
    /// ramp that keeps the contrast of the truecolor value (the tests check each one).
    pub fn indexed(self, bg: Background) -> Option<u8> {
        use Background::*;
        Some(match (self, bg) {
            (Tone::Ink, _) => 234,
            (Tone::Accent, Light) => 94,
            (Tone::AccentDeep, Light) => 58,
            (Tone::Sky, Light) => 25,
            (Tone::Leaf, Light) => 22,
            (Tone::Amber, Light) => 94,
            (Tone::Rose, Light) => 124,
            (Tone::Muted, Light) => 59,
            (Tone::Accent, Dark) => 209,
            (Tone::AccentDeep, Dark) => 180,
            (Tone::Sky, Dark) => 110,
            (Tone::Leaf, Dark) => 107,
            (Tone::Amber, Dark) => 179,
            (Tone::Rose, Dark) => 174,
            (Tone::Muted, Dark) => 247,
            (Tone::Accent | Tone::AccentDeep, Unknown) => 166,
            (Tone::Sky, Unknown) => 67,
            (Tone::Leaf, Unknown) => 65,
            (Tone::Amber, Unknown) => 130,
            (Tone::Rose, Unknown) => 131,
            (Tone::Muted, Unknown) => return None,
        })
    }

    /// The 16-color entry. Accent is magenta, so brand and selection never read as errors.
    fn basic(self) -> Option<Color> {
        Some(match self {
            Tone::Accent | Tone::AccentDeep => Color::Magenta,
            Tone::Sky => Color::Blue,
            Tone::Leaf => Color::Green,
            Tone::Amber => Color::Yellow,
            Tone::Rose => Color::Red,
            Tone::Muted => return None,
            Tone::Ink => Color::Black,
        })
    }

    /// A chip background. Chips carry their own contrast (dark ink on a mid tone), so they
    /// are the same on every background.
    pub fn chip_rgb(self) -> (u8, u8, u8) {
        match self {
            Tone::Sky => (0x5B, 0x9B, 0xD5),
            t => t.rgb(Background::Dark).unwrap_or((0x9A, 0x9A, 0x9A)),
        }
    }

    pub fn chip_indexed(self) -> u8 {
        match self {
            Tone::Sky => 68,
            t => t.indexed(Background::Dark).unwrap_or(247),
        }
    }

    fn chip_basic(self) -> Option<Color> {
        match self {
            Tone::Sky => Some(Color::Cyan),
            t => t.basic(),
        }
    }
}

/// The RGB value of an xterm 256-color index from 16 up.
#[cfg(test)]
pub fn index_rgb(i: u8) -> (u8, u8, u8) {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if i >= 232 {
        let v = 8 + 10 * (i - 232);
        return (v, v, v);
    }
    let i = usize::from(i.saturating_sub(16));
    (STEPS[i / 36], STEPS[(i / 6) % 6], STEPS[i % 6])
}

pub struct Theme {
    pub mode: ColorMode,
    pub bg: Background,
    fg: [Option<Color>; 8],
    chip_bg: [Option<Color>; 8],
}

impl Theme {
    pub fn new(mode: ColorMode, bg: Background) -> Self {
        let pick = |tone: Tone, chip: bool| -> Option<Color> {
            match mode {
                ColorMode::None => None,
                ColorMode::Basic if chip => tone.chip_basic(),
                ColorMode::Basic => tone.basic(),
                ColorMode::Indexed if chip => Some(Color::Indexed(tone.chip_indexed())),
                ColorMode::Indexed => tone.indexed(bg).map(Color::Indexed),
                ColorMode::TrueColor if chip => {
                    let (r, g, b) = tone.chip_rgb();
                    Some(Color::Rgb(r, g, b))
                }
                ColorMode::TrueColor => tone.rgb(bg).map(|(r, g, b)| Color::Rgb(r, g, b)),
            }
        };
        Self {
            mode,
            bg,
            fg: Tone::ALL.map(|t| pick(t, false)),
            chip_bg: Tone::ALL.map(|t| pick(t, true)),
        }
    }

    pub fn color(&self, tone: Tone) -> Option<Color> {
        self.fg[tone.index()]
    }

    /// A mark, a border, or a cursor in `tone`.
    pub fn fg(&self, tone: Tone) -> Style {
        self.color(tone)
            .map_or(Style::default(), |c| Style::default().fg(c))
    }

    /// Words in `tone`. Without a known background the middle tones are too weak for
    /// text, so words use the default foreground; in 16 colors, amber words are bold.
    pub fn word(&self, tone: Tone) -> Style {
        match (self.mode, self.bg, tone) {
            (ColorMode::Basic, _, Tone::Amber) => self.bold(),
            (ColorMode::Indexed | ColorMode::TrueColor, Background::Unknown, _) => Style::default(),
            _ => self.fg(tone),
        }
    }

    /// Secondary text: hints, ages, labels. The default foreground when no gray passes.
    pub fn muted(&self) -> Style {
        self.fg(Tone::Muted)
    }

    pub fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// Faint decoration: rules and unfocused borders. Never for text.
    pub fn dim(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// A filled chip: `tone` background with dark text. Without color: reversed.
    pub fn chip(&self, tone: Tone) -> Style {
        match (self.chip_bg[tone.index()], self.color(Tone::Ink)) {
            (Some(bg), Some(ink)) => Style::default().bg(bg).fg(ink).add_modifier(Modifier::BOLD),
            _ => Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        }
    }

    /// The selected row of a focused list: the sky of the painting, so it stands apart
    /// from the terracotta of key chips and marks.
    pub fn selected(&self) -> Style {
        self.chip(Tone::Sky)
    }

    /// Panel borders: accent when the panel has the focus, dim otherwise.
    pub fn border(&self, focus: bool) -> Style {
        if focus && self.mode.enabled() {
            self.fg(Tone::Accent)
        } else if focus {
            self.bold()
        } else {
            self.dim()
        }
    }
}

/// A state as a glyph, a word, and a tone; the glyph and word carry it without color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mark {
    pub glyph: &'static str,
    pub word: &'static str,
    /// `None` draws the mark muted.
    pub tone: Option<Tone>,
}

impl Mark {
    pub const fn new(glyph: &'static str, word: &'static str, tone: Option<Tone>) -> Self {
        Self { glyph, word, tone }
    }

    /// The glyph's style.
    pub fn style(&self, t: &Theme) -> Style {
        self.tone.map_or(t.muted(), |c| t.fg(c))
    }

    /// The word's style.
    pub fn word_style(&self, t: &Theme) -> Style {
        self.tone.map_or(t.muted(), |c| t.word(c))
    }
}

pub const RUNNING: Mark = Mark::new("●", "running", Some(Tone::Leaf));
pub const OK: Mark = Mark::new("✓", "ok", Some(Tone::Leaf));
pub const FAILED: Mark = Mark::new("✗", "failed", Some(Tone::Rose));
pub const STARTING: Mark = Mark::new("◐", "starting", Some(Tone::Amber));
pub const STOPPING: Mark = Mark::new("◐", "stopping", Some(Tone::Amber));
pub const NOT_RUN: Mark = Mark::new("○", "not run yet", None);
pub const DISABLED: Mark = Mark::new("‖", "disabled", None);
pub const STOPPED: Mark = Mark::new("■", "stopped", None);

// ----- ASCII mode ---------------------------------------------------------------------

static ASCII: AtomicBool = AtomicBool::new(false);
static UTF8: AtomicBool = AtomicBool::new(true);

/// Whether the locale (`LC_ALL`, then `LC_CTYPE`, then `LANG`) names UTF-8. A set locale
/// that is not UTF-8 (for example `C` or `POSIX`) means no. No locale variable at all means
/// yes: macOS terminals launched from the Dock often set none, and they are UTF-8.
pub fn locale_is_utf8(get: impl Fn(&str) -> Option<String>) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .is_none_or(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
}

/// Chooses the glyph set once at start: `MIRA_ASCII=1` forces ASCII, `MIRA_ASCII=0`
/// forces Unicode, and otherwise a locale that is not UTF-8 gets ASCII.
pub fn detect_glyphs() {
    let get = |k: &str| std::env::var(k).ok();
    let utf8 = locale_is_utf8(get);
    let ascii = match get("MIRA_ASCII").as_deref().map(str::trim) {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => !utf8,
    };
    ASCII.store(ascii, Ordering::Relaxed);
    UTF8.store(utf8, Ordering::Relaxed);
}

/// Whether the screen uses ASCII glyphs and `+-|` borders.
pub fn ascii() -> bool {
    ASCII.load(Ordering::Relaxed)
}

/// The ASCII stand-in for one drawn cell in ASCII mode; `None` keeps the cell. Mark
/// glyphs stay distinct: `* v x ~ o - =` (see the help overlay). Other non-ASCII text
/// becomes `?` only when the locale cannot show it.
pub fn ascii_cell(symbol: &str) -> Option<&'static str> {
    if symbol.is_ascii() {
        return None;
    }
    Some(match symbol {
        "╭" | "╮" | "╰" | "╯" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼" => {
            "+"
        }
        "─" | "━" => "-",
        "│" | "┃" => "|",
        "●" => "*",
        "✓" => "v",
        "✗" => "x",
        "◐" => "~",
        "○" | "◌" => "o",
        "‖" => "-",
        "■" => "=",
        "◆" | "◇" => "@",
        "·" => "|",
        "…" => ".",
        "▌" | "▸" | "›" => ">",
        "‹" => "<",
        "▎" => "!",
        "▏" => "_",
        _ if UTF8.load(Ordering::Relaxed) => return None,
        _ => "?",
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_missing_locale_counts_as_utf8_but_c_does_not() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert!(super::locale_is_utf8(env(&[])));
        assert!(super::locale_is_utf8(env(&[("LANG", "en_AU.UTF-8")])));
        assert!(!super::locale_is_utf8(env(&[("LC_ALL", "C")])));
        assert!(!super::locale_is_utf8(env(&[
            ("LC_ALL", "POSIX"),
            ("LANG", "en_AU.UTF-8")
        ])));
    }

    use super::{Background, Tone, contrast, index_rgb};

    const DARK_BGS: [(u8, u8, u8); 2] = [(0x1E, 0x1E, 0x1E), (0, 0, 0)];
    const LIGHT_BGS: [(u8, u8, u8); 2] = [(0xFF, 0xFF, 0xFF), (0xE6, 0xE6, 0xE6)];
    const TEXT: f64 = 4.5;
    const MARK: f64 = 3.0;
    const COLORED: [Tone; 7] = [
        Tone::Accent,
        Tone::AccentDeep,
        Tone::Sky,
        Tone::Leaf,
        Tone::Amber,
        Tone::Rose,
        Tone::Muted,
    ];

    fn check(tone: Tone, rgb: (u8, u8, u8), bgs: &[(u8, u8, u8)], min: f64, what: &str) {
        for bg in bgs {
            let r = contrast(rgb, *bg);
            assert!(
                r >= min,
                "{what} {tone:?} {rgb:02X?} on {bg:02X?}: {r:.2} < {min}"
            );
        }
    }

    #[test]
    fn the_contrast_formula_matches_wcag() {
        assert!((contrast((0, 0, 0), (0xFF, 0xFF, 0xFF)) - 21.0).abs() < 1e-9);
        assert!((contrast((0x77, 0x77, 0x77), (0xFF, 0xFF, 0xFF)) - 4.48).abs() < 0.01);
    }

    #[test]
    fn every_tone_passes_as_text_on_its_background() {
        for tone in COLORED {
            for (bg, bgs) in [(Background::Light, LIGHT_BGS), (Background::Dark, DARK_BGS)] {
                let rgb = tone.rgb(bg).unwrap_or_default();
                check(tone, rgb, &bgs, TEXT, "truecolor text");
                let idx = tone.indexed(bg).map(index_rgb).unwrap_or_default();
                check(tone, idx, &bgs, TEXT, "256-color text");
            }
        }
    }

    #[test]
    fn the_unknown_background_set_passes_as_marks_on_both() {
        let all = [LIGHT_BGS, DARK_BGS].concat();
        for tone in COLORED {
            let Some(rgb) = tone.rgb(Background::Unknown) else {
                assert_eq!(tone, Tone::Muted);
                continue;
            };
            check(tone, rgb, &all, MARK, "truecolor mark");
            let idx = tone.indexed(Background::Unknown).map(index_rgb);
            check(tone, idx.unwrap_or_default(), &all, MARK, "256-color mark");
        }
    }

    #[test]
    fn chip_text_passes_on_every_chip() {
        let ink = Tone::Ink.rgb(Background::Dark).unwrap_or_default();
        for tone in [Tone::Accent, Tone::Sky] {
            let r = contrast(tone.chip_rgb(), ink);
            assert!(r >= TEXT, "chip {tone:?}: {r:.2}");
            let r = contrast(index_rgb(tone.chip_indexed()), index_rgb(234));
            assert!(r >= TEXT, "256-color chip {tone:?}: {r:.2}");
        }
        let sky = contrast(Tone::Sky.chip_rgb(), ink);
        assert!((sky - 5.76).abs() < 0.01, "{sky:.2}");
    }

    #[test]
    fn index_rgb_reads_the_cube_and_the_gray_ramp() {
        assert_eq!(index_rgb(16), (0, 0, 0));
        assert_eq!(index_rgb(196), (0xFF, 0, 0));
        assert_eq!(index_rgb(234), (0x1C, 0x1C, 0x1C));
        assert_eq!(index_rgb(209), (0xFF, 0x87, 0x5F));
    }
}
