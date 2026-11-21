//! Syntax highlighting for code blocks, mapped onto the active theme so
//! highlighted code always matches the terminal's colors.

use docmodel::{Rgb, Theme};

pub struct Piece {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
}

impl std::fmt::Debug for Piece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Piece")
            .field("text", &self.text)
            .field("fg", &self.fg)
            .finish()
    }
}

/// Fence info strings that mean "no language"; skip syntect entirely.
fn is_plain(lang: &str) -> bool {
    matches!(
        lang.trim(),
        "" | "text" | "txt" | "plain" | "plaintext" | "none"
    )
}

/// Fence aliases that Sublime syntax names/extensions do not cover.
fn alias(lang: &str) -> &str {
    match lang.trim() {
        "console" | "shell-session" | "terminal" | "shellsession" => "sh",
        other => other,
    }
}

/// Highlight `text` (a whole code block) as `lang`, returning pieces per
/// line. Falls back to plain pieces when the language is unknown or the
/// `syntaxes` feature is off.
pub fn lines(lang: &str, text: &str, theme: &Theme) -> Vec<Vec<Piece>> {
    let raw = text.trim_end_matches('\n');
    let lang = alias(lang);
    let hit = if is_plain(lang) {
        None
    } else {
        highlighted(lang, raw, theme)
    };
    match hit {
        Some(v) => v,
        None => raw
            .split('\n')
            .map(|l| {
                vec![Piece {
                    text: l.to_string(),
                    fg: None,
                    bold: false,
                    italic: false,
                }]
            })
            .collect(),
    }
}

#[cfg(feature = "syntaxes")]
mod syn {
    use super::*;
    use std::sync::LazyLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::{
        Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
    };
    use syntect::parsing::{SyntaxSet, SyntaxSetBuilder};

    static PLAIN: LazyLock<SyntaxSet> = LazyLock::new(|| {
        let mut b = SyntaxSetBuilder::new();
        b.add_plain_text_syntax();
        b.build()
    });
    static FULL: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

    /// Force the lazy syntax dump to load now.
    pub fn preload() {
        let _ = &*FULL;
    }

