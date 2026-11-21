//! Non-interactive output: markdown as styled text (images noted inline),
//! images as kitty graphics with fixed width, PDFs not yet.

use crate::caps::{Caps, Kind};
use anyhow::Result;
use docmodel::Theme;
use render_md::{doc, text};
use std::io::{IsTerminal, Write};
use std::path::Path;

const DEFAULT_COLS: usize = 80;

pub fn run(file: &Path, kind: Kind, caps: &Caps, theme: &Theme, max_width: usize) -> Result<()> {
    match kind {
        Kind::Markdown => markdown(file, caps, theme, max_width),
        Kind::Image => image(file, caps),
        Kind::Pdf => anyhow::bail!("pdf text output needs the text layer (M3)"),
    }
}

fn markdown(file: &Path, caps: &Caps, theme: &Theme, max_width: usize) -> Result<()> {
    let src = std::fs::read_to_string(file)?;
    // Honour the configured clamp whether or not stdout is a terminal.
    let avail = if std::io::stdout().is_terminal() {
        caps.cols as usize
    } else {
        DEFAULT_COLS
    };
    let width = avail.clamp(20, max_width.max(20));
    let out = markdown_text(&src, theme, width);
    let mut o = std::io::stdout().lock();
    o.write_all(out.as_bytes())?;
    o.flush()?;
    Ok(())
}

/// Render Markdown to styled terminal text. Kept separate from I/O so the
/// code-block highlighting path (including SGR output) is unit-testable.
pub(crate) fn markdown_text(src: &str, theme: &Theme, width: usize) -> String {
    let d = doc::parse(src);
    let ctx = text::Ctx { theme };
    let mut out = String::new();
    for (i, block) in d.blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for line in ctx.node(&block.node, width) {
            let row = line.render(width);
            out.push_str(row.trim_end());
            out.push('\n');
        }
    }
    out
}

fn image(file: &Path, caps: &Caps) -> Result<()> {
    if !caps.graphics {
        eprintln!("docp: terminal lacks kitty graphics support; cannot print image");
        return Ok(());
    }
    let img = render_img::load(file)?;
    let (cw, ch) = (caps.cell.0.max(1) as u32, caps.cell.1.max(1) as u32);
    let lay = render_img::layout(&img, (cw, ch), caps.cols)?;
    let mut buf = Vec::new();
    for t in &lay.tiles {
        let id = kgp::alloc_id();
        kgp::transmit(&mut buf, id, &t.png, t.cols, t.rows);
        for row in 0..t.rows {
            buf.extend_from_slice(kgp::placeholder_row(id, row, t.cols).as_bytes());
            buf.push(b'\n');
        }
    }
    let mut o = std::io::stdout().lock();
    o.write_all(&buf)?;
    o.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_code_emits_syntax_colors() {
        let theme = Theme::dark_default();
        let out = markdown_text("```rust\nfn main() { let x = 1; }\n```\n", &theme, 80);
        assert!(out.contains("38;2;"), "no foreground color: {out:?}");
    }

    #[test]
    fn frontmatter_renders_as_a_card_not_raw_dashes() {
        let theme = Theme::dark_default();
        let out = markdown_text("---\ntitle: Hello\ntags: [a, b]\n---\n\nbody\n", &theme, 80);
        assert!(out.contains("frontmatter"), "no card: {out:?}");
        assert!(out.contains("title"), "missing key: {out:?}");
        // Adjacent words are separate cells, so SGR codes sit inside values.
        assert!(out.contains("Hello"), "missing value: {out:?}");
        assert!(out.contains(" b"), "missing list item: {out:?}");
        assert!(!out.contains("---"), "raw delimiters leaked: {out:?}");
        assert!(out.contains("body"), "body lost: {out:?}");
    }

    #[test]
    fn plain_markdown_stays_uncolored() {
        let theme = Theme::dark_default();
        let out = markdown_text("hello world\n", &theme, 80);
        assert!(!out.contains("38;2;"), "unexpected color: {out:?}");
    }
}
