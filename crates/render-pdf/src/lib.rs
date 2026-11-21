//! PDF backend: MuPDF via the `mupdf` crate.
//!
//! The renderer is deliberately band-based: `render_band` rasterises only a
//! horizontal slice of a page at a given scale and returns it as RGBA. The
//! document object is not `Sync`, so callers keep one `Pdf` on a single
//! thread and ship jobs to it over a channel.

use anyhow::{anyhow, bail, Context, Result};
use image::RgbaImage;
use mupdf::{Colorspace, Device, Document, Matrix, Page, Pixmap};
use std::path::Path;

/// Page sizes in PostScript points (1/72 inch), in page order.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub pages: usize,
    pub sizes: Vec<(f32, f32)>,
}

pub struct Pdf {
    doc: Document,
    meta: Meta,
    /// MuPDF re-parses a page on `load_page`; tiles of the same page are
    /// rendered back to back, so keep one page object around.
    current: Option<(usize, Page)>,
}

impl Pdf {
    pub fn open(path: &Path) -> Result<Pdf> {
        let name = path.to_string_lossy();
        let doc =
            Document::open(name.as_ref()).map_err(|e| anyhow!("open {}: {e}", path.display()))?;
        if doc.needs_password().unwrap_or(false) {
            bail!("encrypted PDF (password entry is not supported yet)");
        }
        let pages = doc.page_count().context("page count")?.max(0) as usize;
        if pages == 0 {
            bail!("document has no pages");
        }
        let mut sizes = Vec::with_capacity(pages);
        for i in 0..pages {
            let page = doc
                .load_page(i as i32)
                .with_context(|| format!("load page {}", i + 1))?;
            let b = page
                .bounds()
                .with_context(|| format!("bounds of page {}", i + 1))?;
            sizes.push((b.width().max(1.0), b.height().max(1.0)));
        }
        Ok(Pdf {
            doc,
            meta: Meta { pages, sizes },
            current: None,
        })
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Rasterise the device-pixel rectangle `[x0, x0 + w) x [y0, y0 + h)` of
    /// `page` at `scale` pixels per point. The rectangle is clipped by the
    /// pixmap, so MuPDF never rasterises the rest of the page.
    pub fn render_band(
        &mut self,
        page: usize,
        scale: f32,
        x0: u32,
        y0: u32,
        w: u32,
        h: u32,
    ) -> Result<RgbaImage> {
        if page >= self.meta.pages {
            bail!("page {} out of range", page + 1);
        }
        if w == 0 || h == 0 {
            bail!("empty band {w}x{h}");
        }
        if w as u64 * h as u64 > 160_000_000 {
            bail!("band too large ({w}x{h})");
        }
        let bounds = self.page(page)?.bounds().context("page bounds")?;
        let cs = Colorspace::device_rgb();
        let mut pix =
            Pixmap::new_with_w_h(&cs, w as i32, h as i32, false).context("allocate pixmap")?;
        pix.clear_with(255).context("clear pixmap")?;
        {
            let dev = Device::from_pixmap(&pix).context("draw device")?;
            // Map page points to band pixels: normalise the page origin,
            // scale, then move the requested rectangle to the pixmap origin.
            let ctm = Matrix::new(
                scale,
                0.0,
                0.0,
                scale,
                -bounds.x0 * scale - x0 as f32,
                -bounds.y0 * scale - y0 as f32,
            );
            self.page(page)?.run(&dev, &ctm).context("render page")?;
        }

        let (stride, n) = (pix.stride() as usize, pix.n() as usize);
        if n != 3 {
            bail!("unexpected pixmap layout: {n} components");
        }
        let samples = pix.samples();
        let (wu, hu) = (w as usize, h as usize);
        let mut rgba = vec![255u8; wu * hu * 4];
        for row in 0..hu {
            let start = row * stride;
            let end = start + wu * 3;
            let src = samples
                .get(start..end)
                .ok_or_else(|| anyhow!("pixmap row {row} out of bounds"))?;
            let dst = &mut rgba[row * wu * 4..(row + 1) * wu * 4];
            let (s3, _) = src.as_chunks::<3>();
            let (d4, _) = dst.as_chunks_mut::<4>();
            for (sp, dp) in s3.iter().zip(d4.iter_mut()) {
                dp[0] = sp[0];
                dp[1] = sp[1];
                dp[2] = sp[2];
            }
        }
        RgbaImage::from_raw(w, h, rgba).ok_or_else(|| anyhow!("rgba buffer size mismatch"))
    }

    fn page(&mut self, no: usize) -> Result<&Page> {
        if self.current.as_ref().map(|(n, _)| *n) != Some(no) {
            let page = self
                .doc
                .load_page(no as i32)
                .with_context(|| format!("load page {}", no + 1))?;
            self.current = Some((no, page));
        }
        Ok(&self.current.as_ref().expect("page just stored").1)
    }
}
