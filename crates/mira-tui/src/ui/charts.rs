//! Chart and stats views, drawn as text: bars with eighth blocks, lines in braille dots,
//! and stats as a grid of tiles. Every color also has a glyph or a word, so nothing
//! depends on color alone.

use mira_protocol::view::{ChartStyle, Series, Stat, StatTone, ViewData};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::logs::{cells, display, slice_cells};
use crate::theme::{self, Theme, Tone};
use crate::views::{ViewPane, number};

/// Series colors, in order. A ninth series cannot exist (at most eight).
const SERIES: [Tone; 8] = [
    Tone::Accent,
    Tone::Sky,
    Tone::Leaf,
    Tone::Amber,
    Tone::Rose,
    Tone::AccentDeep,
    Tone::Muted,
    Tone::Ink,
];

/// Series marks for the legend, so a series is known by its glyph as well as its color.
const MARKS: [&str; 8] = ["●", "■", "◆", "▲", "▼", "○", "□", "◇"];
const ASCII_MARKS: [&str; 8] = ["*", "#", "@", "+", "x", "o", "=", "%"];

fn mark(i: usize) -> &'static str {
    if theme::ascii() {
        ASCII_MARKS[i % 8]
    } else {
        MARKS[i % 8]
    }
}

/// The body of a chart or stats view, `w` cells wide and `h` rows high.
pub(super) fn lines(p: &mut ViewPane, t: &Theme, focus: bool) -> Vec<Line<'static>> {
    let (w, h) = (p.width.max(1), p.height.max(1));
    let sel = if focus {
        t.selected()
    } else {
        t.bold().add_modifier(Modifier::UNDERLINED)
    };
    let cursor = p.cursor;
    match p.data() {
        Some(ViewData::Chart {
            style: ChartStyle::Bar,
            labels,
            series,
        }) => {
            let (out, top) = bars(labels, series, cursor, p.top, w, h, t, sel);
            p.top = top;
            out
        }
        Some(ViewData::Chart {
            style: ChartStyle::Line,
            labels,
            series,
        }) => plot(labels, series, cursor, w, h, t, sel),
        Some(ViewData::Stats { stats }) => tiles(stats, cursor, w, h, t, sel),
        _ => Vec::new(),
    }
}

fn legend(series: &[Series], cursor: Option<usize>, t: &Theme, sel: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, s) in series.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(format!("{} ", mark(i)), t.fg(SERIES[i % 8])));
        let st = if cursor == Some(i) { sel } else { t.bold() };
        spans.push(Span::styled(display(&s.label), st));
    }
    Line::from(spans)
}

/// Horizontal bars: one row per label and series. The cursor moves by label.
#[allow(clippy::too_many_arguments)]
fn bars(
    labels: &[String],
    series: &[Series],
    cursor: usize,
    top: usize,
    w: usize,
    h: usize,
    t: &Theme,
    sel: Style,
) -> (Vec<Line<'static>>, usize) {
    let mut out = Vec::new();
    if series.len() > 1 {
        out.push(legend(series, None, t, sel));
        out.push(Line::from(""));
    }
    let max = series
        .iter()
        .flat_map(|s| s.values.iter().copied().flatten())
        .fold(0.0_f64, |m, v| m.max(v.abs()));
    let lw = labels
        .iter()
        .map(|l| cells(&display(l)))
        .max()
        .unwrap_or(0)
        .min(w / 3)
        .max(1);
    let vw = series
        .iter()
        .flat_map(|s| s.values.iter())
        .map(|v| v.map_or(1, |x| number(x).len()))
        .max()
        .unwrap_or(1);
    let bw = w.saturating_sub(lw + vw + 5).max(1);
    // Each label takes one row per series; keep the cursor's label on screen.
    let per = series.len().max(1);
    let fit = (h.saturating_sub(out.len()) / per).max(1);
    let top = if cursor < top {
        cursor
    } else if cursor >= top + fit {
        cursor + 1 - fit
    } else {
        top
    };
    for (i, label) in labels.iter().enumerate().skip(top).take(fit) {
        let cur = i == cursor;
        for (k, s) in series.iter().enumerate() {
            let v = s.values.get(i).copied().flatten();
            let name = if k == 0 {
                let d = display(label);
                let d = if cells(&d) > lw {
                    format!("{}…", slice_cells(&d, 0, lw.saturating_sub(1)))
                } else {
                    d
                };
                format!("{d}{}", " ".repeat(lw.saturating_sub(cells(&d))))
            } else {
                " ".repeat(lw)
            };
            let lead = if cur && k == 0 { "▸ " } else { "  " };
            let name_st = if cur { sel } else { t.muted() };
            let bar = match v {
                Some(x) if max > 0.0 && x > 0.0 => block_bar(x / max * bw as f64),
                _ => String::new(),
            };
            let value = v.map_or("-".to_owned(), number);
            out.push(Line::from(vec![
                Span::styled(lead, name_st),
                Span::styled(name, name_st),
                Span::raw(" "),
                Span::styled(bar.clone(), t.fg(SERIES[k % 8])),
                Span::raw(" ".repeat(bw.saturating_sub(cells(&bar)) + 1)),
                Span::styled(value, if cur { t.bold() } else { Style::default() }),
            ]));
        }
    }
    if labels.len() > top + fit {
        out.push(Line::from(Span::styled(
            format!("  {} more below", labels.len() - top - fit),
            t.muted(),
        )));
    }
    (out, top)
}

