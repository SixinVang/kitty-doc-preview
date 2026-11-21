//! Terminal capability detection: winsize, cell pixels, graphics protocol,
//! Unicode placeholders, file transfer.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Caps {
    pub rows: u16,
    pub cols: u16,
    /// Cell size (w, h) in pixels, best effort.
    pub cell: (u16, u16),
    pub graphics: bool,
    pub placeholders: bool,
    pub file_transfer: bool,
}

/// Detect capabilities. Never fails hard on missing features: unknown
/// capabilities come back as `false` / defaults.
pub fn detect(timeout_ms: u64) -> Result<Caps> {
    let ws = termui::tty_winsize()?;

    let mut ioctl_cell = None;
    if ws.xpixel > 0 && ws.cols > 0 && ws.ypixel > 0 && ws.rows > 0 {
        ioctl_cell = Some(((ws.xpixel / ws.cols).max(1), (ws.ypixel / ws.rows).max(1)));
    }

    let buf = termui::query_tty(
        &kgp::probe_queries(),
        termui::Stop::Da1,
        Duration::from_millis(timeout_ms),
    )?;
    if std::env::var_os("DOCP_DEBUG_PROBE").is_some() {
        eprintln!("probe bytes: {buf:02x?}");
        eprintln!("probe ascii: {}", String::from_utf8_lossy(&buf).escape_debug());
    }
    let p = kgp::parse_probe(&buf, None);
    let cell = ioctl_cell.or(p.cell).unwrap_or((8, 16));
    // Feature query inconclusive (e.g. Rex EINVALs on it) -> assume the
    // terminal follows the protocol baseline it already confirmed.
    let placeholders = p.placeholders.unwrap_or(p.graphics);

    let mut file_transfer = false;
    if p.graphics {
        // t=f probe: a real regular file with real PNG bytes.
        let png = render_img::tiny_png();
        let path = std::env::temp_dir().join(format!("docp-probe-{}.png", std::process::id()));
        if std::fs::write(&path, &png).is_ok() {
            let q = kgp::file_transfer_query(33, png.len(), &path.to_string_lossy());
            if let Ok(buf2) =
                termui::query_tty(&q, termui::Stop::Graphics(33), Duration::from_millis(150))
            {
                file_transfer = kgp::parse_probe(&buf2, Some(33)).file_transfer;
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    Ok(Caps {
        rows: ws.rows.max(1),
        cols: ws.cols.max(1),
        cell,
        graphics: p.graphics,
        placeholders,
        file_transfer,
    })
}

/// Reload rows/cols (e.g. after a resize event).
pub fn refresh_size(caps: &mut Caps) {
    if let Ok(ws) = termui::tty_winsize() {
        caps.rows = ws.rows.max(1);
        caps.cols = ws.cols.max(1);
    }
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "tif"
                    | "tiff"
                    | "ico"
                    | "ff"
                    | "qoi"
                    | "tga"
                    | "dds"
                    | "hdr"
                    | "avif"
                    | "exr"
                    | "pnm"
                    | "pbm"
                    | "pgm"
                    | "ppm"
            )
        })
        .unwrap_or(false)
}