    pub fn highlighted(lang: &str, text: &str, theme: &super::Theme) -> Option<Vec<Vec<Piece>>> {
        let syntax = if lang.is_empty() {
            PLAIN.find_syntax_plain_text()
        } else {
            FULL.find_syntax_by_token(lang)?
        };
        let syn_theme = build_theme(theme);
        let mut hl = HighlightLines::new(syntax, &syn_theme);
        let mut out = Vec::new();
        for line in text.split('\n') {
            let mut pieces = Vec::new();
            for (style, chunk) in hl.highlight_line(line, &FULL).ok()? {
                if chunk.is_empty() {
                    continue;
                }
                let c = style.foreground;
                let rgb = Rgb(c.r, c.g, c.b);
                // The theme's default foreground is the terminal's; keeping it
                // unset here lets the code block's base attrs inherit it.
                let fg = if rgb == theme.fg { None } else { Some(rgb) };
                pieces.push(Piece {
                    text: chunk.to_string(),
                    fg,
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
            out.push(pieces);
        }
        Some(out)
    }

    /// Build a syntect theme whose scopes are mapped onto our theme's
    /// colors, so highlighting follows the terminal palette.
    fn build_theme(t: &super::Theme) -> Theme {
        let mut items = Vec::new();
        let mut sel = |scope: &str, fg: Rgb, style: Option<FontStyle>| {
            let selector: ScopeSelectors = scope.parse().expect(scope);
            items.push(ThemeItem {
                scope: selector,
                style: StyleModifier {
                    foreground: Some(Color {
                        r: fg.0,
                        g: fg.1,
                        b: fg.2,
                        a: 0xff,
                    }),
                    background: None,
                    font_style: style,
                },
            });
        };

        // Comments and literals.
        sel("comment", t.muted(), Some(FontStyle::ITALIC));
        sel("string", t.green(), None);
        sel("constant.numeric", t.yellow(), None);
        sel("constant.language", t.yellow(), None);
        sel("constant.other", t.yellow(), None);
        sel("constant.character.escape", t.red(), None);
        // Keywords, operators and storage.
        sel("keyword", t.purple(), None);
        sel("keyword.operator", t.faint(), None);
        sel("storage", t.purple(), None);
        // TextMate selectors match scope prefixes, so `function` never matches
        // `entity.name.function`; spell the scopes out instead.
        sel("entity.name.function", t.accent(), None);
        sel("entity.name.method", t.accent(), None);
        sel("entity.name.type", t.cyan(), None);
        sel("entity.name.class", t.cyan(), None);
        sel("entity.name.struct", t.cyan(), None);
        sel("entity.name.enum", t.cyan(), None);
        sel("entity.name.trait", t.cyan(), None);
        sel("entity.name.tag", t.red(), None);
        sel("entity.other.attribute-name", t.yellow(), None);
        sel("variable", t.fg, None);
        sel("variable.parameter", t.fg, None);
        sel("variable.other.property", t.fg, None);
        sel("variable.function", t.fg, None);
        sel("support.function", t.cyan(), None);
        sel("support.type", t.cyan(), None);
        sel("support.class", t.cyan(), None);
        sel("support.constant", t.cyan(), None);
        sel("meta.tag", t.fg, None);
        sel("punctuation", t.faint(), None);
        sel("invalid", t.red(), None);

        Theme {
            name: Some("docp".into()),
            author: Some("docp".into()),
            scopes: items,
            settings: ThemeSettings {
                // syntect's fallback foreground is opaque black, which is
                // invisible on dark terminals; use the terminal's own fg so
                // unmatched text just inherits.
                foreground: Some(Color {
                    r: t.fg.0,
                    g: t.fg.1,
                    b: t.fg.2,
                    a: 0xff,
                }),
                background: Some(Color {
                    r: t.code_bg().0,
                    g: t.code_bg().1,
                    b: t.code_bg().2,
                    a: 0xff,
                }),
                ..ThemeSettings::default()
            },
        }
    }
}

#[cfg(feature = "syntaxes")]
use syn::highlighted;

/// Load the embedded syntax set eagerly (a ~1.4 MB compressed dump). Call it
/// from a worker thread at startup so the first code block does not pay for it.
#[cfg(feature = "syntaxes")]
pub fn preload() {
    syn::preload();
}

/// No-op when highlighting is compiled out.
#[cfg(not(feature = "syntaxes"))]
pub fn preload() {}

#[cfg(not(feature = "syntaxes"))]
fn highlighted(_lang: &str, _text: &str, _theme: &Theme) -> Option<Vec<Vec<Piece>>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_when_unknown_language() {
        let t = Theme::dark_default();
        let lines = lines("definitely-not-a-language", "let x = 1;", &t);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].text, "let x = 1;");
        assert_eq!(lines[0][0].fg, None);
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn rust_keywords_get_color() {
        let t = Theme::dark_default();
        let lines = lines("rust", "fn main() {}", &t);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|p| p.text.clone()))
            .collect();
        assert_eq!(joined, "fn main() {}");
        // `fn` should carry a color (keyword purple)
        let first = &lines[0][0];
        assert!(first.fg.is_some(), "{first:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn keyword_string_and_comment_differ() {
        let t = Theme::dark_default();
        let lines = lines("rust", "fn f() { let s = \"hi\"; } // note", &t);
        let colors: Vec<Rgb> = lines[0].iter().filter_map(|p| p.fg).collect();
        assert!(colors.contains(&t.purple()), "keyword: {colors:?}");
        assert!(colors.contains(&t.green()), "string: {colors:?}");
        assert!(colors.contains(&t.muted()), "comment: {colors:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn comment_state_carries_across_lines() {
        let t = Theme::dark_default();
        let lines = lines("js", "/* a\n b */\nlet x = 1;", &t);
        assert!(
            lines[1].iter().any(|p| p.fg == Some(t.muted())),
            "second line left the comment: {lines:?}"
        );
        assert!(
            lines[2].iter().any(|p| p.fg == Some(t.purple())),
            "keyword after the comment: {lines:?}"
        );
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn unmatched_text_inherits_theme_fg() {
        let t = Theme::dark_default();
        let lines = lines("rust", "let abc = 1;", &t);
        let ident = lines[0]
            .iter()
            .find(|p| p.text.contains("abc"))
            .expect("identifier piece");
        assert_eq!(ident.fg, None, "{ident:?}");
    }

    #[test]
    fn plain_aliases_skip_syntax() {
        let t = Theme::dark_default();
        for lang in ["", "text", "txt", "plaintext"] {
            let lines = lines(lang, "fn main() {}", &t);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0][0].text, "fn main() {}");
            assert_eq!(lines[0][0].fg, None, "lang {lang:?}");
        }
    }
}
