//! Backend tests against a checked-in one-page PDF: page geometry and that a
//! clipped band is pixel-identical to the same region of a full-page render.

use render_pdf::Pdf;

fn open() -> Pdf {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/one.pdf");
    Pdf::open(&path).expect("open fixture")
}

#[test]
fn reports_pages_and_size() {
    let pdf = open();
    assert_eq!(pdf.meta().pages, 1);
    let (w, h) = pdf.meta().sizes[0];
    assert!((w - 595.0).abs() < 1.0, "width {w}");
    assert!((h - 842.0).abs() < 1.0, "height {h}");
}

#[test]
fn renders_ink_and_paper() {
    let mut pdf = open();
    let (w, h) = pdf.meta().sizes[0];
    let (px, py) = (w.round() as u32, h.round() as u32);
    let img = pdf.render_band(0, 1.0, 0, 0, px, py).unwrap();
    assert_eq!((img.width(), img.height()), (px, py));
    // White paper dominates; the test page has text and colored boxes.
    let dark = img.pixels().filter(|p| p[0] < 200 || p[1] < 200).count();
    assert!(dark > 100, "expected ink, got {dark} dark pixels");
    assert!(dark < (px * py / 2) as usize, "page is suspiciously dark");
    assert_eq!(img.get_pixel(0, 0)[0], 255, "corner should be white");
}

#[test]
fn band_matches_full_render() {
    let mut pdf = open();
    let (w, h) = pdf.meta().sizes[0];
    let (px, py) = (w.round() as u32, h.round() as u32);
    let full = pdf.render_band(0, 1.0, 0, 0, px, py).unwrap();

    let (x0, y0, bw, bh) = (37u32, 211u32, 400u32, 250u32);
    let band = pdf.render_band(0, 1.0, x0, y0, bw, bh).unwrap();
    assert_eq!((band.width(), band.height()), (bw, bh));

    for y in 0..bh {
        for x in 0..bw {
            assert_eq!(
                band.get_pixel(x, y),
                full.get_pixel(x0 + x, y0 + y),
                "pixel ({x},{y}) of band at ({x0},{y0})"
            );
        }
    }
}

#[test]
fn deep_zoom_stays_within_bounds() {
    let mut pdf = open();
    let scale = 4.0;
    // The visible rectangle in the middle of the page at 4x.
    let img = pdf.render_band(0, scale, 800, 1200, 640, 480).unwrap();
    assert_eq!((img.width(), img.height()), (640, 480));
    // The strip at that height contains the "1x zoom crispness" band; it must
    // not be pure paper.
    assert!(img.pixels().any(|p| p[0] < 250 || p[1] < 250 || p[2] < 250));
}

#[test]
fn page_out_of_range_is_an_error() {
    let mut pdf = open();
    assert!(pdf.render_band(1, 1.0, 0, 0, 10, 10).is_err());
}
