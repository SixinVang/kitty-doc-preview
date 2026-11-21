//! PDF view: paged layout at fit-width, viewport tile rasterisation, page
//! navigation, zoom and live reload.
//!
//! Layout is pure geometry (page sizes in points + zoom); pixels arrive later
//! as tiles from a dedicated render thread that owns the non-`Sync` MuPDF
//! document. Only the visible tiles (plus a small prefetch) are rasterised,
//! and the cache is bounded by both tile count and decoded pixel area. The
//! cell size is re-synced after resizes: a tile's pixel aspect must match its
//! c*r placement or the terminal letterboxes it (visible as background bands).

use crate::caps::Caps;
use crate::input::Input;
use crate::screen::{Frame, Screen};
use anyhow::{bail, Context, Result};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use docmodel::{Attrs, Theme};
use notify::Watcher;
use render_pdf::{Meta, Pdf};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(15);
const SETTLE: Duration = Duration::from_millis(100);
/// Scroll animation: one cell-row per frame is the smallest step the terminal
/// grid can show, so this only softens small scrolls (keys, wheel), never
/// page turns. Rows that are not already cached snap instead.
const GLIDE_FRAME: Duration = Duration::from_millis(12);
const GLIDE_MS_PER_ROW: u64 = 12;
const GLIDE_MAX_ROWS: usize = 24;

/// Rows per raster tile. Big enough to amortise PNG encoding and protocol
/// overhead, small enough that a scroll crossing a tile boundary renders one
/// screenful, not a whole zoomed page.
const TILE_ROWS: usize = 64;
/// Blank rows between pages.
const GAP: usize = 1;
/// Minimum columns reserved for the page number.
const MIN_GUTTER: usize = 4;
/// Keep one column free on the right so the page is not flush with the edge.
const RIGHT: usize = 1;
/// Hard cap on cached tiles, and a pixel budget so a 4K-cell terminal at 4x
/// zoom cannot make the terminal cache hundreds of megabytes of RGBA.
const CACHE_TILES: usize = 32;
const CACHE_PIXELS: u64 = 48_000_000;
const ZOOMS: [f32; 7] = [1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 4.0];

/// Identifies one raster tile. `px_w` covers zoom + cell width; `cell_h`
/// covers the tile's pixel height.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TileKey {
    page: u32,
    idx: u32,
    px_w: u32,
    cell_h: u32,
    /// Horizontal offset into the page, in device pixels at the current scale.
    x: u32,
    /// Document generation; bumps on reload so tiles from the old file are
    /// never shown against the new one.
    gen: u64,
}

struct Tile {
    id: u32,
    cols: u16,
    used: u64,
    /// Decoded size (cols * cell_w * rows * cell_h) for the cache budget.
    px: u64,
}

/// A short cell-row glide for keyboard/wheel scrolling.
struct Glide {
    from: f32,
    to: f32,
    started: Instant,
    dur: Duration,
}

enum Req {
    Open {
        path: PathBuf,
        gen: u64,
    },
    Render {
        key: TileKey,
        page: usize,
        scale: f32,
        x0: u32,
        y0: u32,
        w: u32,
        h: u32,
    },
}

enum Rep {
    Opened {
        gen: u64,
        result: Result<Meta, String>,
    },
    Tile {
        key: TileKey,
        result: Result<Vec<u8>, String>,
        ms: f32,
    },
}

