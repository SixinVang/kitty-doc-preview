---
name: docp-preview
description: Open Markdown, PDF or image files in a live docp preview pane inside Rex, and drive that pane (pages, zoom, close). Use when the user asks to show, preview, render or open a .md/.pdf/image document, or when a long report, wide table, Mermaid diagram or finished PDF is worth reviewing visually instead of as raw text.
---

# docp preview panes

`docp` is this project's terminal document viewer: it renders Markdown (text +
pixel headings, tables, Mermaid, images), PDFs (paged, zoomable) and images
using the kitty graphics protocol. It must run **inside a terminal pane**, so in
Rex the way to show a document is a `rex split` running docp, not a command in
the agent's own pane.

Read `README.md` for keys, config and rendering behaviour.

## When to use it

Use docp when:

- the user says to open / show / preview / render a `.md`, `.pdf`, `.png`,
  `.jpg`, `.gif`, `.webp`, or a path they call "the doc", "the report", "the
  spec", "the paper";
- you just wrote or heavily edited a long Markdown document (report, design doc,
  changelog, `docs/*.md`) and the user will want to read it — a live pane beats
  a wall of chat text;
- the content is visually structured: wide tables, Mermaid diagrams, embedded
  images, multi-page PDFs.

Do **not** use docp to read a file yourself. Use the `read` tool (or
`docp --cat FILE` only when you specifically want docp's styled text in a
capture). Never open docp in the agent's own pane: it takes over the alternate
screen, needs keyboard input, and would block the session. Never pass `--wait`.

## Preconditions

1. Rex context: `REX_SESSION` and `REX_BLOCK` are set. If not, run
   `rex whoami --json` and `rex ls --json`, and ask the user which session/pane
   to use.
2. `docp` on `PATH` (`command -v docp`, else `~/.local/bin/docp`). If missing,
   tell the user to install it (`cargo install --git
   https://github.com/SixinVang/kitty-doc-preview docp`) — don't build or install
   it unasked.
3. A kitty-graphics terminal. Rex has it; docp prints
   `terminal does not support the kitty graphics protocol` and exits when it
   doesn't. Verify a pane with `rex capture` after opening (step below).

## Open a preview

Prefer the bundled helper — it resolves absolute paths, reuses an existing pane
for the same file, verifies the render and closes the pane if docp bailed:

```sh
~/.agents/skills/docp-preview/scripts/docp-pane.sh open notes.md
# with options
~/.agents/skills/docp-preview/scripts/docp-pane.sh open paper.pdf --page 12 --ratio 55
```

Resolve the script path from this skill's directory (Pi reports it at load time);
the copy above assumes the skill is installed as `~/.agents/skills/docp-preview`.

It prints the `block:<id>` on stdout, a human note on stderr. Exit codes:
`0` ok, `2` not in Rex, `3` bad input, `4` rex/docp failed (message already
printed).

Subcommands: `open FILE [--page N] [--ratio R] [--theme terminal|dark|light]
[--reopen]`, `close FILE`, `close-block BLOCK_ID`, `list`.

### Manual equivalent (no script available)

```sh
path=$(realpath notes.md)
out=$(rex split -s "$REX_SESSION" -b "$REX_BLOCK" --split=right --ratio 40 \
      --label "docp: $(basename "$path")" --focus=false --json -- docp "$path")
pane=$(printf '%s' "$out" | jq -er '.block_ids[0]')
sleep 1
rex capture -s "$REX_SESSION" -b "$pane" --trim | head -20
```

Keep `--focus=false` so the user's focus stays where it was, keep the
`docp: <basename>` label so the pane can be found again, and always pass an
absolute path. Use `--split=right --ratio 30..55` by default; `--split=above`
when the document is wide (tables) and the pane has more width than height.

## Reuse, reload, close

- docp reloads by **itself** when the file is saved — for Markdown it patches
  changed blocks and keeps the scroll anchor; PDFs reopen at the same page. So
  after editing a document that already has a pane, just save (or let the edit
  land); do not open another pane.
- Same file opened twice = two panes. Check first:
  `~/.agents/skills/docp-preview/scripts/docp-pane.sh list`, or
  `rex block ls -s "$REX_SESSION" --json | jq -r '.blocks[] | select(.label|startswith("docp: ")) | "\(.block_id)\t\(.label)"'`.
  Labels use the basename only, so `a/notes.md` and `b/notes.md` share a label —
  keep one such document open at a time, or close by block id.
- Force a fresh render (changed flags/binary, docp wedged): `... open FILE
  --reopen`.
- Close when the user is done: `... close FILE` or `... close-block $pane`.
  Closing the pane kills docp; docp does not need a socket cleanup step unless
  it was started with `--ipc` (only the Neovim plugin does that).
- The pane dies if the user presses `q`; a later `rex capture` then reports a
  missing block. That is a normal close, not an error — reopen if needed.

## Drive the pane

Never steer docp by typing into the agent's pane. Target the preview block:

```sh
rex send-key -s "$REX_SESSION" -b "$pane" n          # PDF: next page
rex send-key -s "$REX_SESSION" -b "$pane" shift+g    # scroll to end / follow
rex send-key -s "$REX_SESSION" -b "$pane" q          # quit (pane closes)
rex capture -s "$REX_SESSION" -b "$pane" --trim      # read the status bar
rex zoom -s "$REX_SESSION" "$pane" --on              # temporarily maximize
```

`send-key` lowercases letter names, so use `shift+g` for `G`. Verify state with
`rex capture`: Markdown shows a heading/text screen, PDF shows
`p 1/1 · 100% · tile 11ms` in the gutter status line, docp's error output means
it failed. Free-form scroll keys (`j`, `d`, `]`) are better sent by the user;
use keys yourself only for a concrete request ("go to page 12").

## Options worth knowing

| flag | use |
|---|---|
| `--page N` | open a PDF at page N (1-based) — matches a quoted page number |
| `--theme terminal\|dark\|light` | `terminal` follows Rex's palette; use `dark`/`light` if contrast looks wrong |
| `--max-width N` | clamp the text column (cells) |
| `--font-sans FILE` / `--font-sans-file PATH` | different heading font |
| `--cat` | non-interactive styled text on stdout, markdown only |

## Troubleshooting

| symptom | cause / fix |
|---|---|
| `terminal does not support the kitty graphics protocol` | docp ran outside a graphics-capable pane. Ensure the split is in Rex, not a captured/headless shell. |
| `no such file` | relative path resolved against the wrong cwd; pass an absolute path (the helper does). |
| pane flashes and closes | docp exited: usually the path or the graphics check. Capture right after `--keep-open` or rerun in a shell pane to read the message. |
| empty/garbled capture but docp is alive | images are sent out of band; text still captures fine. Judge success by the status bar, not by the image. |
| PDF text extraction requested | not implemented (`pdf text output needs the text layer (M3)`); read the PDF another way and note the limitation. |
| `--ipc` error | scroll sync is Markdown-only and needs an interactive preview; don't add it from an agent. |

## Report to the user

Say what opened and where: file name, pane position ("split on the right",
"maximized"), page number for PDFs, and that it reloads on save. Mention
failures verbatim (capability errors, missing binary) instead of retrying
silently, and confirm the close when asked to tidy up.
