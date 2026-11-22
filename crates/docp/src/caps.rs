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

    let ioctl_cell = ioctl_cell(&ws);

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
    let cell = pick_cell(p.cell, ioctl_cell).unwrap_or((8, 16));
    trace_caps(&format!(
        "cell csi={:?} ioctl={:?} -> {cell:?} winsize={}x{}{}",
        p.cell,
        ioctl_cell,
        ws.cols,
        ws.rows,
        if ws.xpixel > 0 {
            format!(" {}x{}px", ws.xpixel, ws.ypixel)
        } else {
            String::new()
        }
    ));
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
    let base_name = base.name;
    let terminal_theme = Theme::from_osc(&buf, base);
    if let Some(path) = std::env::var_os("DOCP_TRACE") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                f,
                "appearance={:?} base={} -> fg={:?} bg={:?} palette0={:?}",
                p.appearance,
                base_name,
                terminal_theme.fg,
                terminal_theme.bg,
                terminal_theme.palette[0]
            );
        }
    }

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

/// Query just the cell size (`CSI 16t`), terminated by DA1.
pub fn probe_cell(timeout_ms: u64) -> Option<(u16, u16)> {
    let mut q = Vec::new();
    q.extend_from_slice(b"\x1b[16t");
    q.extend_from_slice(kgp::DA1);
    let buf = termui::query_tty(&q, termui::Stop::Da1, Duration::from_millis(timeout_ms)).ok()?;
    pick_cell(
        kgp::parse_probe(&buf, None).cell,
        ioctl_cell(&termui::tty_winsize().ok()?),
    )
}

fn ioctl_cell(ws: &termui::Winsize) -> Option<(u16, u16)> {
    if ws.xpixel > 0 && ws.cols > 0 && ws.ypixel > 0 && ws.rows > 0 {
        Some(((ws.xpixel / ws.cols).max(1), (ws.ypixel / ws.rows).max(1)))
    } else {
        None
    }
}

/// Pixel size of one cell, in the terminal's device pixels.
///
/// `CSI 16t` answers in logical pixels on a HiDPI screen (Rex reports 8x16
/// for a 14x32 cell), while `TIOCGWINSZ` reports the pane in device pixels.
/// Raster content has to be drawn in device pixels: otherwise the terminal
/// scales every heading and diagram up, non-uniformly when the two axes
/// differ, which is what makes pixel text look soft and slightly squashed.
///
/// The ioctl is only trusted when its cell is plausible and close to the
/// reported one, because some terminals fill the pixel fields with the whole
/// window instead of the pane.
fn pick_cell(csi: Option<(u16, u16)>, ioctl: Option<(u16, u16)>) -> Option<(u16, u16)> {
    let plausible =
        |c: (u16, u16)| c.0 > 0 && c.1 > 0 && (1.2..=3.2).contains(&(c.1 as f32 / c.0 as f32));
    match (csi, ioctl) {
        (Some(c), Some(i)) if plausible(c) && plausible(i) => {
            let rx = i.0 as f32 / c.0 as f32;
            let ry = i.1 as f32 / c.1 as f32;
            let close = (1.0..=4.0).contains(&rx)
                && (1.0..=4.0).contains(&ry)
                && (rx / ry - 1.0).abs() <= 0.35;
            if close {
                trace_caps(&format!(
                    "cell scale {rx:.2}x{ry:.2} (csi {c:?}, ioctl {i:?}) -> device {i:?}"
                ));
                Some(i)
            } else {
                trace_caps(&format!(
                    "cell ioctl {i:?} disagrees with csi {c:?}; using csi"
                ));
                Some(c)
            }
        }
        (Some(c), _) => Some(c),
        (None, Some(i)) => Some(i),
        (None, None) => None,
    }
}

/// Wait for the pane to be laid out, then re-read its size and cell.
///
/// Rex creates a new pane at 80x24 with a placeholder cell size and resizes it
/// a few milliseconds later. Probing before that settle leaves the view with a
/// cell size that does not match the pixels the terminal actually uses, and
/// image placements then get letterboxed (a tile looks `c*r` cells but its
/// pixels have the wrong aspect, so the terminal leaves bands of background).
/// The cell is taken in device pixels (see `pick_cell`), so a font-size change
/// also re-derives the resolution the raster content must be drawn at.
pub fn settle(caps: &mut Caps) {
    let key = |ws: &termui::Winsize| (ws.cols.max(1), ws.rows.max(1));
    let mut last = termui::tty_winsize().ok().map(|w| key(&w));
    let started = std::time::Instant::now();
    let mut stable = std::time::Instant::now();
    while started.elapsed() < Duration::from_millis(250) {
        std::thread::sleep(Duration::from_millis(10));
        let now = termui::tty_winsize().ok().map(|w| key(&w));
        if now != last {
            last = now;
            stable = std::time::Instant::now();
        } else if stable.elapsed() >= Duration::from_millis(30) {
            break;
        }
    }
    if let Ok(ws) = termui::tty_winsize() {
        caps.rows = ws.rows.max(1);
        caps.cols = ws.cols.max(1);
    }
    if let Some(cell) = probe_cell(200) {
        trace_caps(&format!(
            "settled cell={cell:?} pane={}x{}",
            caps.cols, caps.rows
        ));
        caps.cell = cell;
    }
}

/// Reload rows/cols (e.g. after a resize event).
pub fn refresh_size(caps: &mut Caps) {
    if let Ok(ws) = termui::tty_winsize() {
        caps.rows = ws.rows.max(1);
        caps.cols = ws.cols.max(1);
        // Cell pixel size does not change on resize, and recomputing it from
        // the ioctl can pick up the window size instead of the pane's.
    }
}

fn trace_caps(msg: &str) {
    use std::io::Write as _;
    let Some(path) = std::env::var_os("DOCP_TRACE") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hi_dpi_ioctl_wins_over_logical_csi() {
        // Rex: CSI 16t answers in logical pixels (8x16), the pty's winsize in
        // device pixels (14x32).
        assert_eq!(pick_cell(Some((8, 16)), Some((14, 32))), Some((14, 32)));
        assert_eq!(pick_cell(Some((7, 16)), Some((14, 32))), Some((14, 32)));
        // Same cell size on both: nothing to scale.
        assert_eq!(pick_cell(Some((8, 16)), Some((8, 16))), Some((8, 16)));
        assert_eq!(pick_cell(Some((8, 16)), Some((16, 32))), Some((16, 32)));
    }

    #[test]
    fn implausible_ioctl_does_not_win() {
        // Whole-window pixels divided by pane columns: not a cell shape.
        assert_eq!(pick_cell(Some((8, 16)), Some((160, 90))), Some((8, 16)));
        // Device cell more than 4x the reported one is not a scale factor.
        assert_eq!(pick_cell(Some((8, 16)), Some((40, 80))), Some((8, 16)));
        // x and y disagree by more than 35%: not a uniform scale.
        assert_eq!(pick_cell(Some((8, 16)), Some((8, 48))), Some((8, 16)));
    }

    #[test]
    fn single_source_is_used_as_is() {
        assert_eq!(pick_cell(Some((9, 18)), None), Some((9, 18)));
        assert_eq!(pick_cell(None, Some((9, 18))), Some((9, 18)));
        assert_eq!(pick_cell(None, None), None);
    }
}
