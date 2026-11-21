//! Image viewer: one document, vertically tiled, scroll + reload.

use crate::caps::Caps;
use crate::screen::{Frame, Screen};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use docmodel::{Attrs, Cell, Line, Rgb};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Tile {
    id: u32,
    png: Arc<Vec<u8>>,
    cols: u16,
    rows: u16,
    sent: bool,
}

pub struct Viewer {
    file: PathBuf,
    src: image::DynamicImage,
    tiles: Vec<Tile>,
    rows_per_tile: u16,
    img_rows: u16,
    img_cols: u16,
    src_dim: (u32, u32),
    scroll: usize,
    status: String,
    out: Vec<u8>,
}

pub fn run(file: &Path, caps: Caps) -> Result<()> {
    let src = render_img::load(file)?;
    let mut v = Viewer {
        file: file.to_path_buf(),
        src,
        tiles: Vec::new(),
        rows_per_tile: 1,
        img_rows: 0,
        img_cols: 0,
        src_dim: (0, 0),
        scroll: 0,
        status: String::new(),
        out: Vec::new(),
    };
    let mut screen = Screen::new(caps.clone(), caps.terminal_theme.clone());
    v.rebuild(&mut screen, false);
    let result = v.event_loop(&mut screen);
    let mut bye = Vec::new();
    kgp::delete_all(&mut bye);
    {
        use std::io::Write;
        let mut o = std::io::stdout();
        let _ = o.write_all(&bye);
        let _ = o.flush();
    }
    crate::screen::leave();
    result
}

impl Viewer {
    fn body_rows(&self, screen: &Screen) -> usize {
        screen.body_rows()
    }

    fn max_scroll(&self, screen: &Screen) -> usize {
        (1 + self.img_rows as usize).saturating_sub(self.body_rows(screen))
    }

    fn scroll_by(&mut self, screen: &Screen, delta: i32) {
        let max = self.max_scroll(screen);
        self.scroll = if delta >= 0 {
            (self.scroll + delta as usize).min(max)
        } else {
            self.scroll.saturating_sub(delta.unsigned_abs() as usize)
        };
    }

    fn rebuild(&mut self, screen: &mut Screen, delete_first: bool) {
        screen.resize();
        if delete_first {
            kgp::delete_all(&mut self.out);
        }
        let (cw, ch) = (
            screen.caps.cell.0.max(1) as u32,
            screen.caps.cell.1.max(1) as u32,
        );
        match render_img::layout(&self.src, (cw, ch), screen.caps.cols) {
            Ok(lay) => {
                self.tiles = lay
                    .tiles
                    .into_iter()
                    .map(|t| Tile {
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
                self.scroll = self.scroll.min(self.max_scroll(screen));
                screen.invalidate();
            }
            Err(e) => self.status = format!("layout failed: {e}"),
        }
    }

    fn reload(&mut self, screen: &mut Screen) {
        match render_img::load(&self.file) {
            Ok(img) => {
                self.src = img;
                self.status.clear();
                self.rebuild(screen, true);
            }
            Err(e) => self.status = format!("reload failed: {e}"),
        }
    }

    fn render_row(&self, screen: &Screen, content: usize) -> String {
        if content == 0 {
            let (w, h) = self.src_dim;
            let name = self
                .file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut s = String::from("\x1b[0m");
            let line = Line {
                cells: vec![
                    Cell::new(
                        format!(" {name}"),
                        Attrs {
                            bold: true,
                            ..Default::default()
                        },
                    ),
                    Cell::new(
                        format!(
                            "  {w}x{h}px · {}x{} cells · {} tile(s)",
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
            };
            s.push_str(&line.render(screen.caps.cols as usize));
            return s;
        }
        let ir = content - 1;
        if ir < self.img_rows as usize && !self.tiles.is_empty() {
            let t = (ir / self.rows_per_tile as usize).min(self.tiles.len() - 1);
            let row_in = (ir % self.rows_per_tile as usize) as u16;
            kgp::placeholder_row(self.tiles[t].id, row_in, self.tiles[t].cols)
        } else {
            String::new()
        }
    }

    fn paint(&mut self, screen: &mut Screen) -> Result<()> {
        let body = screen.body_rows();
        let mut rows = Vec::with_capacity(body);
        for s in 0..body {
            rows.push(self.render_row(screen, s + self.scroll));
        }
        for t in &mut self.tiles {
            if !t.sent {
                kgp::transmit(&mut self.out, t.id, &t.png, t.cols, t.rows);
                t.sent = true;
            }
        }
        let left = if self.status.is_empty() {
            self.file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            self.status.clone()
        };
        let right = format!(
            "{}/{}",
            self.scroll.min(self.max_scroll(screen)),
            self.max_scroll(screen)
        );
        screen.paint(
            &Frame {
                rows,
                status_left: left,
                status_right: right,
            },
            &mut self.out,
        )
    }

    fn event_loop(&mut self, screen: &mut Screen) -> Result<()> {
        self.paint(screen)?;
        loop {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let page = screen.body_rows() as i32;
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        KeyCode::Char('j') | KeyCode::Down => self.scroll_by(screen, 1),
                        KeyCode::Char('k') | KeyCode::Up => self.scroll_by(screen, -1),
                        KeyCode::Char('d') => self.scroll_by(screen, page / 2),
                        KeyCode::Char('u') => self.scroll_by(screen, -(page / 2)),
                        KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_by(screen, page - 2),
                        KeyCode::Char('b') | KeyCode::PageUp => self.scroll_by(screen, -(page - 2)),
                        KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
                        KeyCode::Char('G') | KeyCode::End => self.scroll = self.max_scroll(screen),
                        KeyCode::Char('r') => {
                            self.reload(screen);
                            continue;
                        }
                        _ => continue,
                    }
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollDown => self.scroll_by(screen, 3),
                    MouseEventKind::ScrollUp => self.scroll_by(screen, -3),
                    _ => continue,
                },
                Event::Resize(..) => {
                    self.rebuild(screen, true);
                    continue;
                }
                _ => continue,
            }
            self.paint(screen)?;
        }
    }
}
