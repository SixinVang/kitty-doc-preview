//! Fonts for pixel rendering: embedded defaults (Inter, JetBrains Mono),
//! optionally replaced by user files or family names from the config.
//!
//! Measurement reads advance widths straight from the font tables so layout
//! agrees with what resvg later draws. Configuration must happen before the
//! first use; the CLI calls `configure` right after parsing arguments.

use resvg::usvg::{self, fontdb};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use ttf_parser::Face;

static INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
static INTER_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");
static DISPLAY_BOLD: &[u8] = include_bytes!("../assets/fonts/InterDisplay-Bold.ttf");
static DISPLAY_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/InterDisplay-SemiBold.ttf");
static MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

pub const DEFAULT_SANS: &str = "Inter";
pub const DEFAULT_DISPLAY: &str = "Inter Display";
pub const DEFAULT_MONO: &str = "JetBrains Mono";

/// User font configuration (from `[fonts]` in the config file).
#[derive(Clone, Debug, Default)]
pub struct FontSpec {
    /// Family for body/heading pixel text.
    pub sans: Option<String>,
    /// Family for monospace pixel text (code, diagram labels).
    pub mono: Option<String>,
    /// Font file backing `sans`; also used for measurement.
    pub sans_file: Option<String>,
    /// Font file backing `mono`.
    pub mono_file: Option<String>,
    /// Family for emoji runs (else the best installed emoji font is used).
    pub emoji: Option<String>,
    /// Font file backing `emoji`.
    pub emoji_file: Option<String>,
}

static SPEC: OnceLock<FontSpec> = OnceLock::new();

/// Set the font configuration. Call once, before any rendering.
pub fn configure(spec: FontSpec) {
    let _ = SPEC.set(spec);
}

fn spec() -> &'static FontSpec {
    SPEC.get_or_init(FontSpec::default)
}

struct Loaded {
    bytes: &'static [u8],
    face: Face<'static>,
}

fn load_file(path: &str) -> Option<Loaded> {
    let bytes = std::fs::read(Path::new(path)).ok()?;
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let face = Face::parse(leaked, 0).ok()?;
    Some(Loaded {
        bytes: leaked,
        face,
    })
}

/// Family name declared inside a font file.
fn family_of(face: &Face<'_>) -> Option<String> {
    face.names()
        .into_iter()
        .find(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
        .and_then(|n| n.to_string())
}

struct Text {
    user_sans: Option<Loaded>,
    user_mono: Option<Loaded>,
    user_emoji: Option<Loaded>,
    emoji_spec: Option<String>,
    regular: Face<'static>,
    semibold: Face<'static>,
    display_bold: Face<'static>,
    display_semibold: Face<'static>,
    mono: Face<'static>,
    sans_family: String,
    mono_family: String,
    display_family: String,
    /// Families configured by name only: their faces live in the system
    /// database, so measurement and rendering both have to resolve them there.
    sans_system: bool,
    mono_system: bool,
    display_system: bool,
}

static TEXT: LazyLock<Text> = LazyLock::new(|| {
    let s = spec();
    let user_sans = s.sans_file.as_deref().and_then(load_file);
    let user_mono = s.mono_file.as_deref().and_then(load_file);
    let user_emoji = s.emoji_file.as_deref().and_then(load_file);

    let sans_family = user_sans
        .as_ref()
        .and_then(|l| family_of(&l.face))
        .or_else(|| s.sans.clone())
        .unwrap_or_else(|| DEFAULT_SANS.to_string());
    let mono_family = user_mono
        .as_ref()
        .and_then(|l| family_of(&l.face))
        .or_else(|| s.mono.clone())
        .unwrap_or_else(|| DEFAULT_MONO.to_string());
    // Headings keep the display face unless the user chose their own sans.
    let display_family = if s.sans_file.is_some() || s.sans.is_some() {
        sans_family.clone()
    } else {
        DEFAULT_DISPLAY.to_string()
    };
    // A family given by name (no file) can only come from the system database.
    let sans_system = user_sans.is_none() && s.sans.is_some();
    let mono_system = user_mono.is_none() && s.mono.is_some();
    let display_system = sans_system || (s.sans.is_some() && s.sans_file.is_none());

    Text {
        user_sans,
        user_mono,
        user_emoji,
        emoji_spec: s.emoji.clone(),
        regular: Face::parse(INTER, 0).expect("Inter"),
        semibold: Face::parse(INTER_SEMIBOLD, 0).expect("Inter SemiBold"),
        display_bold: Face::parse(DISPLAY_BOLD, 0).expect("Inter Display Bold"),
        display_semibold: Face::parse(DISPLAY_SEMIBOLD, 0).expect("Inter Display SemiBold"),
        mono: Face::parse(MONO, 0).expect("JetBrains Mono"),
        sans_family,
        mono_family,
        display_family,
        sans_system,
        mono_system,
        display_system,
    }
});

/// Family name for body/heading pixel text.
pub fn sans_family() -> &'static str {
    &TEXT.sans_family
}