/// A bar `len` cells long, in eighth blocks (`#` in ASCII mode).
fn block_bar(len: f64) -> String {
    if theme::ascii() {
        return "#".repeat(len.round().max(1.0) as usize);
    }
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (len * 8.0).round().max(1.0) as usize;
    format!("{}{}", "█".repeat(eighths / 8), PARTS[eighths % 8])
}

/// Lines in braille dots: two dots across and four down per cell. The selected series is
/// drawn last, so it stays on top.
fn plot(
    labels: &[String],
    series: &[Series],
    cursor: usize,
    w: usize,
    h: usize,
    t: &Theme,
    sel: Style,
) -> Vec<Line<'static>> {
    let mut out = vec![legend(series, Some(cursor), t, sel)];
    let values: Vec<f64> = series
        .iter()
        .flat_map(|s| s.values.iter().copied().flatten())
        .collect();
    if values.is_empty() {
        out.push(Line::from(Span::styled("(no values)", t.muted())));
        return out;
    }
    let mut lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let mut hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (hi - lo).abs() < f64::EPSILON {
        lo -= 1.0;
        hi += 1.0;
    }
    let (top_label, low_label) = (number(hi), number(lo));
    let yw = cells(&top_label).max(cells(&low_label));
    let ph = h.saturating_sub(3).max(2);
    let pw = w.saturating_sub(yw + 2).max(4);
    let (dw, dh) = (pw * 2, ph * 4);
    // Per cell: braille bits and the series that drew last.
    let mut grid: Vec<(u8, Option<usize>)> = vec![(0, None); pw * ph];
    let n = labels.len();
    let x_of = |i: usize| {
        if n <= 1 {
            0
        } else {
            ((i as f64) * (dw - 1) as f64 / (n - 1) as f64).round() as i64
        }
    };
    let y_of = |v: f64| {
        let f = (v - lo) / (hi - lo);
        (dh as i64 - 1) - (f * (dh - 1) as f64).round() as i64
    };
    let mut order: Vec<usize> = (0..series.len()).filter(|&k| k != cursor).collect();
    if cursor < series.len() {
        order.push(cursor);
    }
    for k in order {
        let mut prev: Option<(i64, i64)> = None;
        for (i, v) in series[k].values.iter().enumerate() {
            let Some(v) = v else {
                prev = None;
                continue;
            };
            let pt = (x_of(i), y_of(*v));
            match prev {
                Some(a) => line_dots(a, pt, |x, y| set_dot(&mut grid, pw, ph, x, y, k)),
                None => set_dot(&mut grid, pw, ph, pt.0, pt.1, k),
            }
            prev = Some(pt);
        }
    }
    for row in 0..ph {
        let axis = match row {
            0 => top_label.clone(),
            r if r + 1 == ph => low_label.clone(),
            _ => String::new(),
        };
        let mut spans = vec![
            Span::styled(format!("{axis:>yw$} "), t.muted()),
            Span::styled(if theme::ascii() { "|" } else { "│" }, t.dim()),
        ];
        for col in 0..pw {
            let (bits, who) = grid[row * pw + col];
            match who {
                Some(k) if bits != 0 => {
                    let ch = if theme::ascii() {
                        "*".to_owned()
                    } else {
                        char::from_u32(0x2800 + u32::from(bits))
                            .unwrap_or(' ')
                            .to_string()
                    };
                    let mut st = t.fg(SERIES[k % 8]);
                    if k == cursor {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    spans.push(Span::styled(ch, st));
                }
                _ => spans.push(Span::raw(" ")),
            }
        }
        out.push(Line::from(spans));
    }
    // The x-axis: the first and last labels, and the middle one when it fits.
    let pad = " ".repeat(yw + 2);
    // Each axis label gets at most a third of the plot width.
    let clip = |l: &String| slice_cells(&display(l), 0, (pw / 3).max(1));
    let first = labels.first().map(clip).unwrap_or_default();
    let last = labels.last().map(clip).unwrap_or_default();
    let mid = labels.get(n / 2).map(clip).unwrap_or_default();
    let mut axis = first.clone();
    let mid_at = pw / 2 - cells(&mid).min(pw / 2) / 2;
    if n > 2
        && mid_at > cells(&axis) + 1
        && mid_at + cells(&mid) + 1 < pw.saturating_sub(cells(&last))
    {
        axis.push_str(&" ".repeat(mid_at - cells(&axis)));
        axis.push_str(&mid);
    }
    if n > 1 && cells(&axis) + 1 + cells(&last) <= pw {
        axis.push_str(&" ".repeat(pw - cells(&axis) - cells(&last)));
        axis.push_str(&last);
    }
    out.push(Line::from(Span::styled(format!("{pad}{axis}"), t.muted())));
    out
}

