//! Markdown view: hybrid rendering (text + pixels), live reload, anchored
//! scrolling, block-level diffing.

use crate::caps::Caps;
use crate::input::Input;
use crate::screen::{Frame, Screen};
use crate::{ipc, srcmap};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use docmodel::{Attrs, Cell, Line, Row, Theme};
use notify::Watcher;
use render_md::raster::{self, Cell as PixelCell};
use render_md::{doc, pele, svgflat, text};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xxhash_rust::xxh3::{xxh3_64, Xxh3};

const DEBOUNCE: Duration = Duration::from_millis(15);
const SETTLE: Duration = Duration::from_millis(80);
const MARK: Duration = Duration::from_millis(1500);
/// Minimum gap between `top` messages sent to an attached editor.
const EMIT_MIN: Duration = Duration::from_millis(33);

const GUTTER: usize = 2;

struct Flat {
    svg: Arc<String>,
    w: f64,
    h: f64,
    hash: u64,
}

enum Diagram {
    Pending,
    Ready(Arc<Flat>),
    Failed(String),
}

struct Media {
    id: u32,
    cols: u16,
    rows: u16,
    placed: (u16, u16),
}

/// A block image from the document, tiled to stay under the placeholder
/// row limit.
struct ImgBlock {
    tiles: Vec<Media>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Ident {
    Mermaid(usize),
    Heading(u64),
    Image(usize),
}

enum Job {
    Heading(raster::Heading),
    Diagram(Arc<Flat>, u16),
}

struct Done {
    key: u64,
    result: Result<raster::Raster, String>,
}

pub struct App {
    path: PathBuf,
    name: String,
    src_hash: Option<u64>,
    doc: doc::Doc,
    theme: Theme,
    graphics: bool,
    cell: PixelCell,
    cols: u16,
    rows: u16,
    max_width: usize,

    top: usize,
    follow: bool,
    layout: Vec<Row>,
    starts: Vec<usize>,
    text_cache: HashMap<(u64, usize), Arc<Vec<Line>>>,

    pele: pele::Pele,
    pele_tags: HashMap<u64, (u64, u32)>,
    diagrams: HashMap<(u64, u32), Diagram>,
    media: HashMap<u64, Media>,
    img_blocks: HashMap<u64, ImgBlock>,
    pending: HashSet<u64>,
    wanted: HashSet<u64>,
    stale: HashMap<Ident, u64>,
    done_tx: Sender<Done>,

    changed: HashMap<usize, Instant>,
    settle_until: Option<Instant>,
    /// A resize arrived; re-probe the cell when it settles (font size changes
    /// and a pane's first real layout both alter it).
    cell_dirty: bool,
    edit_started: Option<Instant>,
    latency: Option<Duration>,
    message: Option<String>,

    /// Editor sync: byte↔line map of the last parse.
    lines: srcmap::LineMap,
    ipc: Option<ipc::Ipc>,
    /// Keeps the channel open when no editor socket was requested.
    ipc_idle: Option<Sender<ipc::Event>>,
    /// The top row was set by the editor; do not echo it back.
    ipc_scroll: bool,
    /// The top changed locally and owes the editor a `top` message.
    top_dirty: bool,
    seen_top: usize,
    emitted: Option<usize>,
    emit_at: Option<Instant>,
    last_emit: Option<Instant>,

