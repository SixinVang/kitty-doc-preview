//! Kitty graphics protocol helpers: capability probing, image transmission
//! with Unicode-placeholder virtual placements, and placeholder encoding.
//!
//! Every command we emit uses `q=2` (suppress responses) so the interactive
//! input loop never sees protocol replies mixed into key events.

use base64::Engine as _;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::LazyLock;

/// Placeholder codepoint; the terminal draws the image whose id is encoded
/// in the foreground color of this character.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// A single virtual placement may span at most `max_rows()` rows (one
/// row-index diacritic per row). We stay under the limit for safety.
pub const MAX_TILE_ROWS: u16 = 280;

pub fn max_rows() -> u16 {
    DIACRITICS.len() as u16
}

static DIACRITICS: LazyLock<Vec<char>> = LazyLock::new(|| {
    include_str!("../assets/rowcolumn-diacritics.txt")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| u32::from_str_radix(l.split(';').next()?.trim(), 16).ok())
        .filter_map(char::from_u32)
        .collect()
});

static NEXT_ID: LazyLock<AtomicU32> = LazyLock::new(|| AtomicU32::new(random_start()));

fn random_start() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    (RandomState::new().hash_one(0u32) % 0x7f_fffe) as u32 + 1
}

/// Allocate an image id in `1..=2^23-1` (fits the 24-bit placeholder color
/// encoding) with a random start so concurrent programs rarely collide.
pub fn alloc_id() -> u32 {
    loop {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if (1..0x7f_ffff).contains(&id) {
            return id;
        }
        NEXT_ID.store(1, Ordering::Relaxed);
    }
}

/// Transmit `png` as image `id` and create a virtual placement of
/// `cols` x `rows` cells (Unicode placeholder based, `U=1`).
/// Payload is base64 chunked to 4096 bytes as the protocol requires.
pub fn transmit(out: &mut Vec<u8>, id: u32, png: &[u8], cols: u16, rows: u16) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let bytes = b64.as_bytes();
    let mut first = true;
    let mut pos = 0;
    while pos < bytes.len() {
        let end = (pos + 4096).min(bytes.len());
        let more = end < bytes.len();
        out.extend_from_slice(b"\x1b_G");
        if first {
            let _ = write!(
                out,
                "a=T,U=1,f=100,i={id},p=1,c={cols},r={rows},q=2,m={}",
                more as u8
            );
            first = false;
        } else {
            let _ = write!(out, "m={}", more as u8);
        }
        out.push(b';');
        out.extend_from_slice(&bytes[pos..end]);
        out.extend_from_slice(b"\x1b\\");
        pos = end;
    }
    if bytes.is_empty() {
        let _ = write!(
            out,
            "\x1b_Ga=T,U=1,f=100,i={id},p=1,c={cols},r={rows},q=2,m=0;\x1b\\"
        );
    }
}

/// Transmit an image without displaying it (`a=t`). A later `place()` picks a
/// source rectangle and display box, which allows sub-cell scrolling by
/// changing the crop without retransmitting pixels.
pub fn transmit_only(out: &mut Vec<u8>, id: u32, png: &[u8]) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let bytes = b64.as_bytes();
    let mut first = true;
    let mut pos = 0;
    while pos < bytes.len() {
        let end = (pos + 4096).min(bytes.len());
        let more = end < bytes.len();
        out.extend_from_slice(b"\x1b_G");
        if first {
            let _ = write!(out, "a=t,f=100,i={id},q=2,m={}", more as u8);
            first = false;
        } else {
            let _ = write!(out, "m={}", more as u8);
        }
        out.push(b';');
        out.extend_from_slice(&bytes[pos..end]);
        out.extend_from_slice(b"\x1b\\");
        pos = end;
    }
    if bytes.is_empty() {
        let _ = write!(out, "\x1b_Ga=t,f=100,i={id},q=2,m=0;\x1b\\");
    }
}

