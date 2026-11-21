//! End-to-end regression: fenced code must reach stdout with syntax colors.
//!
//! The renderer can produce colored pieces while the terminal writer drops
//! them (a cell style flush bug), so this runs the real binary in `--cat`
//! mode instead of only unit-testing `render-md`.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

fn run_cat(markdown: &str) -> String {
    // Each test gets its own file: tests run in parallel and must not delete
    // each other's temp dirs.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("docp-hl-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("code.md");
    std::fs::write(&path, markdown).expect("write markdown");

    let out = Command::new(env!("CARGO_BIN_EXE_docp"))
        .args(["--cat", "--theme", "dark"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .expect("run docp");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "docp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn fenced_code_is_highlighted_in_cat_mode() {
    let out = run_cat("```rust\nfn main() { let x = 1; }\n```\n");
    assert!(out.contains("38;2;"), "no syntax colors in: {out:?}");
}

#[test]
fn plain_markdown_has_no_foreground_colors() {
    let out = run_cat("just some prose\n");
    assert!(!out.contains("38;2;"), "unexpected color in: {out:?}");
}