/// Family name for monospace pixel text.
pub fn mono_family() -> &'static str {
    &TEXT.mono_family
}

/// Family name for big-type headings.
pub fn display_family() -> &'static str {
    &TEXT.display_family
}

/// Which face to measure with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weight {
    Regular,
    SemiBold,
    DisplayBold,
    DisplaySemiBold,
    Mono,
}

/// Faces resolved from the system database, keyed by family and CSS weight.
/// Their bytes are copied once and leaked so the parsed `Face` can be cached
/// and used for measurement exactly like an embedded or file-backed face.
type FaceCache = Mutex<HashMap<(String, u16), Option<&'static Face<'static>>>>;
static SYSTEM_FACES: LazyLock<FaceCache> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn system_face(family: &str, css_weight: u16) -> Option<&'static Face<'static>> {
    let key = (family.to_string(), css_weight);
    if let Some(cached) = SYSTEM_FACES.lock().unwrap().get(&key) {
        return *cached;
    }
    let db = with_system();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight(css_weight),
        ..Default::default()
    };
    let resolved = db
        .query(&query)
        .and_then(|id| {
            db.with_face_data(id, |data, index| {
                let bytes: &'static [u8] = Box::leak(data.to_vec().into_boxed_slice());
                Face::parse(bytes, index).ok()
            })
            .flatten()
        })
        .map(|f| &*Box::leak(Box::new(f)));
    SYSTEM_FACES.lock().unwrap().insert(key, resolved);
    resolved
}

fn face(w: Weight) -> &'static Face<'static> {
    let t = &*TEXT;
    let pick = |file: &'static Option<Loaded>,
                system: bool,
                family: &str,
                css_weight: u16,
                embedded: &'static Face<'static>| {
        if let Some(l) = file {
            return &l.face;
        }
        if system {
            if let Some(f) = system_face(family, css_weight) {
                return f;
            }
        }
        embedded
    };
    match w {
        Weight::Mono => pick(&t.user_mono, t.mono_system, &t.mono_family, 400, &t.mono),
        Weight::Regular => pick(&t.user_sans, t.sans_system, &t.sans_family, 400, &t.regular),
        Weight::SemiBold => pick(
            &t.user_sans,
            t.sans_system,
            &t.sans_family,
            600,
            &t.semibold,
        ),
        Weight::DisplayBold => pick(
            &t.user_sans,
            t.display_system,
            &t.display_family,
            700,
            &t.display_bold,
        ),
        Weight::DisplaySemiBold => pick(
            &t.user_sans,
            t.display_system,
            &t.display_family,
            600,
            &t.display_semibold,
        ),
    }
}

/// Pele's measurer contract: style flags are 1 bold, 2 italic, 4 monospace.
pub fn measure(text: &str, size: f32, style: i32) -> f32 {
    let w = if style & 4 != 0 {
        Weight::Mono
    } else if style & 1 != 0 {
        Weight::SemiBold
    } else {
        Weight::Regular
    };
    width(text, size, w)
}

