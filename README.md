# docp

Preview PDFs, images and Markdown in terminals that speak the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Built for macOS + [Rex](https://github.com/tfcace/rex), zero external
dependencies (no Node, no browser, no poppler).

Status: **M1** — images + Markdown (live preview). PDF lands in M2.

```sh
cargo build --release

docp image.png              # image viewer
docp notes.md               # markdown, re-renders on save
docp --theme light notes.md # terminal (default) | dark | light
docp --cat notes.md         # plain styled output, no alt screen
docp --info                 # print detected capabilities
```

Keys: `j/k` scroll · `space/b` page · `d/u` half page · `g/G` top/bottom
(`G` also follows the file) · `]`/`[` next/previous heading · `r` redraw ·
`q` quit · mouse wheel.

## What it does

- **Hybrid Markdown rendering.** Body text, lists, quotes, tables, links and
  highlighted code stay real terminal text — searchable, copyable, tmux-safe.
  Only what a cell grid cannot express becomes pixels: H1/H2 display headings,
  Mermaid diagrams, and embedded images.
- **Pixels are sent once.** Images ride in the text grid as Unicode
  placeholders, so scrolling, redraws and resizes cost zero image traffic;
  a resize rescales in the terminal immediately and re-rasterizes after 80 ms.
- **Live preview.** Saving the file re-renders only changed blocks, keeps your
  scroll anchor, and follows the end when you are at the bottom.
- **Colors from your terminal.** The `terminal` theme asks Rex for its
  foreground, background and 16-color palette at startup; `dark`/`light` are
  built-in overrides.
- **CJK aware.** Wrapping and table layout measure display width, not chars.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Crates:

| crate | role |
|---|---|
| `kgp` | kitty graphics: probing, transmission, placeholders, tiling |
| `termui` | /dev/tty, winsize ioctl, raw mode, timed query reads |
| `docmodel` | `Row`/`Line`/`Cell` display model, SGR/OSC8 encoding, theme |
| `render-img` | decode, viewport scale, cell-grid canvas, PNG tiles |
| `render-md` | markdown blocks, text layout, syntect, big-type headings, Mermaid |
| `docp` | CLI, screen painter, image view, markdown view, cat mode |

## License

AGPL-3.0-or-later (links MuPDF for the upcoming PDF backend).
Embedded assets: Inter and JetBrains Mono (SIL OFL), Pele (MIT).
