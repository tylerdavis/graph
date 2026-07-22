//! Buffer → SVG: the rendering half of the docs-screenshot pipeline
//! (see [`super::shots`] for the scenario harness that produces the
//! buffers).
//!
//! The styled cell buffer walks straight into an SVG: one background rect
//! per colored run, one text element per non-space segment, on a fixed
//! monospace grid. `textLength` pins every segment to the grid so the
//! viewer's font metrics can't skew alignment — and whitespace never
//! enters a text element, so renderers that collapse it can't stretch
//! glyphs across it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

// ── Grid metrics and palette ─────────────────────────────────────────────

const CELL_W: f64 = 8.4;
const CELL_H: f64 = 18.0;
const FONT_SIZE: f64 = 14.0;
/// Baseline offset from the cell top.
const BASELINE: f64 = 13.5;
const PAD: f64 = 18.0;
/// Window title bar height (traffic lights + title).
const CHROME_H: f64 = 36.0;

const BG: &str = "#171923";
const CHROME_BG: &str = "#11131b";
const DEFAULT_FG: &str = "#c6cad3";

/// ANSI → hex, One-Dark-ish. Only the colors the workbench uses get
/// bespoke values; the rest fall back to the default foreground.
fn color_hex(color: Color) -> Option<&'static str> {
    match color {
        Color::Reset => None,
        Color::Black => Some("#1e222a"),
        Color::Red => Some("#e06c75"),
        Color::Green => Some("#98c379"),
        Color::Yellow => Some("#e5c07b"),
        Color::Blue => Some("#61afef"),
        Color::Magenta => Some("#c678dd"),
        Color::Cyan => Some("#56b6c2"),
        Color::Gray => Some("#abb2bf"),
        Color::DarkGray => Some("#5c6370"),
        Color::White => Some("#dcdfe4"),
        _ => Some(DEFAULT_FG),
    }
}

/// Resolved cell style: fill color, optional background, bold, dim.
#[derive(PartialEq, Clone)]
struct CellStyle {
    fg: &'static str,
    bg: Option<&'static str>,
    bold: bool,
    dim: bool,
}

fn cell_style(fg: Color, bg: Color, modifier: Modifier) -> CellStyle {
    let mut style = CellStyle {
        fg: color_hex(fg).unwrap_or(DEFAULT_FG),
        bg: color_hex(bg),
        bold: modifier.contains(Modifier::BOLD),
        dim: modifier.contains(Modifier::DIM),
    };
    if modifier.contains(Modifier::REVERSED) {
        style = CellStyle {
            fg: style.bg.unwrap_or(BG),
            bg: Some(color_hex(fg).unwrap_or(DEFAULT_FG)),
            ..style
        };
    }
    style
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── Buffer → SVG ─────────────────────────────────────────────────────────

/// Render a buffer (or a clipped pane of it) to SVG. `title` draws the
/// macOS-style window chrome; `None` renders a bare rounded panel —
/// the right dress for pane crops. `clip` selects a cell sub-rect.
pub fn buffer_to_svg(buffer: &Buffer, title: Option<&str>, clip: Option<Rect>) -> String {
    let clip = clip.unwrap_or(buffer.area);
    let cols = clip.width as usize;
    let rows = clip.height as usize;
    let chrome_h = if title.is_some() { CHROME_H } else { 0.0 };
    let grid_w = cols as f64 * CELL_W;
    let grid_h = rows as f64 * CELL_H;
    let width = grid_w + PAD * 2.0;
    let height = grid_h + PAD * 2.0 + chrome_h;

    let mut svg = String::new();
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}" xml:space="preserve" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace" font-size="{FONT_SIZE}px">
"#
    ));
    // Window: chrome bar over the terminal body, one rounded silhouette.
    svg.push_str(&format!(
        r#"<rect width="{width:.0}" height="{height:.0}" rx="10" fill="{CHROME_BG}"/>
<rect y="{chrome_h}" width="{width:.0}" height="{h:.0}" fill="{BG}"/>
<rect y="{h2:.0}" width="{width:.0}" height="20" rx="10" fill="{BG}"/>
"#,
        h = height - chrome_h - 10.0,
        h2 = height - 20.0,
    ));
    if let Some(title) = title {
        for (i, color) in ["#ff5f57", "#febc2e", "#28c840"].iter().enumerate() {
            svg.push_str(&format!(
                r#"<circle cx="{cx}" cy="18" r="6" fill="{color}"/>
"#,
                cx = 22 + i * 20,
            ));
        }
        svg.push_str(&format!(
            r##"<text x="{x:.0}" y="23" fill="#8b90a0" text-anchor="middle">{title}</text>
"##,
            x = width / 2.0,
            title = escape(title),
        ));
    }

    // One pass per row: coalesce runs of identically-styled cells, first
    // the background rects, then the text (so text never sits under a
    // later rect).
    let content = buffer.content();
    let buffer_cols = buffer.area.width as usize;
    let mut rects = String::new();
    let mut texts = String::new();
    for row in 0..rows {
        let cells: Vec<_> = (0..cols)
            .map(|col| {
                let index = (clip.y as usize + row) * buffer_cols + clip.x as usize + col;
                let cell = &content[index];
                (
                    cell.symbol().to_string(),
                    cell_style(cell.fg, cell.bg, cell.modifier),
                )
            })
            .collect();
        let top = chrome_h + PAD + row as f64 * CELL_H;
        let mut col = 0;
        while col < cols {
            let style = cells[col].1.clone();
            let start = col;
            while col < cols && cells[col].1 == style {
                col += 1;
            }
            if let Some(bg) = style.bg {
                rects.push_str(&format!(
                    r#"<rect x="{x:.1}" y="{top:.1}" width="{w:.1}" height="{CELL_H}" fill="{bg}"/>
"#,
                    x = PAD + start as f64 * CELL_W,
                    w = (col - start) as f64 * CELL_W,
                ));
            }
            let mut attrs = format!(r#"fill="{}""#, style.fg);
            if style.bold {
                attrs.push_str(r#" font-weight="600""#);
            }
            if style.dim {
                attrs.push_str(r#" opacity="0.55""#);
            }
            // Text per non-space segment: whitespace never enters a text
            // element, so renderers that collapse it can't skew
            // `textLength`-pinned glyph placement.
            let mut seg = start;
            while seg < col {
                if cells[seg].0.trim().is_empty() {
                    seg += 1;
                    continue;
                }
                let seg_start = seg;
                let mut text = String::new();
                while seg < col && !cells[seg].0.trim().is_empty() {
                    text.push_str(&cells[seg].0);
                    seg += 1;
                }
                texts.push_str(&format!(
                    r#"<text x="{x:.1}" y="{y:.1}" textLength="{len:.1}" lengthAdjust="spacingAndGlyphs" {attrs}>{text}</text>
"#,
                    x = PAD + seg_start as f64 * CELL_W,
                    y = top + BASELINE,
                    len = (seg - seg_start) as f64 * CELL_W,
                    text = escape(&text),
                ));
            }
        }
    }
    svg.push_str(&rects);
    svg.push_str(&texts);
    svg.push_str("</svg>\n");
    svg
}