pub fn width(text: &str, size: f32, w: Weight) -> f32 {
    let f = face(w);
    let scale = size / f.units_per_em() as f32;
    let mut total = 0.0;
    for ch in text.chars() {
        total += match f.glyph_index(ch).and_then(|g| f.glyph_hor_advance(g)) {
            Some(adv) => adv as f32 * scale,
            // Glyphs the measured font lacks come from a system font. CJK and
            // other wide characters occupy a full em, not the ~0.6 em of a
            // Latin letter; getting this wrong makes diagrams and heading
            // wrapping overlap.
            None if ch == '\t' || ch.is_whitespace() => size * 0.28,
            None if is_wide(ch) => size,
            None => size * 0.62,
        };
    }
    total
}

fn is_wide(ch: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    ch.width() == Some(2)
}

/// True when the embedded database can resolve `family` on its own.
pub fn embedded_has_family(family: &str) -> bool {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..Default::default()
    };
    EMBEDDED.query(&query).is_some()
}

/// True when every non-space character exists in the embedded sans face.
pub fn covers(text: &str) -> bool {
    text.chars()
        .all(|c| c.is_whitespace() || TEXT.regular.glyph_index(c).is_some())
}

pub struct Metrics {
    pub cap_height: f32,
}

/// Font metrics in em units (multiply by the font size).
pub fn metrics(w: Weight) -> Metrics {
    let f = face(w);
    let upem = f.units_per_em() as f32;
    Metrics {
        cap_height: f.capital_height().unwrap_or(f.ascender() * 7 / 10) as f32 / upem,
    }
}

fn embedded_db() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    for data in [INTER, INTER_SEMIBOLD, DISPLAY_BOLD, DISPLAY_SEMIBOLD, MONO] {
        db.load_font_source(fontdb::Source::Binary(Arc::new(data)));
    }
    let t = &*TEXT;
    db.set_sans_serif_family(t.sans_family.clone());
    db.set_serif_family(t.sans_family.clone());
    db.set_monospace_family(t.mono_family.clone());
    if let Some(l) = &t.user_sans {
        db.load_font_source(fontdb::Source::Binary(Arc::new(l.bytes)));
    }
    if let Some(l) = &t.user_mono {
        db.load_font_source(fontdb::Source::Binary(Arc::new(l.bytes)));
    }
    if let Some(l) = &t.user_emoji {
        db.load_font_source(fontdb::Source::Binary(Arc::new(l.bytes)));
    }
    db
}

static EMBEDDED: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| Arc::new(embedded_db()));
static WITH_SYSTEM: OnceLock<Arc<fontdb::Database>> = OnceLock::new();

/// The fast database: only the embedded and configured fonts.
pub fn embedded() -> Arc<fontdb::Database> {
    EMBEDDED.clone()
}

/// Embedded fonts plus the system's, for text the embedded fonts cannot draw
/// (CJK, Hebrew, emoji). Loading system fonts takes tens of milliseconds, so
/// `warm_system` starts it in the background at startup.
pub fn with_system() -> Arc<fontdb::Database> {
    WITH_SYSTEM
        .get_or_init(|| {
            let mut db = embedded_db();
            db.load_system_fonts();
            // A family requested by name (no file) has to come from the system.
            let t = &*TEXT;
            db.set_sans_serif_family(t.sans_family.clone());
            db.set_monospace_family(t.mono_family.clone());
            Arc::new(db)
        })
        .clone()
}

/// Family used for emoji runs: the configured one, its file, or the best
/// installed emoji font (Noto Color Emoji before Apple's, since Noto tracks
/// Unicode releases and covers emoji Apple's font does not).
pub fn emoji_family() -> Option<&'static str> {
    static EMOJI: OnceLock<Option<String>> = OnceLock::new();
    EMOJI
        .get_or_init(|| {
            let t = &*TEXT;
            if let Some(l) = &t.user_emoji {
                if let Some(family) = family_of(&l.face) {
                    return Some(family);
                }
            }
            if let Some(family) = &t.emoji_spec {
                return Some(family.clone());
            }
            let db = with_system();
            ["Noto Color Emoji", "Apple Color Emoji"]
                .into_iter()
                .find(|name| {
                    db.query(&fontdb::Query {
                        families: &[fontdb::Family::Name(name)],
                        ..Default::default()
                    })
                    .is_some()
                })
                .map(str::to_string)
        })
        .as_deref()
}