/// Place an already transmitted image at the cursor, displaying the source
/// band `[src_y, src_y + src_h)` (pixels) in a `cols` x `rows` cell box.
/// `C=1` keeps the cursor in place; callers position it with `CSI row;colH`.
#[allow(clippy::too_many_arguments)]
pub fn place(
    out: &mut Vec<u8>,
    id: u32,
    placement: u32,
    cols: u16,
    rows: u16,
    src_w: u32,
    src_y: u32,
    src_h: u32,
) {
    let _ = write!(
        out,
        "\x1b_Ga=p,i={id},p={placement},x=0,y={src_y},w={src_w},h={src_h},c={cols},r={rows},C=1,q=2\x1b\\"
    );
}

/// Delete one placement while keeping the image data (`d=i` with `p`).
pub fn delete_placement(out: &mut Vec<u8>, id: u32, placement: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={id},p={placement},q=2\x1b\\");
}

/// Resize an existing virtual placement; the terminal rescales the pixels it
/// already holds. No image data is retransmitted.
pub fn replace_placement(out: &mut Vec<u8>, id: u32, cols: u16, rows: u16) {
    let _ = write!(out, "\x1b_Ga=p,U=1,i={id},p=1,c={cols},r={rows},q=2\x1b\\");
}

/// Delete one image and free its data.
pub fn delete(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
}

/// Delete every image on screen and free the data (exit / full rebuild).
pub fn delete_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\");
}

/// One row of placeholder cells for image `id`: the id rides in the
/// foreground color, the first cell carries row/col diacritics and the
/// remaining cells inherit row/column from their left neighbour.
pub fn placeholder_row(id: u32, row: u16, cols: u16) -> String {
    let d = &*DIACRITICS;
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    let mut s = String::with_capacity(40 + cols as usize * 5);
    // Full reset first: a leftover background from a previous row would be
    // painted over the image and wash it out.
    let _ = write!(s, "\x1b[0m\x1b[38;2;{r};{g};{b}m");
    s.push(PLACEHOLDER);
    s.push(d[row as usize % d.len()]);
    s.push(d[0]);
    for _ in 1..cols {
        s.push(PLACEHOLDER);
    }
    s.push_str("\x1b[0m");
    s
}

/// Probe sequence body: cell size (CSI 16t), graphics support, Unicode
/// placeholder feature query. Without the DA1 terminator; callers composing
/// a full startup probe append `DA1` last so the reader knows when the
/// terminal is done replying.
pub fn probe_body() -> Vec<u8> {
    let mut v = Vec::new();
    // Appearance first: this is the authoritative light/dark signal and some
    // terminals (Rex) do not answer the fg/bg queries in every theme.
    v.extend_from_slice(b"\x1b[?996n");
    v.extend_from_slice(b"\x1b[16t");
    v.extend_from_slice(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    // feature query for Unicode placeholders; some terminals (Rex) require
    // transmission keys (dims/format) even for query actions
    v.extend_from_slice(b"\x1b_Ga=q,i=32,e=T,U=1,t=d,f=24,s=1,v=1;AAAA\x1b\\");
    v
}

/// Primary Device Attributes; terminals answer this last, so it terminates
/// a probe batch.
pub const DA1: &[u8] = b"\x1b[c";

/// The stock probe: body + DA1.
pub fn probe_queries() -> Vec<u8> {
    let mut v = probe_body();
    v.extend_from_slice(DA1);
    v
}

/// Query whether the terminal can read image data from a regular file
/// (`t=f` fast path). `png_len` is the byte size of the PNG at `path`.
pub fn file_transfer_query(id: u32, png_len: usize, path: &str) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(path);
    let mut v = Vec::new();
    let _ = write!(v, "\x1b_Gi={id},a=q,t=f,f=100,S={png_len};{b64}\x1b\\");
    v
}

/// Result of parsing a probe response stream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub graphics: bool,
    /// Some(true/false) when the feature query got a definitive answer;
    /// None when the terminal could not answer it (treated as "assume yes
    /// if graphics").
    pub placeholders: Option<bool>,
    /// Cell size (w, h) in pixels from a CSI 16t reply.
    pub cell: Option<(u16, u16)>,
    pub file_transfer: bool,
    /// A DA1 reply was seen (probe terminator).
    pub da1: bool,
    /// Light/dark preference from `CSI ? 996 n` (kitty color-scheme query).
    pub appearance: Option<Appearance>,
}

