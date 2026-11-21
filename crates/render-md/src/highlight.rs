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

/// Highlight `text` (a whole code block) as `lang`, returning pieces per
/// line. Falls back to plain pieces when the language is unknown or the
/// `syntaxes` feature is off.
pub fn lines(lang: &str, text: &str, theme: &Theme) -> Vec<Vec<Piece>> {
    let raw = text.trim_end_matches('\n');
    match highlighted(lang, raw, theme) {
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
                let fg = if c.r == 0 && c.g == 0 && c.b == 0 && c.a == 0 {
                    None // unstyled: inherit
                } else {
                    Some(Rgb(c.r, c.g, c.b))
                };
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

        sel("comment", t.muted(), Some(FontStyle::ITALIC));
        sel("string", t.green(), None);
        sel("constant.numeric", t.yellow(), None);
        sel("constant.language", t.yellow(), None);
        sel("constant.character.escape", t.red(), None);
        sel("keyword", t.purple(), None);
        sel("keyword.operator", t.faint(), None);
        sel("variable", t.fg, None);
        sel("variable.function", t.fg, None);
        sel("function", t.accent(), None);
        sel("function.call", t.accent(), None);
        sel("support.function", t.cyan(), None);
        sel("entity.name.type", t.cyan(), None);
        sel("entity.name.tag", t.red(), None);
        sel("entity.other.attribute-name", t.yellow(), None);
        sel("support.type", t.cyan(), None);
        sel("support.class", t.cyan(), None);
        sel("support.constant", t.cyan(), None);
        sel("storage", t.purple(), None);
        sel("meta.tag", t.fg, None);
        sel("punctuation", t.faint(), None);

        Theme {
            name: Some("docp".into()),
            author: Some("docp".into()),
            scopes: items,
            settings: ThemeSettings::default(),
        }
    }
}

#[cfg(feature = "syntaxes")]
use syn::highlighted;

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
}