/// Split a line into `(is_emoji, text)` runs.
///
/// Emoji presentation is not just "codepoint is in an emoji block": `©️`,
/// `™️`, `↔️`, `‼️`, `1️⃣` start with a character the CJK/Latin font also has,
/// so without a run forcing the emoji family they render as monochrome text
/// and the variation selector is dropped. A base character followed by
/// U+FE0F (VS16) is an emoji by definition, and ZWJ joins the cluster.
pub fn emoji_runs(text: &str) -> Vec<(bool, &str)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut flags = Vec::with_capacity(chars.len());
    for (i, &(_, c)) in chars.iter().enumerate() {
        let next = chars.get(i + 1).map(|(_, c)| *c);
        let prev = i.checked_sub(1).map(|p| chars[p].1);
        let emoji = is_emoji_char(c)
            || next == Some('\u{FE0F}')
            || prev == Some('\u{200D}')
            || matches!(c, '\u{200D}' | '\u{FE0F}' | '\u{20E3}')
            || ('\u{E0020}'..='\u{E007F}').contains(&c);
        flags.push(emoji);
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let emoji = flags[start];
        let mut end = start + 1;
        while end < chars.len() && flags[end] == emoji {
            end += 1;
        }
        let byte_start = chars[start].0;
        let byte_end = chars.get(end).map(|(b, _)| *b).unwrap_or(text.len());
        out.push((emoji, &text[byte_start..byte_end]));
        start = end;
    }
    out
}

/// Font resolver for resvg.
///
/// The default fallback selector walks `fontdb` in load order, and on macOS
/// the first face that claims to support every character is often
/// `.LastResort` (or another dot-prefixed private face): it maps all of
/// Unicode to a "tofu" glyph, so emoji and other missing characters render as
/// boxes with a question mark. Skip those faces, and steer emoji toward an
/// emoji font so color glyphs are drawn instead of whatever came first.
pub fn font_resolver() -> usvg::FontResolver<'static> {
    usvg::FontResolver {
        select_font: usvg::FontResolver::default_font_selector(),
        select_fallback: Box::new(|c, used, db| {
            let base = db.face(*used.first()?)?;
            let mut best: Option<(u32, fontdb::ID)> = None;
            for face in db.faces() {
                if used.contains(&face.id) || is_private(face) || !has_char(db, face.id, c) {
                    continue;
                }
                let mut score = 0u32;
                let emoji_face = face.families.iter().any(|(f, _)| f.contains("Emoji"));
                if is_emoji_char(c) {
                    let wanted = emoji_family();
                    let is_wanted = wanted
                        .map(|w| face.families.iter().any(|(f, _)| f == w))
                        .unwrap_or(false);
                    score += if is_wanted {
                        0
                    } else if wanted.is_some() {
                        200
                    } else if emoji_face {
                        0
                    } else {
                        1000
                    };
                } else if emoji_face {
                    score += 1000;
                } else if is_wide(c) {
                    // With no family configured the base is Inter, which has no
                    // CJK at all. Prefer the system's own CJK face over
                    // whatever fontdb happens to list first (Hiragino, Apple SD
                    // Gothic Neo, Arial Unicode…), so default headings look
                    // like the rest of macOS.
                    score += cjk_rank(face) * 100;
                }
                if face.style != base.style {
                    score += 20;
                }
                score += (i32::from(face.weight.0) - i32::from(base.weight.0)).unsigned_abs();
                if best.is_none_or(|(b, _)| score < b) {
                    best = Some((score, face.id));
                }
            }
            best.map(|(_, id)| id)
        }),
    }
}

/// Preference among system CJK faces, lower is better. Only consulted when the
/// configured/inline font lacks the character (fallback).
fn cjk_rank(face: &fontdb::FaceInfo) -> u32 {
    let first = face.families.first().map(|(f, _)| f.as_str()).unwrap_or("");
    match first {
        "PingFang SC" => 0,
        "PingFang TC" | "PingFang HK" | "PingFang MO" => 5,
        "Hiragino Sans GB" => 10,
        "Source Han Sans SC" | "Noto Sans CJK SC" => 15,
        "Heiti SC" => 20,
        // Korean glyph forms for Han characters, and fallback-only faces.
        "Apple SD Gothic Neo" => 60,
        _ => 30,
    }
}

