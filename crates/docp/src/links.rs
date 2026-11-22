//! Markdown link targets: in-document anchors, relative file paths and
//! external URLs.
//!
//! Markdown keeps destinations verbatim, so `[x](#install)` and
//! `[y](docs/ARCH.md#54)` are just strings. This module turns them into
//! something both the view (click / `o` / OSC 8) and the status bar can use:
//!
//! - `#fragment` stays an in-document anchor and is never handed to the
//!   terminal as an OSC 8 URL (a bare `#…` means nothing to it).
//! - relative paths become absolute `file:` URLs, so cmd-click in the terminal
//!   works even outside docp, and docp itself can resolve them back to paths.
//! - `http:`, `mailto:` and friends are passed through untouched.

use std::path::{Path, PathBuf};

/// A destination decoded into what the viewer should do with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// A heading anchor inside the current document (fragment decoded).
    Anchor(String),
    /// Another file on disk, with an optional anchor inside it.
    File {
        path: PathBuf,
        anchor: Option<String>,
    },
    /// A URL the OS (or terminal) should open.
    External(String),
}

/// Split a destination at its `#fragment`.
pub fn split(dest: &str) -> (&str, Option<&str>) {
    match dest.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (dest, None),
    }
}

/// Whether `dest` uses a URL scheme the preview cannot resolve itself.
/// `file:` is treated as a path.
pub fn is_external(dest: &str) -> bool {
    let Some((scheme, _)) = dest.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid && !scheme.eq_ignore_ascii_case("file")
}

/// Decode `%xx` escapes (fragments and `file:` URLs are percent-encoded).
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode the bytes an OSC 8 `file:` URL cannot carry literally.
/// Non-ASCII is left alone: terminals and editors accept UTF-8 paths.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            ' ' => out.push_str("%20"),
            '\t' => out.push_str("%09"),
            c if c.is_ascii_control() => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// An absolute path as a `file:` URL, safe for OSC 8.
pub fn file_url(path: &Path) -> String {
    format!("file://{}", percent_encode(&path.to_string_lossy()))
}

/// Resolve the path part of a destination against the document's directory.
/// `file:` URLs and absolute paths are used as they are; the result is
/// absolute but not required to exist (the caller reports a missing file).
pub fn resolve(path: &str, base: Option<&Path>) -> PathBuf {
    let path = if let Some(rest) = strip_file_scheme(path) {
        PathBuf::from(percent_decode(rest))
    } else {
        // Destinations are URLs: `a%20b.md` means `a b.md`.
        PathBuf::from(percent_decode(path))
    };
    let joined = if path.is_absolute() {
        path
    } else {
        match base {
            Some(dir) => dir.join(path),
            None => path,
        }
    };
    // Normalise `..` so URLs and the watcher agree on the same file. Existing
    // paths resolve through the filesystem (symlinks and all); missing ones
    // (a broken link) are normalised lexically.
    std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize(&joined))
}

fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn strip_file_scheme(dest: &str) -> Option<&str> {
    let rest = dest.strip_prefix("file://")?;
    // `file:///abs/path` has an empty host; `file://localhost/...` is rare.
    Some(rest.strip_prefix("localhost").unwrap_or(rest))
}

/// The OSC 8 URL (and follow target) for a Markdown destination: anchors stay
/// `#fragment`, external URLs pass through, relative paths become `file:`
/// URLs with the fragment preserved.
pub fn localize(dest: &str, base: Option<&Path>) -> Option<String> {
    if dest.is_empty() {
        return None;
    }
    if dest.starts_with('#') {
        return Some(dest.to_string());
    }
    if is_external(dest) {
        return Some(dest.to_string());
    }
    let (target, frag) = split(dest);
    if target.is_empty() {
        return frag.map(|f| format!("#{f}"));
    }
    let mut url = file_url(&resolve(target, base));
    if let Some(frag) = frag.filter(|f| !f.is_empty()) {
        url.push('#');
        url.push_str(frag);
    }
    Some(url)
}

/// Decode a destination into an action for the viewer.
pub fn classify(dest: &str, base: Option<&Path>) -> Target {
    if dest.is_empty() {
        return Target::Anchor(String::new());
    }
    if let Some(frag) = dest.strip_prefix('#') {
        return Target::Anchor(percent_decode(frag));
    }
    if is_external(dest) {
        return Target::External(dest.to_string());
    }
    let (target, frag) = split(dest);
    let anchor = frag.map(percent_decode).filter(|f| !f.is_empty());
    if target.is_empty() {
        return Target::Anchor(anchor.unwrap_or_default());
    }
    Target::File {
        path: resolve(target, base),
        anchor,
    }
}

/// Extensions docp itself can preview and navigate between.
pub fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx")
    )
}

/// Follow `links::canonical` targets with the platform opener.
pub fn open_external(dest: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd)
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn splits_fragments() {
        assert_eq!(split("#top"), ("", Some("top")));
        assert_eq!(split("a.md#x"), ("a.md", Some("x")));
        assert_eq!(split("a.md"), ("a.md", None));
    }

    #[test]
    fn external_schemes_are_not_files() {
        assert!(is_external("https://example.com"));
        assert!(is_external("mailto:a@b"));
        assert!(!is_external("file:///tmp/a.md"));
        assert!(!is_external("a.md"));
        assert!(!is_external("#anchor"));
    }

    #[test]
    fn localize_keeps_anchors_and_resolves_paths() {
        let base = base();
        assert_eq!(
            localize("#install", Some(&base)).as_deref(),
            Some("#install")
        );
        assert_eq!(
            localize("https://x/y", Some(&base)).as_deref(),
            Some("https://x/y")
        );
        let url = localize("sub/a b.md#x", Some(&base)).unwrap();
        assert!(url.starts_with("file://"), "{url}");
        assert!(url.ends_with("/sub/a%20b.md#x"), "{url}");
        assert_eq!(localize("", Some(&base)), None);
    }

    #[test]
    fn classify_splits_files_and_anchors() {
        let base = base();
        assert_eq!(
            classify("#54-editor", Some(&base)),
            Target::Anchor("54-editor".into())
        );
        match classify("docs/A.md#intro", Some(&base)) {
            Target::File { path, anchor } => {
                assert!(path.ends_with("docs/A.md"), "{path:?}");
                assert_eq!(anchor.as_deref(), Some("intro"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            classify("https://x", Some(&base)),
            Target::External("https://x".into())
        );
    }

    #[test]
    fn file_urls_round_trip_through_resolve() {
        let path = std::env::temp_dir().join("docp 中文 #1.md");
        let url = file_url(&path);
        assert!(url.contains("%20"), "{url}");
        assert!(url.contains("%23"), "{url}");
        assert_eq!(resolve(&url, None), path);
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        // A malformed escape is left alone instead of eating the text.
        assert_eq!(percent_decode("100%"), "100%");
    }
}
