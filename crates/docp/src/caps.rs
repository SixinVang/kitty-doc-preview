//! Terminal capability detection: winsize, cell pixels, graphics protocol,
//! Unicode placeholders, file transfer, and the terminal's color theme.

use anyhow::Result;
use docmodel::Theme;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Caps {
    pub rows: u16,
    pub cols: u16,
    /// Cell size (w, h) in pixels, best effort.
    pub cell: (u16, u16),
    pub graphics: bool,
    #[allow(dead_code)]
    pub placeholders: bool,
    #[allow(dead_code)]
    pub file_transfer: bool,
    /// Colors for the `terminal` theme (queried, or dark defaults).
    pub terminal_theme: Theme,
}

/// Detect capabilities. Never fails hard: without a controlling terminal
/// (pipes, CI) it returns plain-text defaults, and unknown protocol features
/// come back as `false` / defaults.
pub fn detect(timeout_ms: u64) -> Result<Caps> {
    let ws = termui::tty_winsize().unwrap_or(termui::Winsize {
        rows: 24,
        cols: 80,
        xpixel: 0,
        ypixel: 0,
    });

    let mut ioctl_cell = None;
    if ws.xpixel > 0 && ws.cols > 0 && ws.ypixel > 0 && ws.rows > 0 {
        ioctl_cell = Some(((ws.xpixel / ws.cols).max(1), (ws.ypixel / ws.rows).max(1)));
    }

    // One round trip: colors, cell size, graphics support, placeholders.
    let mut q = Vec::new();
    q.extend_from_slice(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for i in 0..16 {
        q.extend_from_slice(format!("\x1b]4;{i};?\x1b\\").as_bytes());
    }
    q.extend_from_slice(&kgp::probe_body());
    q.extend_from_slice(kgp::DA1);

    // No tty: plain-text defaults, no probing possible.
    let buf = match termui::query_tty(&q, termui::Stop::Da1, Duration::from_millis(timeout_ms)) {
        Ok(b) => b,
        Err(_) => {
            return Ok(Caps {
                rows: ws.rows.max(1),
                cols: ws.cols.max(1),
                cell: (8, 16),
                graphics: false,
                placeholders: false,
                file_transfer: false,
                terminal_theme: Theme::dark_default(),
            })
        }
    };
    if std::env::var_os("DOCP_DEBUG_PROBE").is_some() {
        eprintln!(
            "probe ascii: {}",
            String::from_utf8_lossy(&buf).escape_debug()
        );
    }
    let p = kgp::parse_probe(&buf, None);
    let cell = ioctl_cell.or(p.cell).unwrap_or((8, 16));
    // Feature query inconclusive (e.g. Rex EINVALs on it) -> assume the
    // terminal follows the protocol baseline it already confirmed.
    let placeholders = p.placeholders.unwrap_or(p.graphics);

    // Base the theme on the terminal's light/dark preference, then overlay
    // whatever colors it actually reported. Querying colors alone is not
    // enough: a terminal can stay silent on OSC 10/11 and we must not fall
    // back to a dark palette in a light window.
    let base = match p.appearance {
        Some(kgp::Appearance::Light) => Theme::light_default(),
        Some(kgp::Appearance::Dark) => Theme::dark_default(),
        None => match std::env::var("COLORFGBG")
            .ok()
            .as_deref()
            .and_then(parse_colorfgbg)
        {
            Some(true) => Theme::light_default(),
            Some(false) => Theme::dark_default(),
            None => Theme::dark_default(),
        },
    };
    let terminal_theme = Theme::from_osc(&buf, base);

    let mut file_transfer = false;
    if p.graphics {
        file_transfer = probe_file_transfer();
    }

    Ok(Caps {
        rows: ws.rows.max(1),
        cols: ws.cols.max(1),
        cell,
        graphics: p.graphics,
        placeholders,
        file_transfer,
        terminal_theme,
    })
}

/// `COLORFGBG` is `fg;bg` (or `fg;...;bg`) with ANSI palette indices; a high
/// background index means a light terminal. Returns `Some(is_light)`.
fn parse_colorfgbg(v: &str) -> Option<bool> {
    let bg: u16 = v.rsplit(';').next()?.trim().parse().ok()?;
    Some(bg >= 7)
}

/// `t=f` probe: a real regular file with real PNG bytes.
fn probe_file_transfer() -> bool {
    let png = render_img::tiny_png();
    let path = std::env::temp_dir().join(format!("docp-probe-{}.png", std::process::id()));
    if std::fs::write(&path, &png).is_err() {
        return false;
    }
    let q = kgp::file_transfer_query(33, png.len(), &path.to_string_lossy());
    let ok = termui::query_tty(&q, termui::Stop::Graphics(33), Duration::from_millis(150))
        .map(|buf| kgp::parse_probe(&buf, Some(33)).file_transfer)
        .unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    ok
}

/// Reload rows/cols (e.g. after a resize event).
pub fn refresh_size(caps: &mut Caps) {
    if let Ok(ws) = termui::tty_winsize() {
        caps.rows = ws.rows.max(1);
        caps.cols = ws.cols.max(1);
        if ws.xpixel > 0 && ws.cols > 0 && ws.ypixel > 0 && ws.rows > 0 {
            caps.cell = ((ws.xpixel / ws.cols).max(1), (ws.ypixel / ws.rows).max(1));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Markdown,
    Pdf,
}

pub fn kind(path: &std::path::Path) -> Kind {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("md" | "markdown" | "mdx" | "mdown" | "mkd") => Kind::Markdown,
        Some("pdf") => Kind::Pdf,
        _ => Kind::Image,
    }
}