/// The render thread owns the document and answers open and tile requests.
fn worker(rx: Receiver<Req>, tx: Sender<Rep>) {
    let mut pdf: Option<Pdf> = None;
    while let Ok(req) = rx.recv() {
        match req {
            Req::Open { path, gen } => {
                let result = Pdf::open(&path)
                    .map(|p| {
                        let meta = p.meta().clone();
                        pdf = Some(p);
                        meta
                    })
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Rep::Opened { gen, result });
            }
            Req::Render {
                key,
                page,
                scale,
                x0,
                y0,
                w,
                h,
            } => {
                let start = Instant::now();
                let result = match pdf.as_mut() {
                    Some(p) => p
                        .render_band(page, scale, x0, y0, w, h)
                        .and_then(|img| render_img::encode_png(&img)),
                    None => Err(anyhow::anyhow!("document is not open")),
                }
                .map_err(|e| format!("{e:#}"));
                let ms = start.elapsed().as_secs_f32() * 1000.0;
                let _ = tx.send(Rep::Tile { key, result, ms });
            }
        }
    }
}

pub fn run(path: &Path, mut caps: Caps, theme: Theme, start_page: Option<usize>) -> Result<()> {
    crate::caps::settle(&mut caps);
    let path = std::fs::canonicalize(path).context("canonicalize")?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Open before taking over the screen so failures print normally.
    let (req_tx, req_rx) = unbounded();
    let (rep_tx, rep_rx) = unbounded();
    std::thread::Builder::new()
        .name("pdf-render".into())
        .spawn(move || worker(req_rx, rep_tx))
        .context("spawn pdf render thread")?;
    req_tx.send(Req::Open {
        path: path.clone(),
        gen: 0,
    })?;
    let meta = match rep_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Rep::Opened { gen: 0, result }) => result.map_err(anyhow::Error::msg)?,
        Ok(_) => bail!("unexpected reply from the pdf render thread"),
        Err(e) => bail!("pdf render thread did not answer: {e}"),
    };

    let (cw, ch) = (caps.cell.0.max(1) as u32, caps.cell.1.max(1) as u32);
    let mut screen = Screen::new(caps.clone(), theme.clone());
    let mut app = App {
        path: path.clone(),
        name,
        theme,
        meta,
        zoom: 0,
        x_frac: 0.0,
        cols: caps.cols,
        rows: caps.rows,
        cell: (cw, ch),
        starts: Vec::new(),
        page_rows: Vec::new(),
        total: 0,
        top: 0,
        follow: false,
        tiles: HashMap::new(),
        pending: HashSet::new(),
        failed: HashSet::new(),
        visible: HashSet::new(),
        tick: 0,
        gen: 0,
        req_tx,
        rep_rx,
        settle_until: None,
        glide: None,
        cell_dirty: false,
        edit_started: None,
        latency: None,
        last_ms: None,
        message: None,
        out: Vec::new(),
    };
    app.layout();
    if let Some(n) = start_page {
        let p = n.saturating_sub(1).min(app.meta.pages - 1);
        app.top = app.starts[p];
    }

    crate::screen::enter()?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut o = std::io::stdout();
        use std::io::Write;
        let _ = o.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = o.flush();
        prev_hook(info);
    }));

    let input = Input::spawn()?;

    let (fs_tx, fs_rx) = unbounded::<()>();
    let file = path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if ev
                .paths
                .iter()
                .any(|p| p == &file || p.file_name() == file.file_name())
            {
                let _ = fs_tx.send(());
            }
        }
    })?;
    watcher.watch(
        path.parent().unwrap_or(Path::new(".")),
        notify::RecursiveMode::NonRecursive,
    )?;

    let result = app
        .paint(&mut screen)
        .and_then(|_| app.event_loop(&mut screen, &input, fs_rx));
    app.cleanup();
    crate::screen::leave();
    result
}

struct App {
    path: PathBuf,
    name: String,
    theme: Theme,
    meta: Meta,
    zoom: usize,
    /// Left edge of the viewport as a fraction of the page width.
    x_frac: f32,
    cols: u16,
    rows: u16,
    cell: (u32, u32),

    /// Layout: first row of each page, and rows per page.
    starts: Vec<usize>,
    page_rows: Vec<usize>,
    total: usize,
    top: usize,
    follow: bool,

