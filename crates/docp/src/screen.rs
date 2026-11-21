//! Shared painter: alternate screen lifecycle, row diffing, synchronized
//! updates, and the status bar. Views produce `Frame`s; this module owns all
//! terminal output so there is exactly one writer.

use crate::caps::{self, Caps};
use anyhow::Result;
use docmodel::{Attrs, Cell, Line, Rgb, Theme};
use std::io::{stdout, Write};
use unicode_width::UnicodeWidthStr;

/// What a view wants drawn this frame.
pub struct Frame {
    /// Screen rows, top to bottom, already containing placeholder rows.
    pub rows: Vec<String>,
    /// Status bar text (left), right-aligned suffix.
    pub status_left: String,
    pub status_right: String,
}

pub struct Screen {
    pub caps: Caps,
    /// Theme used for chrome (status bar) so it belongs to the page.
    pub theme: Theme,
    prev: Vec<String>,
    prev_status: String,
}

/// Put the terminal into screen mode: alt screen, hidden cursor, SGR mouse,
/// bracketed paste off (we do not paste).
pub fn enter() -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut o = stdout();
    o.write_all(b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[2J\x1b[H")?;
    o.flush()?;
    Ok(())
}

pub fn leave() {
    let mut buf = Vec::new();
    kgp::delete_all(&mut buf);
    let mut o = stdout();
    let _ = o.write_all(&buf);
    let _ = o.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l");
    let _ = o.flush();
    let _ = crossterm::terminal::disable_raw_mode();
}

impl Screen {
    pub fn new(caps: Caps, theme: Theme) -> Screen {
        Screen {
            caps,
            theme,
            prev: Vec::new(),
            prev_status: String::new(),
        }
    }

    pub fn body_rows(&self) -> usize {
        (self.caps.rows as usize).saturating_sub(1).max(1)
    }

    pub fn resize(&mut self) {
        caps::refresh_size(&mut self.caps);
        self.prev.clear();
        self.prev_status.clear();
    }

    /// Force the next paint to rewrite every row.
    pub fn invalidate(&mut self) {
        self.prev.clear();
        self.prev_status.clear();
    }

    /// Draw `frame`, plus any pending escape sequences the view queued
    /// (image transmissions, placements, deletes).
    pub fn paint(&mut self, frame: &Frame, pending: &mut Vec<u8>) -> Result<()> {
        let cols = self.caps.cols as usize;
        let mut buf = std::mem::take(pending);
        buf.extend_from_slice(b"\x1b[?2026h");

        let mut cur: Vec<String> = Vec::with_capacity(frame.rows.len());
        for (i, row) in frame.rows.iter().enumerate() {
            let mut s = row.clone();
            s.push_str("\x1b[0m\x1b[K");
            if self.prev.get(i) != Some(&s) {
                buf.extend_from_slice(format!("\x1b[{};1H", i + 1).as_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            cur.push(s);
        }
        // Clear rows that are no longer used (window grew).
        for i in frame.rows.len()..self.prev.len() {
            buf.extend_from_slice(format!("\x1b[{};1H\x1b[K", i + 1).as_bytes());
        }

        let status = status_bar(cols, &frame.status_left, &frame.status_right, &self.theme);
        if status != self.prev_status {
            buf.extend_from_slice(format!("\x1b[{};1H", self.caps.rows).as_bytes());
            buf.extend_from_slice(status.as_bytes());
            buf.extend_from_slice(b"\x1b[0m");
            self.prev_status = status;
        }

        buf.extend_from_slice(b"\x1b[?2026l");
        let mut out = stdout().lock();
        out.write_all(&buf)?;
        out.flush()?;
        buf.clear();
        *pending = buf;
        self.prev = cur;
        Ok(())
    }
}

/// Inverse status bar: left text, right text, middle filled.
/// Stops one column short of the edge: writing the last column puts the
/// terminal in a pending-wrap state, which some terminals resolve by
/// scrolling when the next frame starts.
pub fn status_bar(cols: usize, left: &str, right: &str, theme: &Theme) -> String {
    let cols = cols.saturating_sub(1).max(1);
    // A surface tinted from the page colors, never a hardcoded gray.
    let bar_bg = Rgb::mix(theme.bg, theme.fg, 0.10);
    let bar_fg = theme.muted();
    let lw = left.width();
    let rw = right.width();
    let pad = cols.saturating_sub(lw + rw + 1).max(1);
    let line = Line {
        cells: vec![
            Cell::new(
                format!(" {left}"),
                Attrs {
                    fg: Some(bar_fg),
                    bg: Some(bar_bg),
                    bold: true,
                    ..Default::default()
                },
            ),
            Cell::new(
                " ".repeat(pad),
                Attrs {
                    fg: Some(bar_fg),
                    bg: Some(bar_bg),
                    ..Default::default()
                },
            ),
            Cell::new(
                right.to_string(),
                Attrs {
                    fg: Some(bar_fg),
                    bg: Some(bar_bg),
                    ..Default::default()
                },
            ),
            Cell::new(
                " ",
                Attrs {
                    fg: Some(bar_fg),
                    bg: Some(bar_bg),
                    ..Default::default()
                },
            ),
        ],
        fill: Some(bar_bg),
    };
    let mut s = String::new();
    line.encode_into(&mut s, cols);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bar_fills_width() {
        let t = Theme::dark_default();
        let s = status_bar(40, "doc.md", "42%", &t);
        assert!(s.contains("doc.md"));
        assert!(s.contains("42%"));
        assert!(s.contains("\x1b[0;48;2;"), "{s}");
    }

    #[test]
    fn status_bar_survives_narrow() {
        let t = Theme::dark_default();
        let s = status_bar(5, "averylongname", "99%", &t);
        assert!(!s.is_empty());
    }
}
