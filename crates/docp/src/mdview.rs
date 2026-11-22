//! Markdown view: hybrid rendering (text + pixels), live reload, anchored
//! scrolling, block-level diffing.

use crate::caps::Caps;
use crate::input::Input;
use crate::screen::{Frame, Screen};
use crate::{clipboard, ipc, links, srcmap};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use docmodel::{Attrs, Cell, Line, Rgb, Row, Theme};
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

/// A character position in the laid-out document: layout row (not screen row)
/// and display column inside the text area (left gutter excluded).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pos {
    row: usize,
    col: usize,
}

/// Mouse text selection. Rows are absolute layout rows, so scrolling keeps
/// the highlight attached to the text.
#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: Pos,
    cursor: Pos,
    dragging: bool,
}

impl Selection {
    /// Anchor and cursor in document order.
    fn ordered(&self) -> (Pos, Pos) {
        if (self.anchor.row, self.anchor.col) <= (self.cursor.row, self.cursor.col) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Selected display-column range `[from, to)` on `row`, clamped to
    /// `width`. The cell under the cursor is included.
    fn range_on(&self, row: usize, width: usize) -> Option<(usize, usize)> {
        let (a, b) = self.ordered();
        if row < a.row || row > b.row {
            return None;
        }
        let from = if row == a.row { a.col } else { 0 };
        let to = if row == b.row { b.col + 1 } else { width };
        let to = to.min(width);
        (to > from).then_some((from, to))
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

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
    /// When a transient status message ("copied …") expires.
    message_until: Option<Instant>,

    /// Mouse text selection in layout coordinates.
    selection: Option<Selection>,
    /// Last mouse position, so `o` can follow the link under the pointer.
    mouse: Option<Pos>,

    /// Where link navigation came from: (path, top) pairs, newest last.
    history: Vec<(PathBuf, usize)>,
    /// The file `--ipc` was started for; editor sync only applies to it.
    ipc_root: Option<PathBuf>,

    /// Filesystem watcher, retargeted when a link opens another file.
    watcher: notify::RecommendedWatcher,
    watched_dir: PathBuf,

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
    // The embedded syntax dump is ~1.4 MB compressed; decompress it off the
    // main thread so the first visible code block does not stall frame one.
    std::thread::spawn(render_md::highlight::preload);
    let path = std::fs::canonicalize(path)?;
    render_md::fonts::warm_system();
    let (pele_tx, pele_rx) = unbounded();
    let pele = pele::Pele::spawn(pele_tx);

    let (done_tx, done_rx) = unbounded();
    let (fs_tx, fs_rx) = unbounded::<Vec<PathBuf>>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = fs_tx.send(ev.paths);
        }
    })?;
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
        message_until: None,
        selection: None,
        mouse: None,
        history: Vec::new(),
        ipc_root: Some(path.clone()),
        watcher,
        watched_dir: PathBuf::new(),
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

    app.watch_dir(path.parent());

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
        fs: Receiver<Vec<PathBuf>>,
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
                self.message_until,
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
                recv(fs) -> ev => if let Ok(paths) = ev {
                    if self.touched(&paths) {
                        reload_at = Some(Instant::now() + DEBOUNCE);
                        self.edit_started.get_or_insert_with(Instant::now);
                    }
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
            while let Ok(paths) = fs.try_recv() {
                if self.touched(&paths) {
                    reload_at = Some(Instant::now() + DEBOUNCE);
                }
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
            if self.message_until.is_some_and(|t| t <= now) {
                self.message_until = None;
                self.message = None;
                repaint = true;
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
            Event::Key(k) if k.kind != KeyEventKind::Release => self.key(screen, k, repaint),
            Event::Mouse(m) => {
                match m.kind {
                    MouseEventKind::ScrollDown => self.scroll(3, repaint),
                    MouseEventKind::ScrollUp => self.scroll(-3, repaint),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if m.row as usize >= self.view_height() {
                            return false; // status bar
                        }
                        let pos = self.pos_at(m.row, m.column);
                        self.mouse = Some(pos);
                        self.selection = Some(Selection {
                            anchor: pos,
                            cursor: pos,
                            dragging: true,
                        });
                        *repaint = true;
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        let pos = self.pos_at(m.row, m.column);
                        self.mouse = Some(pos);
                        if let Some(sel) = &mut self.selection {
                            if sel.cursor != pos {
                                sel.cursor = pos;
                                *repaint = true;
                            }
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if m.row as usize >= self.view_height() {
                            return false; // status bar
                        }
                        let pos = self.pos_at(m.row, m.column);
                        self.mouse = Some(pos);
                        if let Some(mut sel) = self.selection {
                            if sel.dragging {
                                sel.dragging = false;
                                if sel.is_empty() {
                                    self.selection = None;
                                    // A click (not a drag) on a link follows it.
                                    self.follow_at(screen, pos, repaint);
                                } else {
                                    self.selection = Some(sel);
                                    // Releasing the drag is the copy gesture;
                                    // `y` copies again after scrolling.
                                    self.copy_selection();
                                }
                                *repaint = true;
                            } else {
                                self.selection = Some(sel);
                            }
                        }
                    }
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
                self.selection = None;
                *relayout = true;
                false
            }
            _ => false,
        }
    }

    fn key(&mut self, screen: &mut Screen, k: KeyEvent, repaint: &mut bool) -> bool {
        let page = self.view_height() as isize;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc if self.selection.is_some() => {
                self.selection = None;
                *repaint = true;
            }
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('y') => {
                self.copy_selection();
                *repaint = true;
            }
            KeyCode::Char('o') => {
                let pos = self.selection.map(|s| s.anchor).or(self.mouse);
                let followed = pos.is_some_and(|pos| self.follow_at(screen, pos, repaint));
                if !followed {
                    self.flash("no link under the cursor".into());
                    *repaint = true;
                }
            }
            KeyCode::Backspace => self.back(screen, repaint),
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

    // ---- selection ----

    /// Map a mouse cell to a document position. The gutter is not selectable
    /// and positions past the text column clamp to the content width.
    fn pos_at(&self, screen_row: u16, screen_col: u16) -> Pos {
        let screen_row = (screen_row as usize).min(self.view_height().saturating_sub(1));
        let row = (self.top + screen_row).min(self.layout.len().saturating_sub(1));
        let col = (screen_col as usize)
            .saturating_sub(GUTTER)
            .min(self.content_width());
        Pos { row, col }
    }

    fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let (a, b) = sel.ordered();
        let width = self.content_width();
        let mut lines = Vec::new();
        for row in a.row..=b.row {
            let Some((from, to)) = sel.range_on(row, width) else {
                continue;
            };
            let text = match self.layout.get(row) {
                Some(Row::Text(line)) => text_between(line, from, to),
                // Images and placeholders contribute no text.
                _ => String::new(),
            };
            lines.push(text.trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return None;
        }
        Some(lines.join("\n"))
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        clipboard::copy(&text, &mut self.out);
        let chars = text.chars().count();
        self.message = Some(format!("copied {chars} chars"));
        self.message_until = Some(Instant::now() + Duration::from_millis(2000));
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

    /// Watch the directory holding the current document.
    fn watch_dir(&mut self, dir: Option<&Path>) {
        let dir = dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        if self.watched_dir == dir {
            return;
        }
        if !self.watched_dir.as_os_str().is_empty() {
            let _ = self.watcher.unwatch(&self.watched_dir);
        }
        if self
            .watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .is_ok()
        {
            self.watched_dir = dir;
        }
    }

    /// Whether a filesystem event concerns the document on screen.
    fn touched(&self, paths: &[PathBuf]) -> bool {
        paths.iter().any(|p| {
            p == &self.path || (p.file_name().is_some() && p.file_name() == self.path.file_name())
        })
    }

    /// A transient status-line message.
    fn flash(&mut self, msg: String) {
        self.message = Some(msg);
        self.message_until = Some(Instant::now() + Duration::from_secs(3));
    }

    // ---- links ----

    /// The link of the cell under a document position, if any.
    fn link_at(&self, pos: Pos) -> Option<String> {
        let Row::Text(line) = self.layout.get(pos.row)? else {
            return None;
        };
        let mut col = 0usize;
        for cell in &line.cells {
            let width = docmodel::display_width(&cell.text);
            if pos.col >= col && pos.col < col + width {
                return cell.url.clone();
            }
            col += width;
        }
        None
    }

    /// Follow the link under `pos`; returns false when there is none.
    fn follow_at(&mut self, screen: &mut Screen, pos: Pos, repaint: &mut bool) -> bool {
        let Some(dest) = self.link_at(pos) else {
            return false;
        };
        self.follow(screen, &dest, repaint);
        true
    }

    /// Follow a link destination: an in-document anchor, another Markdown
    /// file (navigates in place) or anything else (handed to the OS).
    fn follow(&mut self, screen: &mut Screen, dest: &str, repaint: &mut bool) {
        *repaint = true;
        let base = self.path.parent().map(Path::to_path_buf);
        match links::classify(dest, base.as_deref()) {
            links::Target::Anchor(anchor) => {
                if anchor.is_empty() {
                    self.top = 0;
                    self.follow = false;
                } else if !self.jump_anchor(&anchor) {
                    self.flash(format!("no heading #{anchor}"));
                }
            }
            links::Target::External(url) => match links::open_external(&url) {
                Ok(()) => self.flash(format!("opened {url}")),
                Err(e) => self.flash(format!("can't open {url}: {e}")),
            },
            links::Target::File { path, anchor } => {
                if !path.exists() {
                    self.flash(format!("no such file: {}", path.display()));
                    return;
                }
                if !links::is_markdown(&path) {
                    match links::open_external(&path.to_string_lossy()) {
                        Ok(()) => self.flash(format!("opened {}", path.display())),
                        Err(e) => self.flash(format!("can't open {}: {e}", path.display())),
                    }
                    return;
                }
                if path != self.path {
                    self.history.push((self.path.clone(), self.top));
                }
                self.open_path(screen, path, anchor, None);
            }
        }
    }

    /// Scroll to the heading whose anchor (or literal title) is `frag`.
    fn jump_anchor(&mut self, frag: &str) -> bool {
        let hit = self.doc.blocks.iter().enumerate().find_map(|(i, b)| {
            let doc::Node::Heading { anchor, spans, .. } = &b.node else {
                return None;
            };
            anchor_matches(anchor, &doc::plain(spans), frag).then_some(i)
        });
        let Some(i) = hit else {
            return false;
        };
        let start = self.starts.get(i).copied().unwrap_or(0);
        self.top = start.min(self.max_top());
        self.follow = false;
        true
    }

    /// Open a Markdown file in this pane, optionally at `anchor` or `top`.
    fn open_path(
        &mut self,
        screen: &mut Screen,
        path: PathBuf,
        anchor: Option<String>,
        top: Option<usize>,
    ) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if path == self.path {
            if let Some(a) = anchor {
                if !self.jump_anchor(&a) {
                    self.flash(format!("no heading #{a}"));
                }
            } else if let Some(top) = top {
                self.top = top.min(self.max_top());
            }
            return;
        }
        // Free the previous document's pixel state before reusing the pane.
        for m in self.media.values() {
            kgp::delete(&mut self.out, m.id);
        }
        for block in self.img_blocks.values() {
            for m in &block.tiles {
                kgp::delete(&mut self.out, m.id);
            }
        }
        self.media.clear();
        self.img_blocks.clear();
        self.diagrams.clear();
        self.pele_tags.clear();
        self.pending.clear();
        self.wanted.clear();
        self.stale.clear();
        self.text_cache.clear();
        self.changed.clear();
        self.layout.clear();
        self.starts.clear();
        self.doc = doc::Doc { blocks: Vec::new() };
        self.selection = None;
        self.mouse = None;
        self.follow = false;
        self.top = 0;
        self.path = path.clone();
        self.name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.src_hash = None;
        self.watch_dir(path.parent());
        self.reload(screen);
        if let Some(top) = top {
            self.top = top.min(self.max_top());
        }
        if let Some(a) = anchor {
            if !self.jump_anchor(&a) {
                self.flash(format!("no heading #{a}"));
            }
        }
        // The editor still edits the original file; do not report this one.
        self.seen_top = self.top;
        self.top_dirty = false;
    }

    fn back(&mut self, screen: &mut Screen, repaint: &mut bool) {
        *repaint = true;
        match self.history.pop() {
            Some((path, top)) => self.open_path(screen, path, None, Some(top)),
            None => self.flash("no previous document".into()),
        }
    }

    /// Rewrite link cells for both the terminal and navigation: anchors stay
    /// as `#fragment`, relative paths become absolute `file:` URLs.
    fn localize_links(&self, lines: &mut [Line]) {
        let base = self.path.parent();
        for line in lines {
            for cell in &mut line.cells {
                if let Some(url) = cell.url.take() {
                    cell.url = links::localize(&url, base);
                }
            }
        }
    }

    fn reload(&mut self, screen: &mut Screen) {
        let src = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) => {
                self.message = Some(format!("can't read: {e}"));
                self.message_until = None;
                self.edit_started = None;
                let _ = self.paint(screen);
                return;
            }
        };
        self.message = None;
        self.message_until = None;
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
        self.selection = None;
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
            doc::Node::Heading { level, spans, .. } if *level <= 2 && self.graphics => {
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
        if links::is_external(url) {
            return None;
        }
        let (target, _) = links::split(url);
        let path = links::resolve(target, self.path.parent());
        path.exists().then_some(path)
    }

    fn text_rows(&mut self, i: usize, width: usize, used: &mut HashSet<(u64, usize)>) -> Vec<Row> {
        let block = &self.doc.blocks[i];
        let key = (block.hash, width);
        used.insert(key);
        let lines = match self.text_cache.get(&key) {
            Some(l) => l.clone(),
            None => {
                let ctx = text::Ctx { theme: &self.theme };
                let mut l = ctx.node(&block.node, width);
                // Resolve links once, against this document's directory; the
                // cache is cleared when a link opens another file.
                self.localize_links(&mut l);
                let l = Arc::new(l);
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
            Err(e) => {
                self.message = Some(format!("render failed: {e}"));
                self.message_until = None;
            }
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
            ipc::Event::Sync { line } => {
                if self.ipc_root.as_ref() == Some(&self.path) {
                    *repaint |= self.apply_sync(line);
                }
            }
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
        // The editor edits `ipc_root`; after following a link elsewhere,
        // reporting this document's lines would only confuse it.
        if self.ipc_root.as_ref() != Some(&self.path) {
            self.top_dirty = false;
            return;
        }
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
        let selection = self.selection;
        let sel_bg = Theme::mix(self.theme.bg, self.theme.accent(), 0.30);
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
                    Row::Text(line) => {
                        match selection.and_then(|s| s.range_on(self.top + y, width)) {
                            Some((from, to)) => selection_line(line, from, to, width, sel_bg)
                                .encode_into(&mut s, width),
                            None => line.encode_into(&mut s, width),
                        }
                    }
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

/// Whether a heading (already slugified `anchor` plus its plain title)
/// matches a link fragment. GitHub strips a `user-content-` prefix on
/// duplicate anchors, and hand-written anchors often use the raw title.
fn anchor_matches(anchor: &str, title: &str, frag: &str) -> bool {
    let raw = frag.trim();
    let raw = raw.strip_prefix("user-content-").unwrap_or(raw);
    let slug = doc::slugify(raw);
    anchor.eq_ignore_ascii_case(raw)
        || (!slug.is_empty() && anchor == slug)
        || title.trim().eq_ignore_ascii_case(raw)
}

/// Text in display columns `[from, to)` of `line`.
fn text_between(line: &Line, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for cell in &line.cells {
        for ch in cell.text.chars() {
            let start = col;
            col += docmodel::char_cols(ch);
            if start >= from && start < to {
                out.push(ch);
            }
        }
    }
    out
}

/// `line` with display columns `[from, to)` painted in `bg`. Trailing blank
/// cells inside the range are included so multi-line selections read as one
/// block instead of ragged text lines.
fn selection_line(line: &Line, from: usize, to: usize, width: usize, bg: Rgb) -> Line {
    let mut cells: Vec<Cell> = Vec::new();
    let mut col = 0usize;
    for cell in &line.cells {
        let mut part = String::new();
        let mut part_sel = false;
        for ch in cell.text.chars() {
            let width = docmodel::char_cols(ch);
            let sel = col >= from && col < to;
            if part.is_empty() {
                part_sel = sel;
            } else if sel != part_sel {
                cells.push(tinted(std::mem::take(&mut part), cell, part_sel, bg));
                part_sel = sel;
            }
            part.push(ch);
            col += width;
        }
        if !part.is_empty() {
            cells.push(tinted(part, cell, part_sel, bg));
        }
    }
    let pad_end = to.min(width);
    let pad_start = col.max(from);
    if pad_end > pad_start {
        cells.push(Cell {
            text: " ".repeat(pad_end - pad_start),
            attrs: Attrs {
                bg: Some(bg),
                ..Default::default()
            },
            url: None,
        });
    }
    Line {
        cells,
        fill: line.fill,
    }
}

fn tinted(text: String, cell: &Cell, selected: bool, bg: Rgb) -> Cell {
    let mut attrs = cell.attrs;
    if selected {
        attrs.bg = Some(bg);
    }
    Cell {
        text,
        attrs,
        url: cell.url.clone(),
    }
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

    fn line(text: &str) -> Line {
        let mut l = Line::default();
        l.cells.push(Cell::new(text, Attrs::default()));
        l
    }

    #[test]
    fn anchors_match_slugs_titles_and_user_content_prefixes() {
        assert!(anchor_matches("install", "Install", "install"));
        assert!(anchor_matches("install", "Install", "Install"));
        assert!(anchor_matches("install", "Install", "user-content-install"));
        assert!(anchor_matches("install", "Install", "Install "));
        assert!(anchor_matches("54-编辑器", "5.4 编辑器", "5.4 编辑器"));
        assert!(!anchor_matches("install", "Install", "installing"));
    }

    #[test]
    fn link_at_finds_the_cell_under_a_column() {
        let mut line = line("see ");
        line.cells.push(Cell {
            text: "docs".into(),
            attrs: Attrs::default(),
            url: Some("file:///tmp/docs.md".into()),
        });
        let mut app = test_app();
        app.layout = vec![Row::Text(line)];
        assert_eq!(
            app.link_at(Pos { row: 0, col: 4 }).as_deref(),
            Some("file:///tmp/docs.md")
        );
        assert_eq!(app.link_at(Pos { row: 0, col: 2 }), None);
        assert_eq!(app.link_at(Pos { row: 0, col: 20 }), None);
        assert_eq!(app.link_at(Pos { row: 5, col: 0 }), None);
    }

    #[test]
    fn internal_anchors_stay_out_of_osc8() {
        let app = test_app();
        let mut lines = vec![line("a")];
        lines[0].cells[0].url = Some("#install".into());
        app.localize_links(&mut lines);
        assert_eq!(lines[0].cells[0].url.as_deref(), Some("#install"));
        let mut s = String::new();
        lines[0].encode_into(&mut s, 10);
        assert!(!s.contains("\x1b]8;;#"), "{s:?}");
    }

    #[test]
    fn relative_links_become_absolute_file_urls() {
        let mut app = test_app();
        app.path = PathBuf::from("/tmp/docs/notes.md");
        let mut lines = vec![line("a")];
        lines[0].cells[0].url = Some("../ARCH.md#54-rex".into());
        app.localize_links(&mut lines);
        let url = lines[0].cells[0].url.clone().unwrap();
        assert!(url.starts_with("file:///tmp/"), "{url}");
        assert!(url.ends_with("ARCH.md#54-rex"), "{url}");
        assert!(url.contains("/tmp/ARCH.md"), "{url}");
    }

    fn test_app() -> App {
        let dir = std::env::temp_dir();
        let (fs_tx, _fs_rx) = unbounded::<Vec<PathBuf>>();
        let watcher = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
            let _ = fs_tx.send(Vec::new());
        })
        .unwrap();
        let (done_tx, _done_rx) = unbounded();
        let (pele_tx, _pele_rx) = unbounded();
        App {
            name: "notes.md".into(),
            path: dir.join("notes.md"),
            src_hash: None,
            doc: doc::Doc { blocks: Vec::new() },
            theme: Theme::dark_default(),
            graphics: false,
            cell: PixelCell { w: 8, h: 16 },
            cols: 80,
            rows: 24,
            max_width: 110,
            top: 0,
            follow: false,
            layout: Vec::new(),
            starts: Vec::new(),
            text_cache: HashMap::new(),
            pele: pele::Pele::spawn(pele_tx),
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
            edit_started: None,
            latency: None,
            message: None,
            message_until: None,
            selection: None,
            mouse: None,
            history: Vec::new(),
            ipc_root: None,
            watcher,
            watched_dir: PathBuf::new(),
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
        }
    }

    #[test]
    fn selection_range_includes_cursor_cell_and_clamps() {
        let sel = Selection {
            anchor: Pos { row: 2, col: 3 },
            cursor: Pos { row: 2, col: 5 },
            dragging: false,
        };
        assert_eq!(sel.range_on(2, 80), Some((3, 6)));
        assert_eq!(sel.range_on(1, 80), None);
        let sel = Selection {
            anchor: Pos { row: 1, col: 0 },
            cursor: Pos { row: 3, col: 200 },
            dragging: false,
        };
        assert_eq!(sel.range_on(2, 40), Some((0, 40)));
        assert_eq!(sel.range_on(3, 40), Some((0, 40)));
    }

    #[test]
    fn selection_text_clips_display_columns() {
        let l = line("ab中文cd");
        // Columns: a0 b1 中2-3 文4-5 c6 d7
        assert_eq!(text_between(&l, 1, 4), "b中");
        assert_eq!(text_between(&l, 2, 5), "中文");
        assert_eq!(text_between(&l, 6, 20), "cd");
    }

    #[test]
    fn selection_line_tints_only_selected_columns() {
        let bg = Rgb(10, 20, 30);
        let out = selection_line(&line("abcd"), 1, 3, 80, bg);
        let texts: Vec<&str> = out.cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "bc", "d"]);
        assert_eq!(out.cells[1].attrs.bg, Some(bg));
        assert_eq!(out.cells[0].attrs.bg, None);
    }

    #[test]
    fn selection_line_pads_blank_cells_to_the_cursor() {
        let bg = Rgb(10, 20, 30);
        let out = selection_line(&line("ab"), 0, 6, 80, bg);
        assert_eq!(out.width(), 6);
        assert_eq!(out.cells.last().unwrap().text, "    ");
        assert_eq!(out.cells.last().unwrap().attrs.bg, Some(bg));
    }
}
