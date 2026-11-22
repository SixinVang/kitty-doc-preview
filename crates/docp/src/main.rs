mod caps;
mod cat;
mod clipboard;
mod config;
mod imgview;
mod input;
mod ipc;
mod links;
mod mdview;
mod pagedview;
mod screen;
mod srcmap;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{CliOverrides, Config};
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
    /// Config file (default: ~/.config/docp/config.toml)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Color theme: terminal, dark or light
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,
    /// Family for headings and pixel text
    #[arg(long, value_name = "FAMILY")]
    font_sans: Option<String>,
    /// Family for code and diagram labels
    #[arg(long, value_name = "FAMILY")]
    font_mono: Option<String>,
    /// Font file to use for sans text
    #[arg(long, value_name = "PATH")]
    font_sans_file: Option<String>,
    /// Font file to use for monospace text
    #[arg(long, value_name = "PATH")]
    font_mono_file: Option<String>,
    /// Emoji family for pixel text (defaults to the best installed one).
    #[arg(long, env = "DOCP_FONT_EMOJI")]
    font_emoji: Option<String>,
    /// Font file backing `--font-emoji`.
    #[arg(long, env = "DOCP_FONT_EMOJI_FILE")]
    font_emoji_file: Option<String>,
    /// Text column width clamp, in cells
    #[arg(long, value_name = "CELLS")]
    max_width: Option<usize>,
    /// Print detected capabilities and the effective config, then exit
    #[arg(short, long)]
    info: bool,
    /// Render to stdout instead of the alternate screen (no interactivity)
    #[arg(long)]
    cat: bool,
    /// PDF page to start on (1-based)
    #[arg(long, value_name = "N")]
    page: Option<usize>,
    /// Unix socket for editor scroll sync (Markdown only)
    #[arg(long, value_name = "PATH")]
    ipc: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ThemeChoice {
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

    let mut config = Config::load(cli.config.as_deref())?;
    config.apply_cli(&CliOverrides {
        theme: cli.theme.clone(),
        font_sans: cli.font_sans.clone(),
        font_mono: cli.font_mono.clone(),
        font_sans_file: cli.font_sans_file.clone(),
        font_mono_file: cli.font_mono_file.clone(),
        font_emoji: cli.font_emoji.clone(),
        font_emoji_file: cli.font_emoji_file.clone(),
        max_width: cli.max_width,
    });
    // Fonts must be configured before the first render loads them.
    render_md::fonts::configure(config.font_spec());

    let caps = caps::detect(300).context("probing terminal capabilities")?;
    if cli.info {
        println!("{caps:#?}");
        println!("{config:#?}");
        match config::Config::source(cli.config.as_deref()) {
            Some((path, true)) => println!("config: {} (loaded)", path.display()),
            Some((path, false)) => {
                println!("config: {} (not found: using defaults)", path.display())
            }
            None => println!("config: none (using defaults)"),
        }
        println!(
            "fonts: sans={} display={} mono={}",
            render_md::fonts::sans_family(),
            render_md::fonts::display_family(),
            render_md::fonts::mono_family()
        );
        println!(
            "fonts: emoji={}",
            render_md::fonts::emoji_family().unwrap_or("(none)")
        );
        let n = render_md::highlight::syntax_count();
        println!(
            "highlight: {} syntaxes{}",
            n,
            if n == 0 {
                " (built without the `syntaxes` feature)"
            } else {
                ""
            }
        );
        return Ok(());
    }

    let Some(file) = cli.file else {
        bail!("no file given; try `docp image.png`, `docp --info` or `docp gen-test out.png`")
    };
    if !file.exists() {
        bail!("no such file: {}", file.display());
    }

    let theme = resolve_theme(config.theme.name.as_deref(), &caps)?;
    let kind = caps::kind(&file);
    if cli.ipc.is_some() && kind != caps::Kind::Markdown {
        bail!("--ipc is only supported for Markdown previews");
    }
    if cli.cat || !std::io::stdout().is_terminal() {
        if cli.ipc.is_some() {
            bail!("--ipc needs an interactive preview; drop --cat");
        }
        return cat::run(&file, kind, &caps, &theme, config.layout.max_width);
    }
    if !caps.graphics {
        bail!("terminal does not support the kitty graphics protocol");
    }

    match kind {
        caps::Kind::Image => imgview::run(&file, caps),
        caps::Kind::Markdown => mdview::run(
            &file,
            caps,
            theme,
            config.layout.max_width,
            cli.ipc.as_deref(),
        ),
        caps::Kind::Pdf => pagedview::run(&file, caps, theme, cli.page),
    }
}

fn resolve_theme(name: Option<&str>, caps: &caps::Caps) -> Result<Theme> {
    match name.unwrap_or("terminal") {
        "terminal" => Ok(caps.terminal_theme.clone()),
        "dark" => Ok(Theme::dark_default()),
        "light" => Ok(Theme::light_default()),
        other => bail!("unknown theme {other:?}; expected terminal, dark or light"),
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

#[allow(dead_code)]
fn _theme_choice(_c: ThemeChoice) {}
