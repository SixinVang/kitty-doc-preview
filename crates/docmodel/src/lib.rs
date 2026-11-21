//! Shared display model: every document format is reduced to `Row`s of
//! styled text or image placeholder references before painting.

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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Attrs {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub attrs: Attrs,
}

impl Cell {
    pub fn new(text: impl Into<String>, attrs: Attrs) -> Self {
        Self {
            text: text.into(),
            attrs,
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
    pub fn width(&self) -> usize {
        self.cells.iter().map(|c| c.text.chars().count()).sum()
    }

    /// Render to an SGR string (no cursor addressing), truncated to
    /// `max_cols` columns; `fill` pads the remainder of the row.
    pub fn render(&self, max_cols: usize) -> String {
        let mut out = String::new();
        let mut used = 0usize;
        for cell in &self.cells {
            if used >= max_cols {
                break;
            }
            let mut sgr = String::new();
            if cell.attrs.bold {
                sgr.push_str("\x1b[1m");
            }
            if cell.attrs.italic {
                sgr.push_str("\x1b[3m");
            }
            if cell.attrs.underline {
                sgr.push_str("\x1b[4m");
            }
            if let Some(Rgb(r, g, b)) = cell.attrs.fg {
                sgr.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
            }
            if let Some(Rgb(r, g, b)) = cell.attrs.bg {
                sgr.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
            }
            if !sgr.is_empty() {
                out.push_str(&sgr);
            }
            for ch in cell.text.chars() {
                if used >= max_cols {
                    break;
                }
                out.push(ch);
                used += 1;
            }
            if !sgr.is_empty() {
                out.push_str("\x1b[0m");
            }
        }
        if let Some(Rgb(r, g, b)) = self.fill {
            if used < max_cols {
                out.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
                for _ in used..max_cols {
                    out.push(' ');
                }
                out.push_str("\x1b[0m");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(s.contains("\x1b[1mhi\x1b[0m"));
        assert!(s.contains("\x1b[38;2;1;2;3mwo\x1b[0m"));
        assert_eq!(line.width(), 4);

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
        assert!(s.contains("\x1b[48;2;10;10;10m"));
        assert!(s.ends_with("\x1b[0m"));
    }
}