    tiles: HashMap<TileKey, Tile>,
    pending: HashSet<TileKey>,
    failed: HashSet<TileKey>,
    visible: HashSet<TileKey>,
    tick: u64,

    gen: u64,
    req_tx: Sender<Req>,
    rep_rx: Receiver<Rep>,
    settle_until: Option<Instant>,
    glide: Option<Glide>,
    /// A resize arrived; the terminal cell size may have changed (font size,
    /// or the pane finally got its real layout). Re-probe when it settles.
    cell_dirty: bool,
    edit_started: Option<Instant>,
    latency: Option<Duration>,
    last_ms: Option<f32>,
    message: Option<String>,
    out: Vec<u8>,
}

impl App {
    fn event_loop(&mut self, screen: &mut Screen, input: &Input, fs: Receiver<()>) -> Result<()> {
        let reps = self.rep_rx.clone();
        let events = input.events.clone();
        let mut reload_at: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let frame = self.glide.as_ref().map(|_| now + GLIDE_FRAME);
            let deadline = [reload_at, self.settle_until, frame]
                .into_iter()
                .flatten()
                .min();
            let timeout = deadline
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(Duration::from_secs(3600));
            let mut repaint = false;
            let mut quit = false;

            select! {
                recv(events) -> ev => match ev {
                    Ok(ev) => quit |= self.on_input(screen, ev, &mut repaint),
                    Err(_) => return Ok(()),
                },
                recv(fs) -> _ => {
                    reload_at = Some(Instant::now() + DEBOUNCE);
                    self.edit_started.get_or_insert_with(Instant::now);
                },
                recv(reps) -> r => if let Ok(r) = r { repaint |= self.on_done(screen, r); },
                default(timeout) => {},
            }
            while let Ok(ev) = events.try_recv() {
                quit |= self.on_input(screen, ev, &mut repaint);
            }
            while fs.try_recv().is_ok() {
                reload_at = Some(Instant::now() + DEBOUNCE);
                self.edit_started.get_or_insert_with(Instant::now);
            }
            while let Ok(r) = reps.try_recv() {
                repaint |= self.on_done(screen, r);
            }
            if quit {
                return Ok(());
            }

            if reload_at.is_some_and(|t| t <= Instant::now()) {
                reload_at = None;
                self.request_reload();
            }
            if self.advance_glide() {
                repaint = true;
            }
            if self.settle_until.is_some_and(|t| t <= Instant::now()) {
                self.settle_until = None;
                if self.cell_dirty {
                    self.cell_dirty = false;
                    if let Some(cell) = input.cell(Duration::from_millis(300)) {
                        if cell != (self.cell.0 as u16, self.cell.1 as u16) {
                            let (p, frac) = self.anchor();
                            self.cell = (cell.0 as u32, cell.1 as u32);
                            screen.caps.cell = cell;
                            self.clear_tiles();
                            self.layout();
                            self.restore(p, frac);
                            screen.invalidate();
                        }
                    }
                }
                repaint = true;
            }
            if repaint {
                self.paint(screen)?;
            }
        }
    }

    // ---- geometry ----

    fn view_height(&self) -> usize {
        (self.rows as usize).saturating_sub(1).max(1)
    }

    fn max_top(&self) -> usize {
        self.total.saturating_sub(self.view_height())
    }

    /// Page-number gutter: a space, the digits, and a space. Grows with the
    /// page count so a 1000-page document never writes past the terminal edge.
    fn gutter(&self) -> usize {
        (self.meta.pages.to_string().len() + 1).max(MIN_GUTTER)
    }

    fn page_cols(&self) -> u16 {
        self.cols
            .saturating_sub((self.gutter() + RIGHT) as u16)
            .max(8)
    }

    fn px_w(&self) -> u32 {
        self.page_cols() as u32 * self.cell.0.max(1)
    }

    fn scale_of(&self, page: usize) -> f32 {
        let (w, _) = self.meta.sizes[page];
        self.px_w() as f32 / w.max(1.0) * ZOOMS[self.zoom]
    }

    /// Device-pixel x of the page under the left edge of the viewport.
    /// Every page is normalised to `px_w * zoom` pixels wide, so this does
    /// not depend on the page's own width.
    fn x_off(&self) -> u32 {
        let full = self.px_w() as f32 * ZOOMS[self.zoom];
        (self.x_frac * full).round().max(0.0) as u32
    }

    fn max_x_frac(&self) -> f32 {
        (1.0 - 1.0 / ZOOMS[self.zoom]).max(0.0)
    }

    fn clamp_x(&mut self) {
        self.x_frac = self.x_frac.clamp(0.0, self.max_x_frac());
    }

    fn page_rows_at(&self, page: usize) -> usize {
        let (_, h) = self.meta.sizes[page];
        (h * self.scale_of(page) / self.cell.1.max(1) as f32)
            .ceil()
            .max(1.0) as usize
    }

    fn tiles_in_page(&self, page: usize) -> usize {
        self.page_rows[page].div_ceil(TILE_ROWS)
    }

    fn tile_rows(&self, page: usize, idx: usize) -> usize {
        self.page_rows[page]
            .saturating_sub(idx * TILE_ROWS)
            .min(TILE_ROWS)
    }

    fn tile_key(&self, page: usize, row_in_page: usize) -> TileKey {
        TileKey {
            page: page as u32,
            idx: (row_in_page / TILE_ROWS) as u32,
            px_w: self.px_w(),
            cell_h: self.cell.1,
            x: self.x_off(),
            gen: self.gen,
        }
    }

    fn layout(&mut self) {
        self.starts.clear();
        self.page_rows.clear();
        let mut y = 0;
        for p in 0..self.meta.pages {
            self.starts.push(y);
            let rows = self.page_rows_at(p);
            self.page_rows.push(rows);
            y += rows;
            if p + 1 < self.meta.pages {
                y += GAP;
            }
        }
        self.total = y;
    }

    fn page_at(&self, row: usize) -> usize {
        match self.starts.binary_search(&row) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// `(page, row within page)` when `row` lands on a page (not the gap).
    fn page_row(&self, row: usize) -> Option<(usize, usize)> {
        let p = self.page_at(row);
        let r = row.checked_sub(self.starts[p])?;
        (r < self.page_rows[p]).then_some((p, r))
    }

    /// Anchor as `(page, fraction of page height)`, stable across zoom and
    /// reload (page sizes may change when a document is rebuilt).
    fn anchor(&self) -> (usize, f32) {
        let p = self.page_at(self.top);
        let scale = self.scale_of(p).max(0.01);
        let off_pt = (self.top.saturating_sub(self.starts[p]) as f32 * self.cell.1 as f32) / scale;
        (p, (off_pt / self.meta.sizes[p].1).clamp(0.0, 1.0))
    }

    fn restore(&mut self, page: usize, frac: f32) {
        let p = page.min(self.meta.pages - 1);
        let y_pt = frac * self.meta.sizes[p].1;
        let row = (y_pt * self.scale_of(p) / self.cell.1.max(1) as f32).round() as usize;
        self.top = (self.starts[p] + row).min(self.max_top());
    }

    // ---- input ----

    fn on_input(&mut self, screen: &mut Screen, ev: Event, repaint: &mut bool) -> bool {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.key(screen, k, repaint),
            Event::Mouse(m) => {
                match m.kind {
                    MouseEventKind::ScrollDown => self.scroll(3, true, repaint),
                    MouseEventKind::ScrollUp => self.scroll(-3, true, repaint),
                    MouseEventKind::ScrollLeft => {
                        let step = self.page_cols() as isize / 4;
                        self.pan(-step, repaint);
                    }
                    MouseEventKind::ScrollRight => {
                        let step = self.page_cols() as isize / 4;
                        self.pan(step, repaint);
                    }
                    _ => {}
                }
                false
            }
            Event::Resize(..) => {
                self.glide = None;
                screen.resize();
                self.cols = screen.caps.cols;
                self.rows = screen.caps.rows;
                self.cell = (
                    screen.caps.cell.0.max(1) as u32,
                    screen.caps.cell.1.max(1) as u32,
                );
                let (p, frac) = self.anchor();
                self.clamp_x();
                self.clear_tiles();
                self.layout();
                self.restore(p, frac);
                self.settle_until = Some(Instant::now() + SETTLE);
                self.cell_dirty = true;
                screen.invalidate();
                *repaint = true;
                false
            }
            _ => false,
        }
    }

    fn key(&mut self, screen: &mut Screen, k: KeyEvent, repaint: &mut bool) -> bool {
        let page = self.view_height() as isize;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => self.scroll(1, true, repaint),
            KeyCode::Char('k') | KeyCode::Up => self.scroll(-1, true, repaint),
            KeyCode::Char('d') => self.scroll(page / 2, false, repaint),
            KeyCode::Char('u') => self.scroll(-(page / 2), false, repaint),
            KeyCode::Char('f') if ctrl => self.scroll(page - 2, false, repaint),
            KeyCode::Char('b') if ctrl => self.scroll(-(page - 2), false, repaint),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll(page - 2, false, repaint),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll(-(page - 2), false, repaint),
            KeyCode::Char('g') | KeyCode::Home => {
                self.glide = None;
                self.top = 0;
                self.follow = false;
                *repaint = true;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.glide = None;
                self.top = self.max_top();
                self.follow = true;
                *repaint = true;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let step = self.page_cols() as isize / 4;
                self.pan(-step, repaint);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let step = self.page_cols() as isize / 4;
                self.pan(step, repaint);
            }
            KeyCode::Char('n') | KeyCode::Char(']') => self.turn_page(1, repaint),
            KeyCode::Char('p') | KeyCode::Char('[') => self.turn_page(-1, repaint),
            KeyCode::Char('z') => self.zoom_by(1, screen, repaint),
            KeyCode::Char('Z') => self.zoom_by(-1, screen, repaint),
            KeyCode::Char(c @ '1'..='9') => {
                let d = c as usize - '0' as usize;
                let p = (self.meta.pages * d / 10).min(self.meta.pages - 1);
                self.glide = None;
                self.top = self.starts[p];
                self.follow = false;
                *repaint = true;
            }
            KeyCode::Char('r') if ctrl => {
                screen.invalidate();
                *repaint = true;
            }
            KeyCode::Char('r') => self.request_reload(),
            _ => {}
        }
        false
    }

    fn scroll(&mut self, by: isize, animate: bool, repaint: &mut bool) {
        let max = self.max_top();
        // Retarget from the pending destination so repeated keys accumulate
        // instead of restarting the glide on every press.
        let base = self
            .glide
            .as_ref()
            .map(|g| g.to.round() as isize)
            .unwrap_or(self.top as isize);
        let target = (base + by).clamp(0, max as isize);
        if target == base && self.glide.is_none() {
            return;
        }
        self.follow = target as usize == max && by > 0;
        if animate && by.unsigned_abs() <= GLIDE_MAX_ROWS && self.tiles_ready(target as usize) {
            let from = self.top as f32;
            let ms = (by.unsigned_abs() as u64 * GLIDE_MS_PER_ROW).clamp(40, 180);
            self.glide = Some(Glide {
                from,
                to: target as f32,
                started: Instant::now(),
                dur: Duration::from_millis(ms),
            });
        } else {
            self.glide = None;
            self.top = target as usize;
        }
        *repaint = true;
    }

    /// Step the scroll animation. Returns true when the view changed.
    fn advance_glide(&mut self) -> bool {
        let Some(g) = &self.glide else {
            return false;
        };
        let t = (g.started.elapsed().as_secs_f32() / g.dur.as_secs_f32()).clamp(0.0, 1.0);
        // Linear keeps the 32px steps evenly spaced; ease-out front-loads a
        // short glide into one or two frames, which reads as a jump.
        let top = (g.from + (g.to - g.from) * t).round() as usize;
        if t >= 1.0 {
            self.top = g.to.round() as usize;
            self.glide = None;
            return true;
        }
        let changed = top != self.top;
        self.top = top;
        if changed {
            if let Some(path) = std::env::var_os("DOCP_TRACE") {
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "glide top={top} t={t:.2}");
                }
            }
        }
        changed
    }

    /// Are all tiles for the viewport at `target` already in the cache?
    fn tiles_ready(&self, target: usize) -> bool {
        let body = self.view_height();
        let end = (target + body).min(self.total);
        if end <= target {
            return true;
        }
        let first = self.page_at(target);
        let last = self.page_at(end - 1);
        for p in first..=last {
            let ps = self.starts[p];
            let pe = ps + self.page_rows[p];
            let lo = target.max(ps);
            let hi = end.min(pe);
            if lo >= hi {
                continue;
            }
            let r0 = lo - ps;
            let r1 = hi - ps;
            for idx in (r0 / TILE_ROWS)..=(r1 - 1) / TILE_ROWS {
                let key = self.tile_key(p, idx * TILE_ROWS);
                if !self.tiles.contains_key(&key) {
                    return false;
                }
            }
        }
        true
    }

    fn turn_page(&mut self, by: isize, repaint: &mut bool) {
        self.glide = None;
        let p = self.page_at(self.top) as isize + by;
        let p = p.clamp(0, self.meta.pages as isize - 1) as usize;
        if self.starts[p] != self.top {
            self.top = self.starts[p];
            self.follow = false;
            *repaint = true;
        }
    }

    fn pan(&mut self, cols: isize, repaint: &mut bool) {
        if self.max_x_frac() <= 0.0 {
            return;
        }
        let px = cols as f32 * self.cell.0 as f32;
        let full = self.px_w() as f32 * ZOOMS[self.zoom];
        self.x_frac += px / full.max(1.0);
        self.clamp_x();
        *repaint = true;
    }

    fn zoom_by(&mut self, by: isize, screen: &mut Screen, repaint: &mut bool) {
        let zoom = (self.zoom as isize + by).clamp(0, ZOOMS.len() as isize - 1) as usize;
        if zoom == self.zoom {
            return;
        }
        let (p, frac) = self.anchor();
        self.glide = None;
        let center = self.x_frac + 0.5 / ZOOMS[self.zoom];
        self.zoom = zoom;
        self.x_frac = center - 0.5 / ZOOMS[self.zoom];
        self.clamp_x();
        self.clear_tiles();
        self.layout();
        self.restore(p, frac);
        screen.invalidate();
        *repaint = true;
    }

    // ---- rendering ----

    fn request(&mut self, page: usize, idx: usize) {
        if page >= self.meta.pages || idx >= self.tiles_in_page(page) {
            return;
        }
        let key = TileKey {
            page: page as u32,
            idx: idx as u32,
            px_w: self.px_w(),
            cell_h: self.cell.1,
            x: self.x_off(),
            gen: self.gen,
        };
        if self.tiles.contains_key(&key)
            || self.pending.contains(&key)
            || self.failed.contains(&key)
        {
            return;
        }
        let rows = self.tile_rows(page, idx);
        let h = rows as u32 * self.cell.1;
        let y0 = idx as u32 * TILE_ROWS as u32 * self.cell.1;
        self.pending.insert(key);
        let _ = self.req_tx.send(Req::Render {
            key,
            page,
            scale: self.scale_of(page),
            x0: key.x,
            y0,
            w: self.px_w(),
            h,
        });
    }

    /// Queue the tiles needed for the current viewport, then a light prefetch.
    fn ensure(&mut self) {
        if self.settle_until.is_some() {
            return;
        }
        let body = self.view_height();
        let top = self.top;
        let end = (top + body).min(self.total);
        if end <= top {
            return;
        }
        let first = self.page_at(top);
        let last = self.page_at(end - 1);
        let mut segs = Vec::new();
        for p in first..=last {
            let ps = self.starts[p];
            let pe = ps + self.page_rows[p];
            let lo = top.max(ps);
            let hi = end.min(pe);
            if lo < hi {
                segs.push((p, lo - ps, hi - ps));
            }
        }
        for &(p, r0, r1) in &segs {
            for idx in (r0 / TILE_ROWS)..=(r1 - 1) / TILE_ROWS {
                self.request(p, idx);
            }
        }

        // One tile each side keeps a slow scroll from showing blanks.
        for &(p, r0, r1) in &segs {
            if r0 >= TILE_ROWS {
                self.request(p, r0 / TILE_ROWS - 1);
            }
            let last_idx = (r1 - 1) / TILE_ROWS;
            self.request(p, last_idx + 1);
            self.request(p, last_idx + 2);
        }

        // Adjacent pages: the next page turn should hit the cache. Skip at
        // deep zoom where a prefetched tile is expensive.
        if ZOOMS[self.zoom] <= 2.0 && self.pending.is_empty() {
            if first > 0 {
                let p = first - 1;
                self.request(p, self.tiles_in_page(p).saturating_sub(1));
            }
            if last + 1 < self.meta.pages {
                self.request(last + 1, 0);
            }
        }
    }

    fn valid(&self, key: TileKey) -> bool {
        key.gen == self.gen
            && key.px_w == self.px_w()
            && key.cell_h == self.cell.1
            && key.x == self.x_off()
            && (key.page as usize) < self.meta.pages
            && (key.idx as usize) < self.tiles_in_page(key.page as usize)
    }

    fn on_done(&mut self, screen: &mut Screen, rep: Rep) -> bool {
        match rep {
            Rep::Opened { gen, result } => {
                if gen != self.gen {
                    return false;
                }
                match result {
                    Ok(meta) if meta.pages > 0 => {
                        let (p, frac) = self.anchor();
                        self.glide = None;
                        self.meta = meta;
                        self.clear_tiles();
                        self.layout();
                        self.restore(p, frac);
                        self.latency = self.edit_started.take().map(|t| t.elapsed());
                        self.message = None;
                        screen.invalidate();
                        true
                    }
                    Ok(_) => {
                        self.message = Some("pdf has no pages".into());
                        false
                    }
                    Err(e) => {
                        self.message = Some(e);
                        false
                    }
                }
            }
            Rep::Tile { key, result, ms } => {
                self.pending.remove(&key);
                if !self.valid(key) {
                    return false;
                }
                self.last_ms = Some(ms);
                match result {
                    Ok(png) => {
                        if let Some(dir) = std::env::var_os("DOCP_DUMP_RASTER") {
                            let _ = std::fs::write(
                                std::path::Path::new(&dir).join(format!(
                                    "pdf-p{}-t{}-x{}-{}.png",
                                    key.page + 1,
                                    key.idx,
                                    key.x,
                                    key.px_w
                                )),
                                &png,
                            );
                        }
                        let page = key.page as usize;
                        let rows = self.tile_rows(page, key.idx as usize) as u16;
                        let cols = self.page_cols();
                        let id = kgp::alloc_id();
                        kgp::transmit(&mut self.out, id, &png, cols, rows);
                        let px =
                            cols as u64 * self.cell.0 as u64 * rows as u64 * self.cell.1 as u64;
                        self.tiles.insert(
                            key,
                            Tile {
                                id,
                                cols,
                                used: self.tick,
                                px,
                            },
                        );
                        self.evict();
                        true
                    }
                    Err(e) => {
                        self.failed.insert(key);
                        self.message = Some(e);
                        true
                    }
                }
            }
        }
    }

    fn evict(&mut self) {
        let mut px: u64 = self.tiles.values().map(|t| t.px).sum();
        while self.tiles.len() > CACHE_TILES || px > CACHE_PIXELS {
            let victim = self
                .tiles
                .iter()
                .filter(|(k, _)| !self.visible.contains(*k))
                .min_by_key(|(_, t)| t.used)
                .map(|(k, t)| (*k, t.id, t.px));
            let Some((key, id, freed)) = victim else {
                break;
            };
            px = px.saturating_sub(freed);
            self.tiles.remove(&key);
            kgp::delete(&mut self.out, id);
        }
    }

    fn clear_tiles(&mut self) {
        for (_, t) in self.tiles.drain() {
            kgp::delete(&mut self.out, t.id);
        }
        self.pending.clear();
        self.failed.clear();
        self.visible.clear();
    }

    // ---- watch ----

    fn request_reload(&mut self) {
        self.gen += 1;
        let _ = self.req_tx.send(Req::Open {
            path: self.path.clone(),
            gen: self.gen,
        });
    }

    // ---- painting ----

    fn paint(&mut self, screen: &mut Screen) -> Result<()> {
        self.top = self.top.min(self.max_top());
        self.ensure();
        let body = self.view_height();
        let mut rows = Vec::with_capacity(body);
        let mut visible = HashSet::new();
        let mut tick = self.tick;

        for y in 0..body {
            let row = self.top + y;
            let mut s = String::from("\x1b[0m");
            if let Some((p, r)) = self.page_row(row) {
                if r == 0 {
                    let mut a = String::new();
                    docmodel::sgr(
                        &mut a,
                        &Attrs {
                            fg: Some(self.theme.muted()),
                            ..Default::default()
                        },
                    );
                    s.push_str(&a);
                    s.push_str(&format!("{:>width$} ", p + 1, width = self.gutter() - 1));
                    s.push_str("\x1b[0m");
                } else {
                    s.push_str(&" ".repeat(self.gutter()));
                }
                let key = self.tile_key(p, r);
                if let Some(t) = self.tiles.get_mut(&key) {
                    tick += 1;
                    t.used = tick;
                    visible.insert(key);
                    s.push_str(&kgp::placeholder_row(t.id, (r % TILE_ROWS) as u16, t.cols));
                }
            }
            rows.push(s);
        }
        self.tick = tick;
        self.visible = visible;
        self.evict();

        let page = self.page_at(self.top);
        let max_top = self.max_top();
        // 0 % at the top, 100 % at the bottom; a single-screen document is 100 %.
        let pct = (self.top.min(max_top) * 100)
            .checked_div(max_top)
            .unwrap_or(100)
            .min(100);
        let mut right = format!(
            "p {}/{} · {:.0}% · {}%",
            page + 1,
            self.meta.pages,
            ZOOMS[self.zoom] * 100.0,
            pct
        );
        if let Some(ms) = self.last_ms {
            right.push_str(&format!(" · tile {ms:.0}ms"));
        }
        if self.follow {
            right.push_str(" · follow");
        }
        if let Some(l) = self.latency {
            right.push_str(&format!(" · reload {:.0}ms", l.as_secs_f64() * 1000.0));
        }
        let left = match &self.message {
            Some(m) => format!("{} · {m}", self.name),
            None => self.name.clone(),
        };
        screen.paint(
            &Frame {
                rows,
                status_left: left,
                status_right: right,
            },
            &mut self.out,
        )
    }

    fn cleanup(&mut self) {
        let mut out = Vec::new();
        for t in self.tiles.values() {
            kgp::delete(&mut out, t.id);
        }
        use std::io::Write;
        let _ = std::io::stdout().write_all(&out);
    }
}