    out: Vec<u8>,
}

pub fn run(
    path: &Path,
    mut caps: Caps,
    theme: Theme,
    max_width: usize,
    ipc_path: Option<&Path>,
) -> anyhow::Result<()> {
    crate::caps::settle(&mut caps);
    let path = std::fs::canonicalize(path)?;
    render_md::fonts::warm_system();
    let (pele_tx, pele_rx) = unbounded();
    let pele = pele::Pele::spawn(pele_tx);

    let (done_tx, done_rx) = unbounded();
    let (cw, ch) = (caps.cell.0.max(1) as u32, caps.cell.1.max(1) as u32);
    let mut screen = Screen::new(caps.clone(), theme.clone());
    let mut app = App {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.clone(),
        src_hash: None,
        doc: doc::Doc { blocks: Vec::new() },
        theme,
        graphics: caps.graphics && std::env::var_os("DOCP_NO_GRAPHICS").is_none(),
        cell: PixelCell { w: cw, h: ch },
        cols: caps.cols,
        rows: caps.rows,
        max_width: max_width.clamp(20, 400),
        top: 0,
        follow: false,
        layout: Vec::new(),
        starts: Vec::new(),
        text_cache: HashMap::new(),
        pele,
        pele_tags: HashMap::new(),
        diagrams: HashMap::new(),
        media: HashMap::new(),
        img_blocks: HashMap::new(),
        pending: HashSet::new(),
        wanted: HashSet::new(),
        stale: HashMap::new(),
        done_tx,
        changed: HashMap::new(),
        settle_until: None,
        cell_dirty: false,
        edit_started: Some(Instant::now()),
        latency: None,
        message: None,
        lines: srcmap::LineMap::default(),
        ipc: None,
        ipc_idle: None,
        ipc_scroll: false,
        top_dirty: false,
        seen_top: 0,
        emitted: None,
        emit_at: None,
        last_emit: None,
        out: Vec::new(),
    };

    let (ipc_rx, ipc_idle) = match ipc_path {
        Some(sock) => {
            let (server, rx) = ipc::Ipc::listen(sock)?;
            app.ipc = Some(server);
            (rx, None)
        }
        None => {
            // Keep a sender alive so `select!` never observes a disconnected
            // receiver and spins.
            let (tx, rx) = unbounded();
            (rx, Some(tx))
        }
    };
    app.ipc_idle = ipc_idle;

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

    app.reload(&mut screen);
    let result = app.event_loop(&mut screen, &input, fs_rx, pele_rx, done_rx, ipc_rx);
    app.cleanup();
    crate::screen::leave();
    result
}

impl App {
    fn event_loop(
        &mut self,
        screen: &mut Screen,
        input: &Input,
        fs: Receiver<()>,
        pele: Receiver<pele::Response>,
        done: Receiver<Done>,
        ipc_events: Receiver<ipc::Event>,
    ) -> anyhow::Result<()> {
        let events = input.events.clone();
        let mut reload_at: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let deadline = [
                reload_at,
                self.settle_until,
                self.next_mark_expiry(),
                self.emit_at,
            ]
            .into_iter()
            .flatten()
            .min();
            let timeout = deadline
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(Duration::from_secs(3600));
            let mut relayout = false;
            let mut repaint = false;
            let mut quit = false;

            select! {
                recv(events) -> ev => match ev {
                    Ok(ev) => quit |= self.on_input(screen, ev, &mut relayout, &mut repaint),
                    Err(_) => return Ok(()),
                },
                recv(fs) -> _ => {
                    reload_at = Some(Instant::now() + DEBOUNCE);
                    self.edit_started.get_or_insert_with(Instant::now);
                },
                recv(pele) -> r => if let Ok(r) = r { self.on_pele(r); relayout = true; },
                recv(done) -> d => if let Ok(d) = d { relayout |= self.on_done(d); },
                recv(ipc_events) -> m => if let Ok(m) = m {
                    self.on_ipc(m, &mut repaint);
                },
                default(timeout) => {},
            }
            while let Ok(ev) = events.try_recv() {
                quit |= self.on_input(screen, ev, &mut relayout, &mut repaint);
            }
            while fs.try_recv().is_ok() {
                reload_at = Some(Instant::now() + DEBOUNCE);
            }
            while let Ok(r) = pele.try_recv() {
                self.on_pele(r);
                relayout = true;
            }
            while let Ok(d) = done.try_recv() {
                relayout |= self.on_done(d);
            }
            while let Ok(m) = ipc_events.try_recv() {
                self.on_ipc(m, &mut repaint);
            }
            if quit {
                return Ok(());
            }

            let now = Instant::now();
            // Any top change from local input is owed to the editor; changes
            // the editor asked for (`ipc_scroll`) never bounce back.
            if self.top != self.seen_top {
                self.seen_top = self.top;
                if !self.ipc_scroll {
                    self.top_dirty = true;
                }
            }
            if self.top_dirty || self.emit_at.is_some_and(|t| t <= now) {
                self.flush_top();
            }
            if reload_at.is_some_and(|t| t <= now) {
                reload_at = None;
                self.reload(screen);
                repaint = true;
            }
            if self.settle_until.is_some_and(|t| t <= now) {
                self.settle_until = None;
                if self.cell_dirty {
                    self.cell_dirty = false;
                    if let Some(cell) = input.cell(Duration::from_millis(300)) {
                        if (cell.0 as u32, cell.1 as u32) != (self.cell.w, self.cell.h) {
                            self.cell = PixelCell {
                                w: cell.0 as u32,
                                h: cell.1 as u32,
                            };
                            screen.caps.cell = cell;
                            screen.invalidate();
                        }
                    }
                }
                relayout = true;
            }
            let before = self.changed.len();
            self.changed.retain(|_, t| now.duration_since(*t) < MARK);
            repaint |= self.changed.len() != before;

            if relayout {
                self.relayout_keep_anchor(screen);
                repaint = true;
            }
            if repaint {
                self.paint(screen)?;
            }
        }
    }

