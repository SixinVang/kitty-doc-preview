//! Image loading and cell-grid layout: decode, scale to the viewport width
//! (never upscaling), pad to an exact multiple of the cell size so the
//! terminal never rescales, slice into <= MAX_TILE_ROWS tiles, encode PNG.

use anyhow::{bail, Context, Result};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{
    imageops, DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, ImageReader, Rgba,
    RgbaImage,
};
use std::path::Path;

pub use kgp::MAX_TILE_ROWS;

#[derive(Debug, Clone)]
pub struct Tile {
    pub png: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub tiles: Vec<Tile>,
    /// Total cell columns/rows the full image occupies.
    pub cols: u16,
    pub rows: u16,
    /// Uniform rows per full tile (the last tile may be shorter).
    pub rows_per_tile: u16,
    pub src_w: u32,
    pub src_h: u32,
}

pub fn load(path: &Path) -> Result<DynamicImage> {
    ImageReader::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .with_guessed_format()
        .context("sniff image format")?
        .decode()
        .with_context(|| format!("decode {}", path.display()))
}

/// Lay out `img` for a viewport of `avail_cols` cells with `cell` pixels per
/// cell. Returns vertically sliced tiles whose pixel size is an exact
/// multiple of the cell size.
pub fn layout(img: &DynamicImage, cell: (u32, u32), avail_cols: u16) -> Result<Layout> {
    let (cw, ch) = cell;
    if cw == 0 || ch == 0 {
        bail!("bad cell size {cw}x{ch}");
    }
    let (iw, ih) = (img.width().max(1), img.height().max(1));
    let max_w = avail_cols.max(1) as u32 * cw;
    let scale = (max_w as f64 / iw as f64).min(1.0);
    let px_w = ((iw as f64 * scale).round() as u32).clamp(1, max_w);
    let px_h = ((ih as f64 * scale).round() as u32).max(1);

    let cols = px_w.div_ceil(cw).max(1);
    let rows_total = px_h.div_ceil(ch).max(1);
    let canvas_w = cols * cw;
    let canvas_h = rows_total * ch;

    let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, Rgba([0, 0, 0, 0]));
    let resized = imageops::resize(img, px_w, px_h, imageops::FilterType::Lanczos3);
    imageops::overlay(&mut canvas, &resized, 0, 0);

    let rows_per_tile = MAX_TILE_ROWS.min(rows_total as u16).max(1);
    let mut tiles = Vec::new();
    let mut row0: u32 = 0;
    while row0 < rows_total {
        let tr = (rows_total - row0).min(rows_per_tile as u32);
        let sub = canvas.view(0, row0 * ch, canvas_w, tr * ch).to_image();
        tiles.push(Tile {
            png: encode_png(&sub)?,
            cols: cols as u16,
            rows: tr as u16,
        });
        row0 += tr;
    }

    Ok(Layout {
        tiles,
        cols: cols as u16,
        rows: rows_total as u16,
        rows_per_tile,
        src_w: iw,
        src_h: ih,
    })
}

pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let enc = PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive);
    enc.write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        ExtendedColorType::Rgba8,
    )
    .context("png encode")?;
    Ok(out)
}

/// 1x1 transparent PNG used for capability probing.
pub fn tiny_png() -> Vec<u8> {
    let img = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
    encode_png(&img).expect("encode 1x1 png")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn gradient(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(
                    x,
                    y,
                    Rgba([
                        (x * 255 / w.max(1)) as u8,
                        (y * 255 / h.max(1)) as u8,
                        128,
                        255,
                    ]),
                );
            }
        }
        DynamicImage::ImageRgba8(img)
    }

    fn decode(png: &[u8]) -> (u32, u32) {
        let img = ImageReader::new(Cursor::new(png))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn fits_and_pads_to_cell_grid() {
        let img = gradient(100, 50);
        let l = layout(&img, (10, 20), 8).unwrap();
        // 100x50 -> 80x40 px -> 8x2 cells, canvas padded to 80x40
        assert_eq!((l.cols, l.rows), (8, 2));
        assert_eq!(l.tiles.len(), 1);
        assert_eq!(decode(&l.tiles[0].png), (80, 40));
    }

    #[test]
    fn no_upscale() {
        let img = gradient(64, 64);
        let l = layout(&img, (10, 20), 20).unwrap(); // 200px available
        assert_eq!((l.cols, l.rows), (7, 4)); // 64px wide -> 7 cells (70px canvas)
        assert_eq!(decode(&l.tiles[0].png), (70, 80));
    }

    #[test]
    fn tall_image_tiles_vertically() {
        let img = gradient(100, 6000);
        let l = layout(&img, (10, 20), 10).unwrap();
        assert_eq!((l.cols, l.rows), (10, 300));
        assert_eq!(l.rows_per_tile, 280);
        assert_eq!(l.tiles.len(), 2);
        assert_eq!(l.tiles[0].rows, 280);
        assert_eq!(l.tiles[1].rows, 20);
        assert_eq!(decode(&l.tiles[0].png), (100, 5600));
        assert_eq!(decode(&l.tiles[1].png), (100, 400));
    }

    #[test]
    fn tiny_png_is_valid() {
        assert_eq!(decode(&tiny_png()), (1, 1));
    }
}
