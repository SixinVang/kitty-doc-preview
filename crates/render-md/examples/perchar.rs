//! Per-character pixel comparison for a pixel heading.
//!
//! Renders a heading with the production pipeline, then renders each
//! character on its own at its exact origin in that heading and diffs the two
//! images. Identical shapes mean the character really comes from the
//! configured family; a large residual means resvg picked another font (for
//! emoji that is expected and correct).
//!
//! ```sh
//! cargo run -p render-md --example perchar -- "🧩 准备运行环境"
//! PINGFANG=... # or set the family with the first env var below
//! ```
//!
//! Writes a contact sheet to `perchar.png`: one row per character, columns
//! `standalone | in-heading | diff`.

use docmodel::Rgb;
use render_md::{fonts, raster};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::fontdb::{Family, Query, Stretch, Style, Weight as CssWeight};
use std::sync::LazyLock;

const CELL: raster::Cell = raster::Cell { w: 16, h: 32 };
const LEVEL: u8 = 2;
const COLOR: Rgb = Rgb(52, 57, 64);
const SHEET: &str = "perchar.png";

struct Img {
    w: i64,
    h: i64,
    px: Vec<u8>,
}

impl Img {
    fn at(&self, x: i64, y: i64) -> [u8; 4] {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return [0; 4];
        }
        let i = ((y * self.w + x) * 4) as usize;
        [self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]]
    }

    fn ink_bbox(&self) -> Option<(i64, i64, i64, i64)> {
        let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, -1, -1);
        for y in 0..self.h {
            for x in 0..self.w {
                if self.at(x, y)[3] > 8 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x1 >= 0).then_some((x0, y0, x1, y1))
    }

    fn crop(&self, x0: i64, y0: i64, w: i64, h: i64) -> Img {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let p = self.at(x0 + x, y0 + y);
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&p);
            }
        }
        Img { w, h, px }
    }
}

fn decode(bytes: &[u8]) -> Img {
    let mut r = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).unwrap();
    Img {
        w: info.width as i64,
        h: info.height as i64,
        px: buf,
    }
}

/// Alpha-weighted channel difference: RGB under transparent pixels is ignored,
/// otherwise decoded premultiplied leftovers show up as noise.
fn diff(a: &Img, b: &Img) -> (u64, u32, u64, u64) {
    let (mut sum, mut max, mut bad, mut n) = (0u64, 0u32, 0u64, 0u64);
    for y in 0..a.h {
        for x in 0..a.w {
            let pa = a.at(x, y);
            let pb = b.at(x, y);
            let wa = [
                pa[0] as u32 * pa[3] as u32,
                pa[1] as u32 * pa[3] as u32,
                pa[2] as u32 * pa[3] as u32,
            ];
            let wb = [
                pb[0] as u32 * pb[3] as u32,
                pb[1] as u32 * pb[3] as u32,
                pb[2] as u32 * pb[3] as u32,
            ];
            let d = (0..3).map(|k| wa[k].abs_diff(wb[k]) / 255).max().unwrap();
            sum += d as u64;
            max = max.max(d);
            n += 1;
            if d > 24 {
                bad += 1;
            }
        }
    }
    (sum, max, bad, n)
}

/// The full heading, drawn by the production path at full column width so the
/// comparison is never clipped by `cols` sizing.
fn render_heading(text: &str, cell: raster::Cell) -> (Img, f32, u16, fonts::Weight) {
    let base = raster::layout_heading("x", LEVEL, cell, 80);
    let css_weight = if base.weight == fonts::Weight::DisplayBold {
        700
    } else {
        600
    };
    let spec = raster::Heading {
        lines: vec![text.to_string()],
        level: LEVEL,
        rows_per_line: base.rows_per_line,
        cols: 80,
        font_size: base.font_size,
        weight: base.weight,
    };
    let raster = raster::heading(&spec, COLOR, cell).unwrap();
    (decode(&raster.png), base.font_size, css_weight, base.weight)
}

/// Advance of `c` in the face resvg would use for it.
fn advance(c: char, font_size: f32) -> f32 {
    static DB: LazyLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
        LazyLock::new(fonts::with_system);
    let db = &*DB;
    let query = Query {
        families: &[Family::Name(fonts::display_family())],
        weight: CssWeight(600),
        stretch: Stretch::Normal,
        style: Style::Normal,
    };
    let mut ids: Vec<_> = db.query(&query).into_iter().collect();
    ids.extend(db.faces().map(|f| f.id));
    for id in ids {
        // Private faces answer for every codepoint while drawing placeholders.
        if db
            .face(id)
            .map(|f| f.families.iter().any(|(n, _)| n.starts_with('.')))
            .unwrap_or(true)
        {
            continue;
        }
        let mut advance = None;
        db.with_face_data(id, |data, index| {
            if let Ok(face) = ttf_parser::Face::parse(data, index) {
                if let Some(glyph) = face.glyph_index(c) {
                    advance = face
                        .glyph_hor_advance(glyph)
                        .map(|a| a as f32 * font_size / face.units_per_em() as f32);
                }
            }
        });
        if let Some(a) = advance {
            return a;
        }
    }
    font_size
}

