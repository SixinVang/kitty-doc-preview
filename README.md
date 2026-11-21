# docp

Preview PDFs, images and Markdown in terminals that speak the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Built for macOS + [Rex](https://github.com/tfcace/rex), zero external
dependencies (no Node, no browser, no poppler).

Status: **M3a** — images + Markdown (live preview, Neovim scroll sync) + PDF (paged, zoom, reload).

```sh
cargo build --release

docp image.png              # image viewer
docp notes.md               # markdown, re-renders on save
docp paper.pdf              # PDF, fit-width, n/p pages, z/Z zoom
docp --page 12 paper.pdf    # start on page 12
docp --theme light notes.md # terminal (default) | dark | light
docp --cat notes.md         # plain styled output, no alt screen
docp --info                 # print detected capabilities and config
docp --ipc /tmp/p.sock n.md # also serve editor scroll sync (Neovim plugin)
```

Fonts and layout are configurable, by file or by flag (flags win):

```sh
docp --font-sans "Atkinson Hyperlegible" notes.md
docp --font-sans-file ~/Library/Fonts/MySans.ttf notes.md
docp --max-width 90 notes.md
docp --config ./docp.toml notes.md
```

```toml
# ~/.config/docp/config.toml   (see docp.example.toml)
[theme]
name = "terminal"            # terminal | dark | light

[fonts]
sans = "Inter"               # headings / pixel text
mono = "JetBrains Mono"      # diagram labels
sans_file = ""               # optional files win over the families
mono_file = ""

[layout]
max_width = 110              # text column clamp, in cells
```

Keys: `j/k` scroll · `space/b` page · `d/u` half page · `g/G` top/bottom
(`G` also follows the file) · `]`/`[` next/previous heading · `r` redraw ·
`q` quit · mouse wheel.

PDF adds: `n/p` (or `]`/`[`) next/previous page · `1`–`9` jump to 10 %…90 % ·
`z`/`Z` zoom in/out (re-rasterised, never upscaled) · `h/l` pan when zoomed ·
`Option+J/K` instant 5-row jumps (the old non-animated feel) · `r` reopen.
Scroll with `j/k`/wheel is pixel-smooth. Page number sits in the left gutter
and in the status bar.

## What it does

- **Hybrid Markdown rendering.** Body text, lists, quotes, tables, links and
  highlighted code stay real terminal text — searchable, copyable, tmux-safe.
  Only what a cell grid cannot express becomes pixels: H1/H2 display headings,
  Mermaid diagrams, and embedded images.
- **Pixels are sent once.** Images ride in the text grid as Unicode
  placeholders, so scrolling, redraws and resizes cost zero image traffic;
  a resize rescales in the terminal immediately and re-rasterizes after 80 ms.
- **Live preview.** Saving the file re-renders only changed blocks, keeps your
  scroll anchor, and follows the end when you are at the bottom. PDFs reopen in
  place, keeping the page and the scroll offset inside it.
- **PDF is vector, tiled to the viewport.** Fit-width layout is pure geometry;
  only the visible rectangle (± prefetch) is rasterised by MuPDF on a dedicated
  thread, sliced into ≤64-row tiles and cached in the terminal. Zooming
  re-rasterises at the new scale and pans horizontally instead of stretching
  pixels.
- **Smooth PDF scrolling.** Tiles are transmitted once and shown as placements
  with a source crop; a scroll only changes the crop, so the page glides by
  pixels (a few hundred bytes per frame) instead of jumping a whole cell row.
- **Colors from your terminal.** The `terminal` theme asks Rex for its
  foreground, background and 16-color palette at startup; `dark`/`light` are
  built-in overrides.
- **CJK aware.** Wrapping, table padding and alignment measure display width
  (`图片` is 4 columns, not 2 chars), with hanging punctuation so closing marks
  never start a line.

## Neovim + Rex: synced preview

Edit Markdown in Neovim with a live docp pane beside it; scrolling in either
pane follows the other. The plugin lives in [`nvim/`](nvim/) and currently
requires [Rex](https://github.com/tfcace/rex) (the pane is opened with
`rex split`).

```lua
-- lazy.nvim, or add ~/workspace/kitty-doc-preview/nvim to 'runtimepath'
{ dir = "~/workspace/kitty-doc-preview/nvim",
  config = function() require("docp").setup({ docp_bin = "docp" }) end }
```

Then, in a Markdown buffer inside a Rex pane:

```vim
:DocpPreview   " open a preview pane for this buffer (reuses it next time)
:DocpToggle    " open or close
:DocpClose     " close the pane and the socket
```

`setup()` options: `docp_bin` (name on `PATH` or absolute), `docp_args`,
`ratio` (share of the Rex split, default 50), `debounce` (ms, default 40),
`takeover_ms` (how long preview-driven scrolls suppress editor echo, default
120).

Under the hood `docp --ipc <socket> file.md` serves newline-delimited JSON on a
Unix socket: the editor sends the top visible line (`sync`), the preview
answers with its top block's line (`top`). Rendering still reloads on save;
sync is block-granular in v1 (see
[ARCHITECTURE §5.4](docs/ARCHITECTURE.md#54-编辑器同步neovim--rex)).

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Crates:

| crate | role |
|---|---|
| `kgp` | kitty graphics: probing, transmission, placeholders, tiling |
| `termui` | /dev/tty, winsize ioctl, raw mode, timed query reads |
| `docmodel` | `Row`/`Line`/`Cell` display model, SGR/OSC8 encoding, theme |
| `render-img` | decode, viewport scale, cell-grid canvas, PNG tiles |
| `render-md` | markdown blocks, text layout, syntect, big-type headings, Mermaid |
| `render-pdf` | MuPDF open/page geometry, clipped band rasterisation |
| `docp` | CLI, screen painter, image view, markdown view, PDF view, cat mode |

## License

AGPL-3.0-or-later (links MuPDF, itself AGPL, for the PDF backend).
Embedded assets: Inter and JetBrains Mono (SIL OFL), Pele (MIT).