    fn next_mark_expiry(&self) -> Option<Instant> {
        self.changed.values().min().map(|t| *t + MARK)
    }

    fn content_width(&self) -> usize {
        (self.cols as usize)
            .saturating_sub(GUTTER + 2)
            .clamp(10, self.max_width)
    }

    fn view_height(&self) -> usize {
        (self.rows as usize).saturating_sub(1).max(1)
    }

    fn max_top(&self) -> usize {
        self.layout.len().saturating_sub(self.view_height())
    }

    // ---- input ----

    fn on_input(
        &mut self,
        screen: &mut Screen,
        ev: Event,
        relayout: &mut bool,
        repaint: &mut bool,
    ) -> bool {
        // Local input takes over from the editor.
        if matches!(ev, Event::Key(_) | Event::Mouse(_)) {
            self.ipc_scroll = false;
        }
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.key(k, repaint),
            Event::Mouse(m) => {
                match m.kind {
                    MouseEventKind::ScrollDown => self.scroll(3, repaint),
                    MouseEventKind::ScrollUp => self.scroll(-3, repaint),
                    _ => {}
                }
                false
            }
            Event::Resize(..) => {
                screen.resize();
                self.cols = screen.caps.cols;
                self.rows = screen.caps.rows;
                let (cw, ch) = (
                    screen.caps.cell.0.max(1) as u32,
                    screen.caps.cell.1.max(1) as u32,
                );
                self.cell = PixelCell { w: cw, h: ch };
                self.settle_until = Some(Instant::now() + SETTLE);
                self.cell_dirty = true;
                *relayout = true;
                false
            }
            _ => false,
        }
    }

    fn key(&mut self, k: KeyEvent, repaint: &mut bool) -> bool {
        let page = self.view_height() as isize;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => self.scroll(1, repaint),
            KeyCode::Char('k') | KeyCode::Up => self.scroll(-1, repaint),
            KeyCode::Char('d') => self.scroll(page / 2, repaint),
            KeyCode::Char('u') => self.scroll(-page / 2, repaint),
            KeyCode::Char('f') if ctrl => self.scroll(page - 2, repaint),
            KeyCode::Char('b') if ctrl => self.scroll(-(page - 2), repaint),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll(page - 2, repaint),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll(-(page - 2), repaint),
            KeyCode::Char('g') | KeyCode::Home => {
                self.top = 0;
                self.follow = false;
                *repaint = true;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.top = self.max_top();
                self.follow = true;
                *repaint = true;
            }
            KeyCode::Char(']') => self.jump_heading(true, repaint),
            KeyCode::Char('[') => self.jump_heading(false, repaint),
            KeyCode::Char('r') if ctrl => {
                *repaint = true;
            }
            _ => {}
        }
        false
    }

    fn scroll(&mut self, by: isize, repaint: &mut bool) {
        let max = self.max_top();
        let top = (self.top as isize + by).clamp(0, max as isize) as usize;
        if top != self.top {
            self.top = top;
            *repaint = true;
        }
        self.follow = self.top == max && by > 0;
    }

    fn jump_heading(&mut self, forward: bool, repaint: &mut bool) {
        let heads: Vec<usize> = self
            .doc
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| matches!(b.node, doc::Node::Heading { .. }))
            .filter_map(|(i, _)| self.starts.get(i).copied())
            .collect();
        let target = if forward {
            heads.into_iter().find(|&r| r > self.top)
        } else {
            heads.into_iter().rev().find(|&r| r < self.top)
        };
        if let Some(t) = target {
            self.top = t.min(self.max_top());
            self.follow = false;
            *repaint = true;
        }
    }

    // ---- document ----

    fn reload(&mut self, screen: &mut Screen) {
        let src = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) => {
                self.message = Some(format!("can't read: {e}"));
                self.edit_started = None;
                let _ = self.paint(screen);
                return;
            }
        };
        self.message = None;
        let hash = xxh3_64(src.as_bytes());
        if self.src_hash == Some(hash) {
            self.edit_started = None;
            return;
        }
        let first = self.src_hash.is_none();
        self.src_hash = Some(hash);
        self.lines = srcmap::LineMap::new(&src);
        let new = doc::parse(&src);

        let at_bottom = !first && (self.follow || (self.top > 0 && self.top >= self.max_top()));
        let (anchor_block, offset) = self.anchor();
        let anchor_hash = self.doc.blocks.get(anchor_block).map(|b| b.hash);
        let old: HashSet<u64> = self.doc.blocks.iter().map(|b| b.hash).collect();

        let now = Instant::now();
        self.changed.clear();
        if !first {
            for (i, b) in new.blocks.iter().enumerate() {
                if !old.contains(&b.hash) {
                    self.changed.insert(i, now);
                }
            }
        }
        let new_anchor = anchor_hash
            .and_then(|h| {
                new.blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.hash == h)
                    .min_by_key(|(i, _)| i.abs_diff(anchor_block))
                    .map(|(i, _)| i)
            })
            .unwrap_or(anchor_block.min(new.blocks.len().saturating_sub(1)));
        self.doc = new;
        self.relayout(screen);
        self.restore_anchor(new_anchor, offset);
        if at_bottom {
            self.top = self.max_top();
            self.follow = true;
        }
        // Reloading must not push a `top` back at the editor: the save came
        // from there, and it re-sends its line if it wants to re-anchor.
        self.seen_top = self.top;
        self.top_dirty = false;
        let _ = self.paint(screen);
    }

    fn anchor(&self) -> (usize, usize) {
        if self.layout.get(self.top).is_none() {
            return (0, 0);
        }
        let block = self.block_at(self.top);
        let start = self.starts.get(block).copied().unwrap_or(0);
        (block, self.top.saturating_sub(start))
    }

    /// Which block a layout row belongs to.
    fn block_at(&self, row: usize) -> usize {
        match self.starts.binary_search(&row) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    fn restore_anchor(&mut self, block: usize, offset: usize) {
        let start = self.starts.get(block).copied().unwrap_or(0);
        let end = self
            .starts
            .get(block + 1)
            .copied()
            .unwrap_or(self.layout.len());
        let top = start + offset.min(end.saturating_sub(start + 1));
        self.top = top.min(self.max_top());
    }

    fn relayout_keep_anchor(&mut self, screen: &mut Screen) {
        let follow = self.follow;
        let (b, o) = self.anchor();
        self.relayout(screen);
        self.restore_anchor(b, o);
        if follow {
            self.top = self.max_top();
        }
        self.seen_top = self.top;
    }

    // ---- layout ----

    fn relayout(&mut self, _screen: &mut Screen) {
        let width = self.content_width();
        let mut rows = Vec::with_capacity(self.layout.len());
        let mut starts = Vec::with_capacity(self.doc.blocks.len());
        let mut new_stale = HashMap::new();
        self.wanted.clear();
        let mut used_text = HashSet::new();
        let mut used_diagrams = HashSet::new();
        let mut used_images = HashSet::new();
        let mut ordinal = 0;
        for i in 0..self.doc.blocks.len() {
            if i > 0 {
                rows.push(Row::Text(Line::default()));
                // Headings get extra air above them so sections read apart.
                if matches!(self.doc.blocks[i].node, doc::Node::Heading { .. }) {
                    rows.push(Row::Text(Line::default()));
                }
            }
            starts.push(rows.len());
            for kind in self.block_rows(
                i,
                width,
                &mut ordinal,
                &mut new_stale,
                &mut used_text,
                &mut used_diagrams,
                &mut used_images,
            ) {
                rows.push(kind);
            }
        }
        self.layout = rows;
        self.starts = starts;
        self.text_cache.retain(|k, _| used_text.contains(k));
        self.diagrams
            .retain(|k, d| used_diagrams.contains(k) || matches!(d, Diagram::Pending));
        // Drop block images that are gone, freeing their tiles in the terminal.
        let mut dropped_blocks = Vec::new();
        self.img_blocks.retain(|k, b| {
            let keep = used_images.contains(k);
            if !keep {
                dropped_blocks.push(b.tiles.iter().map(|m| m.id).collect::<Vec<_>>());
            }
            keep
        });
        for ids in dropped_blocks {
            for id in ids {
                kgp::delete(&mut self.out, id);
            }
        }
        // Images neither shown nor kept as a stand-in are freed in the terminal.
        let keep: HashSet<u64> = new_stale.values().copied().collect();
        let mut freed = Vec::new();
        self.media.retain(|k, m| {
            let k = keep.contains(k);
            if !k {
                freed.push(m.id);
            }
            k
        });
        for id in freed {
            kgp::delete(&mut self.out, id);
        }
        self.stale = new_stale;
    }

    #[allow(clippy::too_many_arguments)]
    fn block_rows(
        &mut self,
        i: usize,
        width: usize,
        ordinal: &mut usize,
        stale: &mut HashMap<Ident, u64>,
        used_text: &mut HashSet<(u64, usize)>,
        used_diagrams: &mut HashSet<(u64, u32)>,
        used_images: &mut HashSet<u64>,
    ) -> Vec<Row> {
        let block = &self.doc.blocks[i];
        let hash = block.hash;
        let kind = match &block.node {
            doc::Node::Heading { level, spans } if *level <= 2 && self.graphics => {
                Kind::Heading(*level, doc::plain(spans))
            }
            doc::Node::Mermaid(src) if self.graphics => Kind::Mermaid(src.clone()),
            doc::Node::Image { url } if self.graphics => Kind::Image(url.clone()),
            _ => Kind::Text,
        };
        match kind {
            Kind::Heading(level, text) => {
                if text.trim().is_empty() {
                    return self.text_rows(i, width, used_text);
                }
                let spec = raster::layout_heading(&text, level, self.cell, width as u16);
                let mut h = Xxh3::new();
                ("heading", &spec.lines, spec.level, self.cell, self.theme.fg).hash(&mut h);
                let key = h.digest();
                let ident = Ident::Heading(hash);
                if self.media.contains_key(&key) {
                    stale.insert(ident, key);
                    return self.image_rows(key, width);
                }
                let rows = spec.rows();
                self.want(key, Job::Heading(spec));
                if let Some(&old) = self
                    .stale
                    .get(&ident)
                    .filter(|k| self.media.contains_key(k))
                {
                    stale.insert(ident, old);
                    return self.image_rows(old, width);
                }
                // Reserve the final height so the image lands without a jump.
                let mut out = self.text_rows(i, width, used_text);
                out.truncate(rows as usize);
                while out.len() < rows as usize {
                    out.push(Row::Text(Line::default()));
                }
                out
            }
            Kind::Mermaid(src) => {
                let ident = Ident::Mermaid(*ordinal);
                *ordinal += 1;
                let scale = raster::diagram_scale(self.cell);
                let max_css = pele::layout_width((width as u32 * self.cell.w) as f64, scale);
                let dkey = (hash, max_css);
                used_diagrams.insert(dkey);
                let mut error = None;
                match self.diagrams.get(&dkey) {
                    None => {
                        if self.settle_until.is_none() {
                            self.request_pele(dkey, src);
                        }
                    }
                    Some(Diagram::Pending) => {}
                    Some(Diagram::Ready(flat)) => {
                        let flat = flat.clone();
                        let (cols, rows, _) =
                            raster::diagram_cells(flat.w, flat.h, self.cell, width as u16);
                        let mut h = Xxh3::new();
                        ("diagram", flat.hash, cols, rows, self.cell).hash(&mut h);
                        let key = h.digest();
                        if self.media.contains_key(&key) {
                            stale.insert(ident, key);
                            return self.image_rows(key, width);
                        }
                        self.want(key, Job::Diagram(flat, width as u16));
                    }
                    Some(Diagram::Failed(e)) => error = Some(e.clone()),
                }
                let mut out = match self.stale.get(&ident) {
                    Some(&old) if self.media.contains_key(&old) => {
                        stale.insert(ident, old);
                        self.image_rows(old, width)
                    }
                    _ => {
                        let mut rows = self.text_rows(i, width, used_text);
                        for r in &mut rows {
                            if let Row::Text(l) = r {
                                for c in &mut l.cells {
                                    c.attrs.fg = Some(self.theme.muted());
                                }
                            }
                        }
                        rows
                    }
                };
                if let Some(e) = error {
                    let mut l = Line::default();
                    l.cells.push(Cell {
                        text: format!("✗ {e}"),
                        attrs: Attrs {
                            fg: Some(self.theme.red()),
                            ..Default::default()
                        },
                        url: None,
                    });
                    out.push(Row::Text(l));
                }
                out
            }
            Kind::Image(url) => {
                let ident = Ident::Image(*ordinal);
                *ordinal += 1;
                match self.load_image_block(&url, width, hash, used_images) {
                    Some(block) => {
                        stale.insert(ident, block);
                        self.image_rows(block, width)
                    }
                    None => self.text_rows(i, width, used_text),
                }
            }
            Kind::Text => self.text_rows(i, width, used_text),
        }
    }

    /// Decode/lay out a markdown image block (cached by url + width + cell).
    /// Returns the media key.
    fn load_image_block(
        &mut self,
        url: &str,
        width: usize,
        block_hash: u64,
        used: &mut HashSet<u64>,
    ) -> Option<u64> {
        let path = self.resolve(url)?;
        let data_hash = std::fs::metadata(&path).ok().map(|m| {
            let len = m.len();
            let time = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            xxh3_64(format!("{}{}", len, time).as_bytes())
        })?;
        let mut h = Xxh3::new();
        ("mdimg", &path, data_hash, width, self.cell, block_hash).hash(&mut h);
        let key = h.digest();
        used.insert(key);
        if self.img_blocks.contains_key(&key) {
            return Some(key);
        }
        let img = render_img::load(&path).ok()?;
        let (cw, ch) = (self.cell.w, self.cell.h);
        let lay = render_img::layout(&img, (cw, ch), width as u16).ok()?;
        let tiles = lay
            .tiles
            .iter()
            .map(|t| Media {
                id: kgp::alloc_id(),
                cols: t.cols,
                rows: t.rows,
                placed: (t.cols, t.rows),
            })
            .collect::<Vec<_>>();
        for (t, m) in lay.tiles.iter().zip(tiles.iter()) {
            kgp::transmit(&mut self.out, m.id, &t.png, m.cols, m.rows);
        }
        let block = ImgBlock { tiles };
        self.img_blocks.insert(key, block);
        Some(key)
    }

    fn resolve(&self, url: &str) -> Option<PathBuf> {
        if url.starts_with("http://") || url.starts_with("https://") {
            return None;
        }
        let p = PathBuf::from(url);
        if p.is_absolute() {
            return p.exists().then_some(p);
        }
        let base = self.path.parent()?;
        let joined = base.join(url.trim_start_matches("./"));
        joined.exists().then_some(joined)
    }

    fn text_rows(&mut self, i: usize, width: usize, used: &mut HashSet<(u64, usize)>) -> Vec<Row> {
        let block = &self.doc.blocks[i];
        let key = (block.hash, width);
        used.insert(key);
        let lines = match self.text_cache.get(&key) {
            Some(l) => l.clone(),
            None => {
                let ctx = text::Ctx { theme: &self.theme };
                let l = Arc::new(ctx.node(&block.node, width));
                self.text_cache.insert(key, l.clone());
                l
            }
        };
        lines.iter().cloned().map(Row::Text).collect()
    }

    /// Rows for a transmitted image, rescaling its placement in the terminal
    /// when it is wider than the space it has now.
    fn image_rows(&mut self, key: u64, width: usize) -> Vec<Row> {
        // Block images are tiled; the first tile key maps to the block.
        if let Some(block) = self.img_blocks.get_mut(&key) {
            let mut out = Vec::new();
            for m in &mut block.tiles {
                let cols = m.cols.min(width as u16).max(1);
                let rows = if cols == m.cols {
                    m.rows
                } else {
                    ((m.rows as u32 * cols as u32).div_ceil(m.cols as u32)).max(1) as u16
                };
                if m.placed != (cols, rows) {
                    kgp::replace_placement(&mut self.out, m.id, cols, rows);
                    m.placed = (cols, rows);
                }
                out.extend((0..rows).map(|row| Row::Image {
                    id: m.id,
                    row,
                    cols,
                }));
            }
            return out;
        }
        let Some(img) = self.media.get_mut(&key) else {
            return Vec::new();
        };
        let cols = img.cols.min(width as u16).max(1);
        let rows = if cols == img.cols {
            img.rows
        } else {
            ((img.rows as u32 * cols as u32).div_ceil(img.cols as u32)).max(1) as u16
        };
        if img.placed != (cols, rows) {
            kgp::replace_placement(&mut self.out, img.id, cols, rows);
            img.placed = (cols, rows);
        }
        let id = img.id;
        (0..rows).map(|row| Row::Image { id, row, cols }).collect()
    }

    fn request_pele(&mut self, dkey: (u64, u32), src: String) {
        let mut h = Xxh3::new();
        dkey.hash(&mut h);
        let tag = h.digest();
        self.pele_tags.insert(tag, dkey);
        self.diagrams.insert(dkey, Diagram::Pending);
        self.pele.request(pele::Request {
            tag,
            source: src,
            max_width: dkey.1 as f64,
        });
    }

    fn on_pele(&mut self, r: pele::Response) {
        let Some(dkey) = self.pele_tags.remove(&r.tag) else {
            return;
        };
        let state = match r.result {
            Ok(svg) => {
                let flat = svgflat::flatten(&svg.svg, &self.theme);
                let hash = xxh3_64(flat.as_bytes());
                Diagram::Ready(Arc::new(Flat {
                    svg: Arc::new(flat),
                    w: svg.width,
                    h: svg.height,
                    hash,
                }))
            }
            Err(e) => Diagram::Failed(e.message),
        };
        self.diagrams.insert(dkey, state);
    }

    fn want(&mut self, key: u64, job: Job) {
        self.wanted.insert(key);
        if self.pending.contains(&key) || self.settle_until.is_some() {
            return;
        }
        self.pending.insert(key);
        let tx = self.done_tx.clone();
        let cell = self.cell;
        let fg = self.theme.fg;
        rayon::spawn(move || {
            let result = match job {
                Job::Heading(spec) => raster::heading(&spec, fg, cell).map_err(|e| e.to_string()),
                Job::Diagram(flat, cols) => raster::diagram(&flat.svg, flat.w, flat.h, cell, cols),
            };
            let _ = tx.send(Done { key, result });
        });
    }

    /// Returns true when the layout needs to change.
    fn on_done(&mut self, d: Done) -> bool {
        self.pending.remove(&d.key);
        if !self.wanted.contains(&d.key) {
            return false;
        }
        match d.result {
            Ok(r) => {
                if let Some(dir) = std::env::var_os("DOCP_DUMP_RASTER") {
                    let _ = std::fs::write(
                        std::path::Path::new(&dir)
                            .join(format!("{:x}-{}x{}.png", d.key, r.cols, r.rows)),
                        &r.png,
                    );
                }
                let id = kgp::alloc_id();
                kgp::transmit(&mut self.out, id, &r.png, r.cols, r.rows);
                self.media.insert(
                    d.key,
                    Media {
                        id,
                        cols: r.cols,
                        rows: r.rows,
                        placed: (r.cols, r.rows),
                    },
                );
            }
            Err(e) => self.message = Some(format!("render failed: {e}")),
        }
        true
    }

    // ---- editor sync ----

    /// First source line (1-based) of the block at the top of the viewport.
    fn top_line(&self) -> usize {
        let block = self.block_at(self.top.min(self.layout.len().saturating_sub(1)));
        srcmap::block_line(&self.doc.blocks, &self.lines, block)
    }

    fn on_ipc(&mut self, ev: ipc::Event, repaint: &mut bool) {
        match ev {
            ipc::Event::Connected => {
                if let Some(ipc) = &self.ipc {
                    ipc.send_ready();
                }
                // A new editor session has not agreed on a position yet.
                self.emitted = None;
                self.emit_at = None;
            }
            ipc::Event::Sync { line } => *repaint |= self.apply_sync(line),
            ipc::Event::Close | ipc::Event::Disconnected => {
                if matches!(ev, ipc::Event::Close) {
                    if let Some(ipc) = &self.ipc {
                        ipc.disconnect();
                    }
                }
                self.emitted = None;
                self.emit_at = None;
            }
        }
        // The IPC message may have changed the top; do not treat it as local.
        self.seen_top = self.top;
    }

    /// Jump to the block containing an editor line. Returns true on a change.
    fn apply_sync(&mut self, line: usize) -> bool {
        if self.doc.blocks.is_empty() {
            return false;
        }
        let block = srcmap::line_block(
            &self.doc.blocks,
            &self.lines,
            line.clamp(1, self.lines.line_count()),
        );
        let row = self
            .starts
            .get(block)
            .copied()
            .unwrap_or(0)
            .min(self.max_top());
        let changed = row != self.top;
        self.top = row;
        self.follow = false;
        // The editor drove this scroll; never echo it back.
        self.ipc_scroll = true;
        self.top_dirty = false;
        self.emit_at = None;
        changed
    }

    /// Send the top block's source line, throttled and deduplicated.
    fn flush_top(&mut self) {
        self.emit_at = None;
        if self.ipc.is_none() || self.ipc_scroll || !self.top_dirty {
            return;
        }
        let line = self.top_line();
        if self.emitted == Some(line) {
            self.top_dirty = false;
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_emit {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < EMIT_MIN {
                self.emit_at = Some(now + (EMIT_MIN - elapsed));
                return;
            }
        }
        let block = self.block_at(self.top.min(self.layout.len().saturating_sub(1)));
        if let Some(ipc) = &self.ipc {
            ipc.send_top(line, block + 1);
        }
        self.emitted = Some(line);
        self.last_emit = Some(now);
        self.top_dirty = false;
    }

    // ---- painting ----

    fn paint(&mut self, screen: &mut Screen) -> anyhow::Result<()> {
        let h = self.view_height();
        self.top = self.top.min(self.max_top());
        let width = self.content_width();
        let now = Instant::now();
        let mut rows = Vec::with_capacity(h);
        for y in 0..h {
            // Never inherit SGR from the previous row: a stale background
            // would be composited over any image on this row.
            let mut s = String::from("\x1b[0m");
            if let Some(row) = self.layout.get(self.top + y) {
                let block = self.block_at(self.top + y);
                let marked = self
                    .changed
                    .get(&block)
                    .is_some_and(|t| now.duration_since(*t) < MARK);
                if marked {
                    let mut a = String::new();
                    docmodel::sgr(
                        &mut a,
                        &Attrs {
                            fg: Some(self.theme.accent()),
                            ..Default::default()
                        },
                    );
                    s.push_str(&a);
                    s.push_str("▎ \x1b[0m");
                } else {
                    s.push_str("  ");
                }
                match row {
                    Row::Text(line) => line.encode_into(&mut s, width),
                    Row::Image { id, row, cols } => {
                        s.push_str(&kgp::placeholder_row(*id, *row, *cols))
                    }
                }
            }
            rows.push(s);
        }

        let total = self.layout.len().max(1);
        let pct = if self.layout.len() <= self.view_height() {
            100
        } else {
            (self.top + self.view_height()).min(total) * 100 / total
        };
        let mut right = format!("{pct}%");
        if self.follow {
            right.push_str(" · follow");
        }
        if !self.graphics {
            right.push_str(" · text only");
        }
        if let Some(l) = self.latency {
            right.push_str(&format!(" · {:.1}ms", l.as_secs_f64() * 1000.0));
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
        )?;

        let busy = !self.pending.is_empty()
            || self
                .diagrams
                .values()
                .any(|d| matches!(d, Diagram::Pending));
        if !busy && self.settle_until.is_none() {
            if let Some(t) = self.edit_started.take() {
                self.latency = Some(t.elapsed());
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        let mut out = Vec::new();
        for m in self.media.values() {
            kgp::delete(&mut out, m.id);
        }
        for block in self.img_blocks.values() {
            for m in &block.tiles {
                kgp::delete(&mut out, m.id);
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().write_all(&out);
    }
}

enum Kind {
    Heading(u8, String),
    Mermaid(String),
    Image(String),
    Text,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_image_paths() {
        // resolve() is exercised through the app in integration; the path
        // logic itself is covered by render-img.
        let s = "![x](a.png)";
        let d = doc::parse(s);
        assert!(matches!(&d.blocks[0].node, doc::Node::Image { url } if url == "a.png"));
    }
}