fn set_dot(grid: &mut [(u8, Option<usize>)], pw: usize, ph: usize, x: i64, y: i64, k: usize) {
    if x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as usize, y as usize);
    let (cx, cy) = (x / 2, y / 4);
    if cx >= pw || cy >= ph {
        return;
    }
    const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    let c = &mut grid[cy * pw + cx];
    c.0 |= BITS[x % 2][y % 4];
    c.1 = Some(k);
}

/// The dots of a straight line from `a` to `b` (Bresenham).
fn line_dots(a: (i64, i64), b: (i64, i64), mut dot: impl FnMut(i64, i64)) {
    let (mut x, mut y) = a;
    let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
    let (sx, sy) = (if x < b.0 { 1 } else { -1 }, if y < b.1 { 1 } else { -1 });
    let mut err = dx + dy;
    loop {
        dot(x, y);
        if (x, y) == b {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// A stat's tone as a color and a mark.
fn tone(x: StatTone) -> (Option<Tone>, &'static str) {
    let ascii = theme::ascii();
    match x {
        StatTone::Neutral => (None, ""),
        StatTone::Good => (Some(Tone::Leaf), if ascii { "v " } else { "✓ " }),
        StatTone::Warn => (Some(Tone::Amber), "! "),
        StatTone::Bad => (Some(Tone::Rose), if ascii { "x " } else { "✗ " }),
    }
}

/// Stats as tiles in rows: label, value, and detail, with a blank row between tile rows.
fn tiles(
    stats: &[Stat],
    cursor: usize,
    w: usize,
    h: usize,
    t: &Theme,
    sel: Style,
) -> Vec<Line<'static>> {
    let cols = (w / 24).clamp(1, stats.len().max(1));
    let tw = w / cols;
    let rows = stats.len().div_ceil(cols);
    let fit = (h + 1) / 4;
    let first = (cursor / cols).saturating_sub(fit.saturating_sub(1));
    let cut = |s: String, n: usize| {
        if cells(&s) > n {
            format!("{}…", slice_cells(&s, 0, n.saturating_sub(1)))
        } else {
            let pad = n - cells(&s);
            format!("{s}{}", " ".repeat(pad))
        }
    };
    let mut out = Vec::new();
    for r in first..rows {
        let row = &stats[r * cols..((r + 1) * cols).min(stats.len())];
        let mut label = Vec::new();
        let mut value = Vec::new();
        let mut detail = Vec::new();
        for (j, x) in row.iter().enumerate() {
            let cur = r * cols + j == cursor;
            let (color, glyph) = tone(x.tone);
            let inner = tw.saturating_sub(3);
            label.push(Span::styled(
                if cur { "▸ " } else { "  " },
                if cur { sel } else { t.muted() },
            ));
            label.push(Span::styled(
                cut(display(&x.label), inner),
                if cur { sel } else { t.muted() },
            ));
            label.push(Span::raw(" "));
            let vs = color.map_or(t.bold(), |c| t.word(c).add_modifier(Modifier::BOLD));
            value.push(Span::raw("  "));
            value.push(Span::styled(
                cut(format!("{glyph}{}", display(&x.value)), inner),
                vs,
            ));
            value.push(Span::raw(" "));
            detail.push(Span::raw("  "));
            detail.push(Span::styled(
                cut(display(x.detail.as_deref().unwrap_or("")), inner),
                color.map_or(t.muted(), |c| t.word(c)),
            ));
            detail.push(Span::raw(" "));
        }
        out.push(Line::from(label));
        out.push(Line::from(value));
        out.push(Line::from(detail));
        if out.len() + 1 > h {
            break;
        }
        out.push(Line::from(""));
    }
    out
}
