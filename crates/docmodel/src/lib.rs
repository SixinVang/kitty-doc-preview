//! Shared display model: every document format is reduced to `Row`s of
//! styled text or image placeholder references before painting, plus the
//! color theme both the text and pixel paths derive from.

/// One row of content, independent of its screen position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Text(Line),
    /// Placeholder row `row` (0-based, within the image) of media `id`,
    /// `cols` cells wide.
    Image {
        id: u32,
        row: u16,
        cols: u16,
    },
}

pub mod theme;

pub use theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// `t` = 0 gives `self`, 1 gives `other`.
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let l = |a: u8, b: u8| {
            (a as f32 + (b as f32 - a as f32) * t)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Rgb(l(self.0, other.0), l(self.1, other.1), l(self.2, other.2))
    }

    /// Parses `#rgb`, `#rrggbb` and xterm's `rgb:rrrr/gggg/bbbb`.
    pub fn parse(s: &str) -> Option<Rgb> {
        let s = s.trim();
        if let Some(h) = s.strip_prefix('#') {
            let v = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
            return match h.len() {
                3 => Some(Rgb(v(0, 1)? * 17, v(1, 1)? * 17, v(2, 1)? * 17)),
                6 => Some(Rgb(v(0, 2)?, v(2, 2)?, v(4, 2)?)),
                _ => None,
            };
        }
        let body = s.strip_prefix("rgb:")?;
        let mut parts = body.split('/');
        let mut chan = || -> Option<u8> {
            let p = parts.next()?;
            let v = u32::from_str_radix(p, 16).ok()?;
            let max = (1u32 << (4 * p.len() as u32)) - 1;
            Some(((v * 255 + max / 2) / max) as u8)
        };
        Some(Rgb(chan()?, chan()?, chan()?))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Attrs {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub attrs: Attrs,
    /// OSC 8 hyperlink for this cell.
    pub url: Option<String>,
}

impl Cell {
    pub fn new(text: impl Into<String>, attrs: Attrs) -> Self {
        Self {
            text: text.into(),
            attrs,
            url: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// Paint the rest of the row in this color (status bars, code blocks).
    pub fill: Option<Rgb>,
}

impl Line {
    /// Display width in terminal columns (CJK aware). This is what padding and
    /// alignment must use: `图片` is two chars but four columns.
    pub fn width(&self) -> usize {
        self.cells.iter().map(|c| display_width(&c.text)).sum()
    }

    /// `rest` with `prefix` cells prepended (quote bars, list markers).
    pub fn prefixed(prefix: &[Cell], mut rest: Line) -> Line {
        let mut cells = prefix.to_vec();
        cells.append(&mut rest.cells);
        Line {
            cells,
            fill: rest.fill,
        }
    }

    /// Encode into `s`: SGR per cell, OSC 8 around url runs, `fill` pads to
    /// `width`. Does not truncate; layout is responsible for fitting.
    ///
    /// In-document anchors (`#heading`) never reach the terminal as OSC 8:
    /// they only mean something inside the preview, which follows them on
    /// click. They stay in `Cell::url` for that purpose.
    pub fn encode_into(&self, s: &mut String, width: usize) {
        let mut link: Option<&str> = None;
        for c in &self.cells {
            let target = c.url.as_deref().filter(|u| !u.starts_with('#'));
            if target != link {
                s.push_str("\x1b]8;;");
                s.push_str(target.unwrap_or(""));
                s.push_str("\x1b\\");
                link = target;
            }
            sgr(s, &c.attrs);
            s.push_str(&c.text);
        }
        if link.is_some() {
            s.push_str("\x1b]8;;\x1b\\");
        }
        if let Some(bg) = self.fill {
            let pad = width.saturating_sub(self.width());
            sgr(
                s,
                &Attrs {
                    bg: Some(bg),
                    ..Default::default()
                },
            );
            s.push_str(&" ".repeat(pad));
        }
    }

    /// Convenience: truncate to `max_cols` columns and encode.
    pub fn render(&self, max_cols: usize) -> String {
        let mut s = String::new();
        let mut line = Line {
            cells: Vec::new(),
            fill: self.fill,
        };
        let mut used = 0usize;
        'outer: for cell in &self.cells {
            let mut text = String::new();
            for ch in cell.text.chars() {
                let cw = crate::char_cols(ch);
                if used + cw > max_cols {
                    break 'outer;
                }
                text.push(ch);
                used += cw;
            }
            let mut c = cell.clone();
            c.text = text;
            line.cells.push(c);
        }
        line.encode_into(&mut s, max_cols);
        s
    }
}

/// Terminal column width of one char (CJK aware).
pub fn char_cols(ch: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    ch.width().unwrap_or(0)
}

/// Terminal column width of a string.
///
/// Zero-width joiners and the emoji variation selector carry no advance of
/// their own; counting them would shift every cell after them.
pub fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    s.chars()
        .filter(|c| !matches!(*c, '\u{FE0E}' | '\u{FE0F}' | '\u{200D}'))
        .map(|c| c.width().unwrap_or(0))
        .sum()
}

/// Append one SGR sequence for `a` (resets first, so cells are independent).
pub fn sgr(s: &mut String, a: &Attrs) {
    s.push_str("\x1b[0");
    if a.bold {
        s.push_str(";1");
    }
    if a.italic {
        s.push_str(";3");
    }
    if a.underline {
        s.push_str(";4");
    }
    if a.strike {
        s.push_str(";9");
    }
    if let Some(c) = a.fg {
        s.push_str(&format!(";38;2;{};{};{}", c.0, c.1, c.2));
    }
    if let Some(c) = a.bg {
        s.push_str(&format!(";48;2;{};{};{}", c.0, c.1, c.2));
    }
    s.push('m');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_is_display_width_not_char_count() {
        let line = Line {
            cells: vec![Cell::new("图片", Attrs::default())],
            fill: None,
        };
        assert_eq!(line.width(), 4);
        assert_eq!(display_width("✅"), 2);
        assert_eq!(display_width("\u{2705}\u{FE0F}"), 2); // emoji + VS16
        assert_eq!(display_width("a中"), 3);
    }

    #[test]
    fn renders_styles_and_truncates() {
        let line = Line {
            cells: vec![
                Cell::new(
                    "hi",
                    Attrs {
                        bold: true,
                        ..Default::default()
                    },
                ),
                Cell::new(
                    "wo",
                    Attrs {
                        fg: Some(Rgb(1, 2, 3)),
                        ..Default::default()
                    },
                ),
            ],
            fill: None,
        };
        let s = line.render(10);
        assert!(s.contains("\x1b[0;1mhi"));
        assert!(s.contains("\x1b[0;38;2;1;2;3mwo"));

        let t = line.render(3);
        assert!(!t.contains("wo"));
    }

    #[test]
    fn fill_pads_row() {
        let line = Line {
            cells: vec![Cell::new("x", Attrs::default())],
            fill: Some(Rgb(10, 10, 10)),
        };
        let s = line.render(4);
        assert!(s.contains("\x1b[0;48;2;10;10;10m"));
        assert!(!s.trim_end_matches('\x1b').is_empty());
    }

    #[test]
    fn osc8_wraps_url_runs() {
        let line = Line {
            cells: vec![
                Cell {
                    text: "a".into(),
                    attrs: Attrs::default(),
                    url: Some("https://x".into()),
                },
                Cell {
                    text: "b".into(),
                    attrs: Attrs::default(),
                    url: Some("https://x".into()),
                },
                Cell::new("c", Attrs::default()),
            ],
            fill: None,
        };
        let mut s = String::new();
        line.encode_into(&mut s, 10);
        assert_eq!(s.matches("\x1b]8;;https://x\x1b\\").count(), 1);
        // the url run is closed before the non-url cell
        assert!(s.contains("b\x1b]8;;\x1b\\"));
        assert!(s.ends_with('c'));
    }

    #[test]
    fn internal_anchors_are_not_osc8_links() {
        let line = Line {
            cells: vec![Cell {
                text: "jump".into(),
                attrs: Attrs::default(),
                url: Some("#install".into()),
            }],
            fill: None,
        };
        let mut s = String::new();
        line.encode_into(&mut s, 10);
        assert!(!s.contains("\x1b]8;;#"), "{s:?}");
        assert!(s.contains("jump"));
    }
}
