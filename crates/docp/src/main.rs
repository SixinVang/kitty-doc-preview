mod caps;
mod cat;
mod imgview;
mod mdview;
mod screen;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use docmodel::Theme;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "docp",
    version,
    about = "Preview PDFs, images and Markdown in kitty-graphics terminals (Rex)"
)]
struct Cli {
    /// File to preview
    file: Option<PathBuf>,
    /// Color theme
    #[arg(long, value_enum, env = "DOCP_THEME", default_value_t = ThemeChoice::Terminal)]
    theme: ThemeChoice,
    /// Print detected terminal capabilities and exit
    #[arg(short, long)]
    info: bool,
    /// Render to stdout instead of the alternate screen (no interactivity)
    #[arg(long)]
    cat: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ThemeChoice {
    /// Use the terminal's own colors (queried at startup)
    Terminal,
    Dark,
    Light,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a test image
    GenTest {
        path: PathBuf,
        /// Generate a tall image (exercises vertical tiling)
        #[arg(long)]
        tall: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::GenTest { path, tall }) = cli.command {
        return gen_test(&path, tall);
    }

    let caps = caps::detect(300).context("probing terminal capabilities")?;
    if cli.info {
        println!("{caps:#?}");
        return Ok(());
    }

    let Some(file) = cli.file else {
        bail!("no file given; try `docp image.png`, `docp --info` or `docp gen-test out.png`")
    };
    if !file.exists() {
        bail!("no such file: {}", file.display());
    }

    let theme = match cli.theme {
        ThemeChoice::Terminal => caps.terminal_theme.clone(),
        ThemeChoice::Dark => Theme::dark_default(),
        ThemeChoice::Light => Theme::light_default(),
    };

    let kind = caps::kind(&file);
    if cli.cat || !std::io::stdout().is_terminal() {
        return cat::run(&file, kind, &caps, &theme);
    }
    if !caps.graphics {
        bail!("terminal does not support the kitty graphics protocol");
    }

    match kind {
        caps::Kind::Image => imgview::run(&file, caps),
        caps::Kind::Markdown => mdview::run(&file, caps, theme),
        caps::Kind::Pdf => bail!("PDF support lands in M2; not wired yet"),
    }
}

fn gen_test(path: &std::path::Path, tall: bool) -> Result<()> {
    let (w, h) = if tall { (600u32, 9000u32) } else { (1280, 800) };
    let mut img = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = (x * 255 / w) as u8;
            let g = (y * 255 / h) as u8;
            let b = ((x + y) % 256) as u8;
            img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
        }
    }
    // white grid + dark bars so tiling seams and scaling are easy to see
    for y in (0..h).step_by(100) {
        for x in 0..w {
            img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
        }
    }
    for x in (0..w).step_by(100) {
        for y in 0..h {
            img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
        }
    }
    for k in 0..10 {
        let y0 = 40 + k * 90;
        for y in y0..(y0 + 40).min(h) {
            for x in 40..w.saturating_sub(40) {
                img.put_pixel(x, y, image::Rgba([32, 32, 32, 255]));
            }
        }
    }
    img.save(path)
        .with_context(|| format!("save {}", path.display()))?;
    println!("wrote {} ({w}x{h})", path.display());
    Ok(())
}
