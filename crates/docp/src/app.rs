//! TUI application: event loop, row diffing painter, image lifecycle.

use crate::caps::{self, Caps};
use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use docmodel::{Attrs, Cell as DCell, Line, Rgb};
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct TileLive {
    id: u32,
    png: Arc<Vec<u8>>,
    cols: u16,
    rows: u16,
    sent: bool,
}

struct App {
    caps: Caps,
    file: PathBuf,
    src: image::DynamicImage,
    tiles: Vec<TileLive>,
    rows_per_tile: u16,
    img_rows: u16,
    img_cols: u16,
    src_dim: (u32, u32),
    scroll: usize,
    status: String,
    prev: Vec<String>,
    prev_footer: String,
}

pub fn run(file: &Path, caps: Caps) -> Result<()> {
    let src = render_img::load(file)?;
    let mut app = App {
        caps,
        file: file.to_path_buf(),
        src,
        tiles: Vec::new(),
        rows_per_tile: 1,
        img_rows: 0,
        img_cols: 0,
        src_dim: (0, 0),
        scroll: 0,
        status: String::new(),
        prev: Vec::new(),
        prev_footer: String::new(),
    };
    app.rebuild(false);

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, Hide, EnableMouseCapture)?;

    let mut running = true;
    let mut force_paint = true;
    while running {
        if force_paint || event::poll(std::time::Duration::from_millis(200))? {
            if !force_paint {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        app.on_key(k.code, k.modifiers, &mut running);
                    }
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollDown => app.scroll_by(3),
                        MouseEventKind::ScrollUp => app.scroll_by(-3),
                        _ => {}
                    },
                    Event::Resize(..) => app.rebuild(true),
                    _ => {}
                }
            }
            force_paint = false;
            app.paint(&mut out)?;
            out.flush()?;
        }
    }

    let mut bye = Vec::new();
    kgp::delete_all(&mut bye);
    out.write_all(&bye)?;
    execute!(out, LeaveAlternateScreen, Show, DisableMouseCapture)?;
    disable_raw_mode()?;
    Ok(())
}

impl App {
    fn body_rows(&self) -> usize {
        (self.caps.rows as usize).saturating_sub(1).max(1)
    }

    fn content_rows(&self) -> usize {
        1 + self.img_rows as usize // title + image
    }

    fn max_scroll(&self) -> usize {
        self.content_rows().saturating_sub(self.body_rows())
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn scroll_by(&mut self, delta: i32) {
        if delta >= 0 {
            self.scroll = (self.scroll + delta as usize).min(self.max_scroll());
        } else {
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs() as usize);
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers, running: &mut bool) {
        match code {
            KeyCode::Char('q') => *running = false,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => *running = false,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(2),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-2),
            KeyCode::Char('d') | KeyCode::PageDown => self.scroll_by(self.body_rows() as i32 / 2),
            KeyCode::Char('u') | KeyCode::PageUp => self.scroll_by(-(self.body_rows() as i32 / 2)),
            KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('G') => self.scroll = self.max_scroll(),
            KeyCode::Char('r') => self.reload(),
            _ => {}
        }
    }

    fn reload(&mut self) {
        match render_img::load(&self.file) {
            Ok(img) => {
                self.src = img;
                self.status.clear();
                self.rebuild(true);
            }
            Err(e) => self.status = format!("reload failed: {e}"),
        }
    }

    /// Recompute layout for the current viewport. When `delete_first` is
    /// set, previously transmitted images are freed first (resize/reload).
    fn rebuild(&mut self, delete_first: bool) {
        caps::refresh_size(&mut self.caps);
        let (cw, ch) = (
            self.caps.cell.0.max(1) as u32,
            self.caps.cell.1.max(1) as u32,
        );
        match render_img::layout(&self.src, (cw, ch), self.caps.cols) {
            Ok(lay) => {
                let mut bye = Vec::new();
                if delete_first {
                    kgp::delete_all(&mut bye);
                }
                self.tiles = lay
                    .tiles
                    .into_iter()
                    .map(|t| TileLive {
                        id: kgp::alloc_id(),
                        png: Arc::new(t.png),
                        cols: t.cols,
                        rows: t.rows,
                        sent: false,
                    })
                    .collect();
                self.rows_per_tile = lay.rows_per_tile;
                self.img_rows = lay.rows;
                self.img_cols = lay.cols;
                self.src_dim = (lay.src_w, lay.src_h);
                self.prev = vec![String::new(); self.body_rows()];
                self.prev_footer.clear();
                self.clamp_scroll();
                if !bye.is_empty() {
                    let mut o = stdout();
                    let _ = o.write_all(&bye);
                    let _ = o.flush();
                }
            }
            Err(e) => self.status = format!("layout failed: {e}"),
        }
    }