/// Terminal color-scheme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// Parse a probe response stream (see `probe_queries`). `file_id` is the id
/// used for the `t=f` query, if one was sent.
pub fn parse_probe(buf: &[u8], file_id: Option<u32>) -> Probe {
    let mut p = Probe::default();

    // CSI 16t reply: ESC [ 6 ; <height> ; <width> t
    if let Some(i) = find(buf, b"\x1b[6;") {
        let tail = &buf[i + 4..];
        if let Some(t) = find(tail, b"t") {
            let s = String::from_utf8_lossy(&tail[..t]);
            let parts: Vec<&str> = s.split(';').collect();
            if parts.len() == 2 {
                if let (Ok(h), Ok(w)) = (
                    parts[0].trim().parse::<u16>(),
                    parts[1].trim().parse::<u16>(),
                ) {
                    if h > 0 && w > 0 {
                        p.cell = Some((w, h));
                    }
                }
            }
        }
    }

    // graphics replies: ESC _ G <ctrl> ; <message> ESC \
    let mut rest = buf;
    while let Some(i) = find(rest, b"\x1b_G") {
        let after = &rest[i + 3..];
        let Some(end) = find(after, b"\x1b\\") else {
            break;
        };
        let payload = &after[..end];
        if let Some(pi) = find(payload, b";") {
            let ctrl = String::from_utf8_lossy(&payload[..pi]);
            let msg = String::from_utf8_lossy(&payload[pi + 1..])
                .trim()
                .to_string();
            let id = kv(&ctrl, "i");
            if msg == "OK" {
                match id.as_deref() {
                    Some("31") => p.graphics = true,
                    Some("32") => {
                        // Only trust a feature reply that echoes `e=` (the
                        // terminal understood the feature query). A plain OK
                        // (e.g. Rex) or EINVAL is inconclusive.
                        if ctrl.split(',').any(|kv| kv == "e=T") {
                            p.placeholders = Some(ctrl.split(',').any(|kv| kv == "U=1"));
                        }
                    }
                    _ => {}
                }
                if let Some(fid) = file_id {
                    if id.as_deref() == Some(&fid.to_string()) {
                        p.file_transfer = true;
                    }
                }
            }
        }
        rest = &after[end + 2..];
    }

    // DA1 reply: ESC [ ? ... c
    if let Some(i) = find(buf, b"\x1b[?") {
        for &b in &buf[i + 3..] {
            if b == b'c' {
                p.da1 = true;
                break;
            }
            if b == 0x1b {
                break;
            }
        }
    }

    // Color-scheme reply: ESC [ ? 997 ; 1 n (dark) or ; 2 n (light)
    if let Some(i) = find(buf, b"\x1b[?997;") {
        match buf.get(i + 7).copied() {
            Some(b'1') => p.appearance = Some(Appearance::Dark),
            Some(b'2') => p.appearance = Some(Appearance::Light),
            _ => {}
        }
    }
    p
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn kv(ctrl: &str, key: &str) -> Option<String> {
    ctrl.split(',')
        .find_map(|p| p.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diacritics_table_is_complete() {
        assert_eq!(DIACRITICS.len(), 297);
        assert_eq!(DIACRITICS[0], '\u{0305}');
        assert_eq!(DIACRITICS[1], '\u{030D}');
    }

    #[test]
    fn ids_stay_in_24bit_range() {
        let a = alloc_id();
        let b = alloc_id();
        assert_ne!(a, b);
        assert!((1..0x7f_ffff).contains(&a));
        assert!((1..0x7f_ffff).contains(&b));
    }

    #[test]
    fn placeholder_encodes_id_and_row() {
        let s = placeholder_row(0x010203, 2, 3);
        // Reset first, then the id color: an inherited background would be
        // composited over the image.
        assert!(s.starts_with("\x1b[0m\x1b[38;2;1;2;3m"), "{s:?}");
        let chars: Vec<char> = s.chars().filter(|c| *c == PLACEHOLDER).collect();
        assert_eq!(chars.len(), 3);
        assert!(s.ends_with("\x1b[0m"));
    }

    #[test]
    fn placeholder_row_diacritics() {
        let s = placeholder_row(42, 0, 2);
        assert!(s.contains(format!("{PLACEHOLDER}{}{}", DIACRITICS[0], DIACRITICS[0]).as_str()));
    }

    #[test]
    fn transmit_chunks() {
        let png = vec![7u8; 10_000];
        let mut out = Vec::new();
        transmit(&mut out, 5, &png, 10, 2);
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(s.starts_with("\x1b_Ga=T,U=1,f=100,i=5,p=1,c=10,r=2,q=2,m=1;"));
        assert!(s.contains("\x1b_Gm=1;"));
        assert!(s.contains("\x1b_Gm=0;"));
        assert_eq!(s.matches("\x1b_G").count(), 4);
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn transmit_small_is_single_chunk() {
        let png = vec![1u8; 100];
        let mut out = Vec::new();
        transmit(&mut out, 9, &png, 4, 2);
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(s.starts_with("\x1b_Ga=T,U=1,f=100,i=9,p=1,c=4,r=2,q=2,m=0;"));
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn parse_appearance() {
        let p = parse_probe(b"\x1b[?997;2n\x1b[?62;22c", None);
        assert_eq!(p.appearance, Some(Appearance::Light));
        let p = parse_probe(b"\x1b[?997;1n\x1b[?62;22c", None);
        assert_eq!(p.appearance, Some(Appearance::Dark));
        assert_eq!(parse_probe(b"\x1b[?62;22c", None).appearance, None);
    }

    #[test]
    fn parse_full_probe() {
        let buf = b"\x1b[6;28;14t\x1b_Gi=31;OK\x1b\\\x1b_Gi=32,e=T,U=1;OK\x1b\\\x1b[?62;1;2;6;9c";
        let p = parse_probe(buf, Some(33));
        assert!(p.graphics);
        assert_eq!(p.placeholders, Some(true));
        assert!(p.da1);
        assert_eq!(p.cell, Some((14, 28))); // (w, h)
        assert!(!p.file_transfer);
    }

    #[test]
    fn parse_no_graphics() {
        let p = parse_probe(b"\x1b[?62;22c", None);
        assert!(!p.graphics && p.placeholders.is_none() && p.da1);
    }

    #[test]
    fn parse_feature_without_placeholders() {
        let p = parse_probe(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32,e=T;OK\x1b\\\x1b[?62c", None);
        assert!(p.graphics);
        assert_eq!(p.placeholders, Some(false));
    }

    #[test]
    fn parse_feature_einval_is_inconclusive() {
        // Rex replies EINVAL to feature queries lacking dimensions
        let p = parse_probe(
            b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EINVAL: dimensions required\x1b\\\x1b[?62c",
            None,
        );
        assert!(p.graphics);
        assert_eq!(p.placeholders, None);
    }

    #[test]
    fn parse_feature_plain_ok_is_inconclusive() {
        // Rex accepts the query (with dims) but does not echo feature keys
        let p = parse_probe(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;OK\x1b\\\x1b[?62c", None);
        assert!(p.graphics);
        assert_eq!(p.placeholders, None);
    }

    #[test]
    fn parse_file_transfer_ok() {
        assert!(parse_probe(b"\x1b_Gi=33;OK\x1b\\", Some(33)).file_transfer);
        assert!(!parse_probe(b"\x1b_Gi=33;EBADF:x\x1b\\", Some(33)).file_transfer);
    }

    #[test]
    fn transmit_only_creates_no_placement() {
        let mut out = Vec::new();
        transmit_only(&mut out, 7, &[1u8; 100]);
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(s.starts_with("\x1b_Ga=t,f=100,i=7,q=2,m=0;"), "{s:?}");
        assert!(!s.contains("U=1") && !s.contains("c="));
    }

    #[test]
    fn place_encodes_crop_and_box() {
        let mut out = Vec::new();
        place(&mut out, 9, 1, 173, 64, 2422, 100, 2048);
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(
            s.starts_with("\x1b_Ga=p,i=9,p=1,x=0,y=100,w=2422,h=2048,c=173,r=64,C=1,q=2"),
            "{s:?}"
        );
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn delete_placement_keeps_image_data() {
        let mut out = Vec::new();
        delete_placement(&mut out, 9, 1);
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(s.contains("a=d,d=i,i=9,p=1"), "{s:?}");
    }
}