/// A single glyph at an exact (fractional) x, with the same SVG attributes the
/// production `heading()` emits.
fn render_glyph(
    c: char,
    x: f32,
    font_size: f32,
    css_weight: u16,
    weight: fonts::Weight,
    w: u32,
    h: u32,
) -> Img {
    let metrics = fonts::metrics(weight);
    let baseline = (h as f32 + metrics.cap_height * font_size) / 2.0;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"><text x="{x:.3}" y="{baseline:.3}" font-family="{}" font-weight="{css_weight}" font-size="{:.3}" fill="{}">{}</text></svg>"##,
        fonts::display_family(),
        font_size,
        COLOR.hex(),
        escape(c)
    );
    let opts = resvg::usvg::Options {
        fontdb: fonts::with_system(),
        font_family: fonts::sans_family().into(),
        font_resolver: fonts::font_resolver(),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(&svg, &opts).unwrap();
    let mut pixmap = Pixmap::new(w, h).unwrap();
    resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());

    let mut data = pixmap.data().to_vec();
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3];
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u16 * 255 + a as u16 / 2) / a as u16) as u8;
            }
        }
    }
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&data).unwrap();
    decode(&png)
}

fn escape(c: char) -> String {
    match c {
        '<' => "&lt;".into(),
        '>' => "&gt;".into(),
        '&' => "&amp;".into(),
        c => c.to_string(),
    }
}

fn main() {
    let family = std::env::var("PERCHAR_FAMILY").unwrap_or_else(|_| "PingFang SC".into());
    fonts::configure(fonts::FontSpec {
        sans: Some(family.clone()),
        ..Default::default()
    });
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "🧩 准备运行环境".into());

    let (full, font_size, css_weight, weight) = render_heading(&text, CELL);
    println!(
        "family={family} text={text:?} size={font_size:.2} weight={css_weight} image={}x{}",
        full.w, full.h
    );
    println!(
        "{:<4} {:<6} {:>9} {:>8} {:>7} {:>7}  verdict",
        "idx", "char", "origin", "maxdiff", "bad_px", "mean"
    );

    let mut sheet = Vec::new();
    let mut x = 0.0;
    for (i, c) in text.chars().enumerate() {
        let origin = x;
        x += advance(c, font_size);
        if c.is_whitespace() {
            continue;
        }
        let glyph = render_glyph(
            c,
            origin,
            font_size,
            css_weight,
            weight,
            full.w as u32,
            full.h as u32,
        );
        let Some((x0, y0, x1, y1)) = glyph.ink_bbox() else {
            println!("[{i}] {c:?} EMPTY");
            continue;
        };
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let standalone = glyph.crop(x0, y0, w, h);
        let in_heading = full.crop(x0, y0, w, h);
        let (sum, max, bad, n) = diff(&standalone, &in_heading);
        let mean = sum as f64 / n as f64;
        let verdict = if bad == 0 {
            "identical"
        } else if mean < 2.0 {
            "same font (AA only)"
        } else {
            "DIFFERENT FONT"
        };
        println!("{i:<4} {c:<6} {origin:>9.2} {max:>8} {bad:>7} {mean:>7.3}  {verdict}");

        let mut dpx = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let a = standalone.at(x, y);
                let b = in_heading.at(x, y);
                let wa = [
                    a[0] as u32 * a[3] as u32,
                    a[1] as u32 * a[3] as u32,
                    a[2] as u32 * a[3] as u32,
                ];
                let wb = [
                    b[0] as u32 * b[3] as u32,
                    b[1] as u32 * b[3] as u32,
                    b[2] as u32 * b[3] as u32,
                ];
                let d = (0..3).map(|k| wa[k].abs_diff(wb[k]) / 255).max().unwrap() as u8;
                let i = ((y * w + x) * 4) as usize;
                dpx[i] = d;
                dpx[i + 3] = 255;
            }
        }
        sheet.push((standalone, in_heading, Img { w, h, px: dpx }));
    }

    // Contact sheet: standalone | in-heading | diff, one row per character.
    let gap = 8;
    let col_w = sheet.iter().map(|(a, _, _)| a.w).max().unwrap_or(1) + gap;
    let row_h = sheet.iter().map(|(a, _, _)| a.h).max().unwrap_or(1) + gap;
    let (sw, sh) = (col_w * 3, row_h * sheet.len() as i64);
    let mut out = vec![255u8; (sw * sh * 4) as usize];
    for (r, (a, b, d)) in sheet.iter().enumerate() {
        for (c, img) in [a, b, d].iter().enumerate() {
            let (ox, oy) = (c as i64 * col_w, r as i64 * row_h);
            for y in 0..img.h {
                for x in 0..img.w {
                    let p = img.at(x, y);
                    let alpha = p[3] as u16;
                    let i = (((oy + y) * sw + ox + x) * 4) as usize;
                    for k in 0..3 {
                        out[i + k] = (p[k] as u16 * alpha / 255 + 255 - alpha) as u8;
                    }
                    out[i + 3] = 255;
                }
            }
        }
    }
    let mut png = Vec::new();
    let mut enc = png::Encoder::new(&mut png, sw as u32, sh as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&out).unwrap();
    std::fs::write(SHEET, &png).unwrap();
    println!("\nwrote {SHEET}");
}