    fn row_content(&self, c: usize) -> String {
        if c == 0 {
            let (w, h) = self.src_dim;
            return Line {
                cells: vec![
                    DCell::new(
                        format!(" {}", self.file.display()),
                        Attrs {
                            bold: true,
                            ..Default::default()
                        },
                    ),
                    DCell::new(
                        format!(
                            "  {w}x{h}px -> {}x{} cells, {} tile(s)",
                            self.img_cols,
                            self.img_rows,
                            self.tiles.len()
                        ),
                        Attrs {
                            fg: Some(Rgb(130, 130, 130)),
                            ..Default::default()
                        },
                    ),
                ],
                fill: None,
            }
            .render(self.caps.cols as usize);
        }
        let ir = c - 1;
        if ir < self.img_rows as usize && !self.tiles.is_empty() {
            let t = (ir / self.rows_per_tile as usize).min(self.tiles.len() - 1);
            let row_in = (ir % self.rows_per_tile as usize) as u16;
            kgp::placeholder_row(self.tiles[t].id, row_in, self.tiles[t].cols)
        } else {
            String::new()
        }
    }

    fn footer(&self) -> String {
        let mut cells = vec![DCell::new(
            format!(
                " {}/{} ",
                self.scroll.min(self.max_scroll()),
                self.max_scroll()
            ),
            Attrs {
                fg: Some(Rgb(40, 40, 40)),
                bg: Some(Rgb(180, 180, 180)),
                bold: true,
                ..Default::default()
            },
        )];
        let info = if self.status.is_empty() {
            format!(
                " cell {}x{} | {} | q quit j/k scroll g/G d/u r reload ",
                self.caps.cell.0,
                self.caps.cell.1,
                if self.caps.file_transfer {
                    "t=f"
                } else {
                    "t=d"
                }
            )
        } else {
            format!(" {} ", self.status)
        };
        cells.push(DCell::new(
            info,
            Attrs {
                fg: Some(Rgb(60, 60, 60)),
                bg: Some(Rgb(180, 180, 180)),
                ..Default::default()
            },
        ));
        Line {
            cells,
            fill: Some(Rgb(180, 180, 180)),
        }
        .render(self.caps.cols as usize)
    }

    /// Paint one frame: transmit unsent tiles, rewrite only rows that
    /// changed, all inside a synchronized update.
    fn paint(&mut self, out: &mut impl Write) -> Result<()> {
        use std::io::Write as _;
        let body = self.body_rows();
        let mut cur: Vec<String> = Vec::with_capacity(body);
        for s in 0..body {
            cur.push(self.row_content(s + self.scroll));
        }
        let footer = self.footer();

        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        buf.extend_from_slice(b"\x1b[?2026h"); // begin synchronized update
        for t in &mut self.tiles {
            if !t.sent {
                kgp::transmit(&mut buf, t.id, &t.png, t.cols, t.rows);
                t.sent = true;
            }
        }
        for (i, s) in cur.iter().enumerate() {
            if self.prev.get(i) != Some(s) {
                let _ = write!(buf, "\x1b[{};1H", i + 1);
                buf.extend_from_slice(s.as_bytes());
                buf.extend_from_slice(b"\x1b[0m");
            }
        }
        if footer != self.prev_footer {
            let _ = write!(buf, "\x1b[{};1H", self.caps.rows);
            buf.extend_from_slice(footer.as_bytes());
            buf.extend_from_slice(b"\x1b[0m");
        }
        buf.extend_from_slice(b"\x1b[?2026l"); // end synchronized update

        out.write_all(&buf)?;
        self.prev = cur;
        self.prev_footer = footer;
        Ok(())
    }
}