/// Dot-prefixed families are private system faces; `.LastResort` in particular
/// answers "yes" for every character while drawing the same placeholder.
fn is_private(face: &fontdb::FaceInfo) -> bool {
    face.families
        .iter()
        .any(|(f, _)| f.starts_with('.') || f.eq_ignore_ascii_case("LastResort"))
}

fn has_char(db: &fontdb::Database, id: fontdb::ID, c: char) -> bool {
    db.with_face_data(id, |data, index| {
        ttf_parser::Face::parse(data, index)
            .ok()
            .and_then(|face| face.glyph_index(c))
            .is_some()
    }) == Some(true)
}

/// True for emoji-presentation characters and the joiners that build them.
pub fn is_emoji_char(c: char) -> bool {
    matches!(
        c as u32,
        0x1F000..=0x1FAFF // pictographs, emoticons, flags, transport
            | 0x2600..=0x27BF // misc symbols and dingbats
            | 0x2B00..=0x2BFF   // arrows and geometric shapes
            | 0x200D            // zero-width joiner
            | 0xFE0F            // variation selector-16
            | 0x20E3            // combining enclosing keycap
    )
}

pub fn warm_system() {
    std::thread::Builder::new()
        .name("fonts".into())
        .spawn(|| {
            with_system();
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_wider_for_longer_text() {
        let a = width("Hello", 16.0, Weight::Regular);
        let b = width("Hello world", 16.0, Weight::Regular);
        assert!(a > 20.0 && b > a);
    }

    #[test]
    fn bold_is_wider_than_regular() {
        assert!(measure("Markdown", 16.0, 1) > measure("Markdown", 16.0, 0));
    }

    #[test]
    fn coverage() {
        assert!(covers("Plan: phase 2 (draft)"));
        assert!(!covers("שלום"));
    }

    #[test]
    fn wide_glyphs_measure_one_em() {
        let one = width("中", 16.0, Weight::Regular);
        assert!((one - 16.0).abs() < 0.5, "{one}");
        let three = width("中文字", 16.0, Weight::Regular);
        assert!((three - 48.0).abs() < 1.5, "{three}");
    }

    #[test]
    fn latin_wide_is_narrower_than_cjk() {
        let latin = width("iii", 16.0, Weight::Regular);
        let cjk = width("中", 16.0, Weight::Regular);
        assert!(latin < cjk);
    }

    #[test]
    fn embedded_families_are_known() {
        assert!(embedded_has_family(DEFAULT_SANS));
        assert!(embedded_has_family(DEFAULT_DISPLAY));
        assert!(embedded_has_family(DEFAULT_MONO));
        // Not shipped with docp: must come from the system database.
        assert!(!embedded_has_family("PingFang SC"));
    }

    #[test]
    fn emoji_runs_follow_variation_selectors() {
        // Variation selector 16 turns a text-default character into an emoji.
        assert_eq!(
            emoji_runs("准备❤\u{FE0F}Podman"),
            vec![(false, "准备"), (true, "❤\u{FE0F}"), (false, "Podman")]
        );
        assert_eq!(emoji_runs("©\u{FE0F}")[0].0, true);
        assert_eq!(emoji_runs("©")[0].0, false);
        assert_eq!(emoji_runs("1\u{FE0F}\u{20E3}")[0].0, true);
        // ZWJ clusters stay in one run.
        let runs = emoji_runs("👨\u{200D}👩\u{200D}👧");
        assert_eq!(runs.len(), 1);
        assert!(runs[0].0);
        // Text stays text.
        assert_eq!(emoji_runs("准备运行环境"), vec![(false, "准备运行环境")]);
    }

    #[test]
    fn default_families() {
        assert_eq!(sans_family(), DEFAULT_SANS);
        assert_eq!(mono_family(), DEFAULT_MONO);
        assert_eq!(display_family(), DEFAULT_DISPLAY);
    }
}
