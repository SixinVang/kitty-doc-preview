//! Rasterise a PDF band to a PNG for eyeballing the backend.
//! Usage: render <in.pdf> <out.png> [page] [scale] [y0] [h]
use anyhow::Result;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        anyhow::bail!("usage: render <in.pdf> <out.png> [page=0] [scale=2] [y0=0] [h=1200] [x0=0]");
    }
    let mut pdf = render_pdf::Pdf::open(a[1].as_ref())?;
    let num = |i: usize, d: f32| a.get(i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(d);
    let page = num(3, 0.0) as usize;
    let scale = num(4, 2.0);
    let y0 = num(5, 0.0) as u32;
    let h = num(6, 1200.0) as u32;
    let size = pdf.meta().sizes[page];
    let w = (size.0 * scale).round() as u32;
    let x0 = num(7, 0.0) as u32;
    let img = pdf.render_band(page, scale, x0, y0, w, h)?;
    img.save(&a[2])?;
    println!(
        "{} pages, page {} {:.0}x{:.0}pt -> {}x{}",
        pdf.meta().pages,
        page + 1,
        size.0,
        size.1,
        img.width(),
        img.height()
    );
    Ok(())
}
