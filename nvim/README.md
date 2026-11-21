# docp.nvim

Live [docp](../../README.md) Markdown preview in a Rex split, with
bidirectional scroll sync.

Requires Neovim 0.10+ (uses `vim.system`, `vim.uv`) and
[Rex](https://github.com/tfcace/rex): the preview pane is opened with
`rex split`, so `REX_SESSION` and `REX_BLOCK` must be set (they are inside a
Rex terminal).

## Install

This plugin ships inside the docp repository, under `nvim/`, and talks to the
`docp` binary. Source of truth:
<https://github.com/SixinVang/kitty-doc-preview>

```sh
git clone https://github.com/SixinVang/kitty-doc-preview
cd kitty-doc-preview
cargo build --release
ln -sf "$PWD/target/release/docp" ~/.local/bin/docp   # or: cargo install --path crates/docp
```

With lazy.nvim, add the repo and put its `nvim/` directory on `runtimepath`:

```lua
{
  "SixinVang/kitty-doc-preview",
  name = "docp",
  lazy = false, -- config() adds nvim/ to 'runtimepath'
  config = function(plugin)
    vim.opt.rtp:append(plugin.dir .. "/nvim")
    require("docp").setup({ docp_bin = "docp" })
  end,
}
```

Without a plugin manager, add `nvim/` from the clone to `runtimepath` before
plugins load:

```lua
vim.opt.rtp:append(vim.fn.expand("~/src/kitty-doc-preview/nvim"))
require("docp").setup({ docp_bin = "docp" })
```

`docp` must be built (`cargo build --release`) and either on `PATH` or given as
an absolute path in `docp_bin`.

## Commands

| command | effect |
|---|---|
| `:DocpPreview` | open a preview pane for the current buffer (closes a previous one) |
| `:DocpClose` | close the pane, the socket and the IPC client |
| `:DocpToggle` | preview if closed, close if open |

Opt-in: no preview opens automatically and no autocmds exist until
`:DocpPreview`. Autocmds are buffer-local (`WinScrolled`, `CursorMoved`,
`CursorMovedI`, `BufWritePost`); the pane is closed on `VimLeavePre`.

## Options

```lua
require("docp").setup({
  docp_bin = "docp",   -- name on PATH or an absolute path
  docp_args = {},      -- extra docp flags, before --ipc
  ratio = 50,          -- percent of the Rex split given to the preview
  debounce = 40,       -- ms between an editor scroll and its sync message
  takeover_ms = 120,   -- ms a preview-driven scroll suppresses editor echo
})
```

## How sync behaves

- Editor -> preview: the top visible line (`line("w0")`) is sent; the preview
  jumps to the block containing that line.
- Preview -> editor: scrolling the preview puts the top block's first line at
  the top of the window (`nvim_win_set_cursor` + `zt`).
- v1 sync is **block granular**; see
  [ARCHITECTURE §5.4](../../docs/ARCHITECTURE.md).
- Rendering still reloads on save. Saving resends the top line so the preview
  re-anchors.
- Anti-loop: a preview scroll sets `applying_until` (default 120 ms) so the
  editor does not echo it back; the preview never echoes editor-driven
  scrolls.

## Socket

`vim.fn.stdpath("run") .. "/docp-<bufnr>.sock"` (short enough for the 104-byte
Unix socket limit). docp is the server: it unlinks a stale socket at startup
and removes it on exit; the plugin retries the connect for a few seconds while
docp starts.
