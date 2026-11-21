# docp

Preview PDFs, images and Markdown in terminals that speak the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
(macOS + Rex first).

Status: **M0** — image viewer. Markdown (M1), PDF (M2/M3) coming up.

```
cargo build --release
target/release/docp gen-test /tmp/docp-test.png
target/release/docp /tmp/docp-test.png
```

Keys: `q` quit · `j/k` scroll · `g/G` top/bottom · `d/u` half page · `r` reload · mouse wheel.

Architecture: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
