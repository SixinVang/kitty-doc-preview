//! Syntax highlighting for code blocks, mapped onto the active theme so
//! highlighted code always matches the terminal's colors.
//!
//! The syntax definitions are bat's (`two-face`): ~190 languages, far more
//! than syntect's built-in set, including TOML, Dockerfile, TypeScript, Nix,
//! Protobuf and friends. Fence info strings are resolved leniently (aliases,
//! extensions, syntax names, scopes, shebangs), and every line is handed to
//! syntect newline-terminated so line-scoped constructs (`# note`, `-- note`)
//! close where they should.

use docmodel::{Rgb, Theme};

pub struct Piece {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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
        "text" | "txt" | "plain" | "plaintext" | "none" | "output"
    )
}

/// Highlight `text` (a whole code block) as `lang`, returning pieces per
/// line. Falls back to plain pieces when the language is unknown or the
/// `syntaxes` feature is off.
pub fn lines(lang: &str, text: &str, theme: &Theme) -> Vec<Vec<Piece>> {
    let raw = text.trim_end_matches('\n');
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
                    underline: false,
                }]
            })
            .collect(),
    }
}

/// Number of bundled syntax definitions.
pub fn syntax_count() -> usize {
    #[cfg(feature = "syntaxes")]
    {
        syn::syntax_count()
    }
    #[cfg(not(feature = "syntaxes"))]
    {
        0
    }
}

#[cfg(feature = "syntaxes")]
mod syn {
    use super::*;
    use std::sync::LazyLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::{
        Color, FontStyle, ScopeSelectors, StyleModifier, ThemeItem, ThemeSettings,
    };
    use syntect::highlighting::Theme as SynTheme;
    use syntect::parsing::{SyntaxReference, SyntaxSet};

    /// bat's syntax set. Loading is expensive (a ~960 KB packed dump), so it
    /// happens once, on the preload worker thread when the app starts.
    pub(super) static SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

    /// Force the lazy syntax dump to load now.
    pub fn preload() {
        let _ = &*SET;
    }

    pub fn syntax_count() -> usize {
        SET.syntaxes().len()
    }

    /// Fence aliases that neither an extension nor a syntax name covers.
    fn aliases(key: &str) -> &'static [&'static str] {
        match key {
            "console" | "shell-session" | "shellsession" | "sh-session" | "terminal" | "session" => {
                &["sh"]
            }
            "shell" => &["ShellScript", "sh"],
            "docker" | "containerfile" | "dockerfile" => &["Dockerfile", "dockerfile"],
            "make" | "mk" | "gnumake" | "bsdmake" => &["Makefile", "makefile"],
            "md" | "mdx" | "markdown" | "gfm" => &["Markdown", "markdown"],
            "golang" => &["Go", "go"],
            "yml" => &["yaml"],
            "cxx" | "cplusplus" => &["C++", "cpp"],
            "csharp" => &["C#", "cs"],
            "objc" | "objectivec" => &["Objective-C", "m"],
            "protobuf" | "proto3" => &["Protocol Buffer", "proto"],
            "terraform" | "tf" => &["Terraform", "tf"],
            "pwsh" | "powershell" | "ps1" => &["PowerShell", "ps1"],
            "batch" | "bat" | "cmd" => &["Batch File", "bat"],
            "env" | "dotenv" => &["DotENV", "env"],
            "gitignore" => &["Git Ignore", "gitignore"],
            "jsx" => &["JavaScript (Babel)", "jsx", "javascript"],
            "tsx" => &["TSX", "tsx", "typescript"],
            "vue" => &["Vue Component", "vue", "html"],
            "svelte" => &["Svelte", "svelte", "html"],
            "latex" | "tex" => &["LaTeX", "tex"],
            "asm" | "nasm" | "assembly" => &["Assembly (x86_64)", "asm", "s"],
            "pl" | "perl6" | "raku" => &["Perl", "pl"],
            "rb" => &["Ruby", "rb"],
            "py" | "python3" | "pycon" => &["Python", "py"],
            "rs" => &["Rust", "rs"],
            "kt" | "kts" => &["Kotlin", "kt"],
            "hs" => &["Haskell", "hs"],
            "ex" | "exs" => &["Elixir", "ex"],
            "erl" => &["Erlang", "erl"],
            "clj" | "cljs" => &["Clojure", "clj"],
            "scss" => &["SCSS", "scss"],
            "htm" => &["HTML", "html"],
            "jsonc" | "json5" => &["JSON", "json"],
            "http" | "rest" => &["HTTP", "http"],
            "reg" | "regex" => &["Regular Expression", "regexp"],
            _ => &[],
        }
    }

    /// Normalise a fence info string: `Python {linenos=table}` → `python`,
    /// `language-rs` → `rs`, `Rust,ignore` → `rust`.
    fn normalize(lang: &str) -> String {
        let lower = lang.trim().to_ascii_lowercase();
        let lower = lower.strip_prefix("language-").unwrap_or(&lower);
        let key: String = lower
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'))
            .collect();
        key.trim_matches('.').to_string()
    }

    /// Comparison key for a syntax name: `C++` → `c++`, `Objective-C` →
    /// `objectivec`. Symbols are kept so `c` and `c++` stay distinct.
    fn name_key(name: &str) -> String {
        name.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#'))
            .collect::<String>()
            .to_ascii_lowercase()
    }

    fn by_name<'a>(set: &'a SyntaxSet, key: &str) -> Option<&'a SyntaxReference> {
        let want = name_key(key);
        if want.is_empty() {
            return None;
        }
        set.syntaxes().iter().find(|s| name_key(&s.name) == want)
    }

    fn by_extension<'a>(set: &'a SyntaxSet, key: &str) -> Option<&'a SyntaxReference> {
        if key.is_empty() {
            return None;
        }
        set.syntaxes()
            .iter()
            .find(|s| s.file_extensions.iter().any(|e| e.eq_ignore_ascii_case(key)))
    }

    fn by_scope<'a>(set: &'a SyntaxSet, key: &str) -> Option<&'a SyntaxReference> {
        if key.is_empty() {
            return None;
        }
        set.syntaxes().iter().find(|s| {
            s.scope
                .to_string()
                .to_ascii_lowercase()
                .split('.')
                .any(|seg| seg == key)
        })
    }

    /// Resolve a fence key. Names win over extensions: `rs` is an extension of
    /// both Rust and RenderScript, and a fence saying `rs` means Rust.
    fn by_key<'a>(set: &'a SyntaxSet, key: &str) -> Option<&'a SyntaxReference> {
        by_name(set, key)
            .or_else(|| by_extension(set, key))
            .or_else(|| by_scope(set, key))
    }

    fn by_shebang<'a>(set: &'a SyntaxSet, text: &str) -> Option<&'a SyntaxReference> {
        let first = text.lines().find(|l| !l.trim().is_empty())?;
        if !first.starts_with("#!") {
            return None;
        }
        // `find_syntax_by_first_line` wants the interactive line as written.
        set.find_syntax_by_first_line(first)
            .or_else(|| set.find_syntax_by_first_line(&first[2..]))
            .or_else(|| {
                // `#!/usr/bin/env python3` → try the interpreter words.
                first[2..]
                    .split_whitespace()
                    .filter(|w| !w.starts_with('-') && !w.contains('/'))
                    .find_map(|w| by_key(set, &normalize(w)))
            })
    }

    pub fn resolve(lang: &str, text: &str) -> Option<&'static SyntaxReference> {
        let set = &*SET;
        let key = normalize(lang);
        if let Some(s) = by_name(set, &key) {
            return Some(s);
        }
        for alias in aliases(&key) {
            for candidate in [normalize(alias), alias.to_ascii_lowercase()] {
                if let Some(s) = by_name(set, &candidate).or_else(|| by_extension(set, &candidate)) {
                    return Some(s);
                }
            }
        }
        by_key(set, &key).or_else(|| by_shebang(set, text))
    }

    pub fn highlighted(lang: &str, text: &str, theme: &super::Theme) -> Option<Vec<Vec<Piece>>> {
        let syntax = resolve(lang, text)?;
        highlight_syntax(syntax, text, theme)
    }

    pub fn highlight_syntax(
        syntax: &SyntaxReference,
        text: &str,
        theme: &super::Theme,
    ) -> Option<Vec<Vec<Piece>>> {
        let syn_theme = build_theme(theme);
        let mut hl = HighlightLines::new(syntax, &syn_theme);
        let mut out = Vec::new();
        for line in text.split('\n') {
            // The syntaxes are line-ending aware (`extra_newlines`): constructs
            // such as `# comment` or `-- line comment` only close on a trailing
            // newline, so feeding the line bare leaves the comment scope open
            // and paints every following line as a comment.
            let mut terminated = String::with_capacity(line.len() + 1);
            terminated.push_str(line);
            terminated.push('\n');
            let mut pieces = Vec::new();
            for (style, chunk) in hl.highlight_line(&terminated, &SET).ok()? {
                let chunk = chunk.strip_suffix('\n').unwrap_or(chunk);
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
                    underline: style.font_style.contains(FontStyle::UNDERLINE),
                });
            }
            out.push(pieces);
        }
        Some(out)
    }

    /// Build a syntect theme whose scopes are mapped onto our theme's colors,
    /// so highlighting follows the terminal palette.
    ///
    /// Entries apply in order and the last match wins, so this reads
    /// general → specific: punctuation is dimmed first, then strings paint
    /// their own delimiters, and so on.
    fn build_theme(t: &super::Theme) -> SynTheme {
        const BOLD: FontStyle = FontStyle::BOLD;
        const ITALIC: FontStyle = FontStyle::ITALIC;
        const UNDERLINE: FontStyle = FontStyle::UNDERLINE;
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

        // Punctuation and meta scaffolding: dimmed, then re-colored by the
        // construct it belongs to.
        sel("punctuation", t.faint(), None);
        sel("punctuation.definition.comment", t.muted(), Some(ITALIC));
        sel("punctuation.definition.string", t.green(), None);
        sel("punctuation.section.embedded", t.accent(), None);
        sel("meta.tag", t.fg, None);

        // Comments.
        sel("comment", t.muted(), Some(ITALIC));
        sel("comment.documentation", t.muted(), Some(ITALIC | BOLD));

        // Strings and regular expressions.
        sel("string", t.green(), None);
        // YAML/TOML/JS template bodies scope unquoted text as `string`; keep
        // those neutral so a YAML block is not one wall of green.
        sel("string.unquoted", t.fg, None);
        sel("string.interpolated", t.accent(), None);
        sel("string.regexp", t.yellow(), None);
        sel("string.other.link", t.accent(), Some(UNDERLINE));

        // Constants and literals.
        sel("constant.character.escape", t.red(), None);
        sel("constant.character", t.red(), None);
        sel("constant.numeric", t.yellow(), None);
        sel("constant.language", t.purple(), None);
        sel("constant.other.placeholder", t.accent(), Some(BOLD));
        sel("constant.other", t.yellow(), None);

        // Keywords, operators, storage.
        sel("storage", t.purple(), None);
        sel("keyword", t.purple(), None);
        sel("keyword.operator.word", t.purple(), None);
        sel("keyword.operator", t.faint(), None);
        sel("keyword.other.unit", t.yellow(), None);

        // Names: functions, types, tags, attributes.
        sel("entity.name.function", t.accent(), None);
        sel("entity.name.function.macro", t.cyan(), None);
        sel("entity.name.type", t.cyan(), None);
        sel("entity.name.class", t.cyan(), None);
        sel("entity.name.struct", t.cyan(), None);
        sel("entity.name.enum", t.cyan(), None);
        sel("entity.name.trait", t.cyan(), None);
        sel("entity.name.interface", t.cyan(), None);
        sel("entity.name.namespace", t.cyan(), None);
        sel("entity.name.module", t.cyan(), None);
        sel("entity.name.tag", t.red(), None);
        sel("entity.name.section", t.accent(), Some(BOLD));
        sel("entity.other.attribute-name", t.yellow(), None);
        sel("entity.other.inherited-class", t.cyan(), None);

        // Variables.
        sel("variable", t.fg, None);
        sel("variable.other.constant", t.yellow(), None);
        sel("variable.language", t.purple(), Some(ITALIC));

        // Library-provided names.
        sel("support.function", t.cyan(), None);
        sel("support.type", t.cyan(), None);
        sel("support.class", t.cyan(), None);
        sel("support.constant", t.yellow(), None);
        sel("support.variable", t.fg, None);

        // Preprocessor and macros.
        sel("meta.preprocessor", t.purple(), None);
        sel("meta.preprocessor.string", t.green(), None);
        sel("meta.preprocessor.numeric", t.yellow(), None);

        // Embedded markup (Markdown/HTML inside a code block).
        sel("markup.heading", t.accent(), Some(BOLD));
        sel("markup.bold", t.fg, Some(BOLD));
        sel("markup.italic", t.fg, Some(ITALIC));
        sel("markup.underline", t.accent(), Some(UNDERLINE));
        sel("markup.link", t.accent(), Some(UNDERLINE));
        sel("markup.raw", t.green(), None);
        sel("markup.inserted", t.green(), None);
        sel("markup.deleted", t.red(), None);
        sel("markup.changed", t.yellow(), None);

        // Diff headers and hunk markers.
        sel("meta.diff.header", t.accent(), Some(BOLD));
        sel("meta.diff.range", t.purple(), None);

        // Errors.
        sel("invalid", t.red(), None);
        sel("invalid.illegal", t.red(), Some(BOLD));

        SynTheme {
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

/// Load the embedded syntax set eagerly (a ~960 KB compressed dump). Call it
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

    fn piece_lines(lang: &str, text: &str, t: &Theme) -> Vec<Vec<Piece>> {
        lines(lang, text, t)
    }

    fn has_fg(lines: &[Vec<Piece>], fg: Rgb) -> bool {
        lines.iter().flatten().any(|p| p.fg == Some(fg))
    }

    fn joined(lines: &[Vec<Piece>]) -> String {
        lines
            .iter()
            .map(|l| l.iter().map(|p| p.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_when_unknown_language() {
        let t = Theme::dark_default();
        let ls = lines("definitely-not-a-language", "let x = 1;", &t);
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0][0].text, "let x = 1;");
        assert_eq!(ls[0][0].fg, None);
    }

    #[test]
    fn plain_aliases_skip_syntax() {
        let t = Theme::dark_default();
        for lang in ["text", "txt", "plaintext", "none", "output"] {
            let ls = lines(lang, "# include <stdio.h>\nint main(void);", &t);
            assert_eq!(ls[0][0].text, "# include <stdio.h>");
            assert_eq!(ls[0][0].fg, None, "lang {lang:?}");
            assert!(!ls[0][0].italic, "lang {lang:?}");
        }
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn rust_keywords_get_color() {
        let t = Theme::dark_default();
        let ls = lines("rust", "fn main() {}", &t);
        assert_eq!(joined(&ls), "fn main() {}");
        assert!(ls[0][0].fg.is_some(), "{:?}", ls[0][0]);
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn keyword_string_and_comment_differ() {
        let t = Theme::dark_default();
        let ls = lines("rust", "fn f() { let s = \"hi\"; } // note", &t);
        let colors: Vec<Rgb> = ls[0].iter().filter_map(|p| p.fg).collect();
        assert!(colors.contains(&t.purple()), "keyword: {colors:?}");
        assert!(colors.contains(&t.green()), "string: {colors:?}");
        assert!(colors.contains(&t.muted()), "comment: {colors:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn comment_state_carries_across_lines() {
        let t = Theme::dark_default();
        let ls = lines("js", "/* a\n b */\nlet x = 1;", &t);
        assert!(
            ls[1].iter().any(|p| p.fg == Some(t.muted())),
            "second line left the comment: {ls:?}"
        );
        assert!(
            ls[2].iter().any(|p| p.fg == Some(t.purple())),
            "keyword after the comment: {ls:?}"
        );
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn line_comment_does_not_leak_down() {
        let t = Theme::dark_default();
        let cases = [
            ("sh", "# note\npodman rm -f app"),
            ("bash", "# note\npodman rm -f app"),
            ("zsh", "# note\npodman rm -f app"),
            ("ruby", "# note\nputs :hi"),
            ("python", "# note\nx = 1"),
            ("yaml", "# note\nkey: 1"),
            ("perl", "# note\nmy $x = 1;"),
            ("toml", "# note\nkey = 1"),
            ("makefile", "# note\nall:"),
            ("dockerfile", "# note\nFROM debian"),
            ("r", "# note\nx <- 1"),
            ("nginx", "# note\nserver { }"),
            ("conf", "# note\nkey = 1"),
            ("ini", "; note\nkey = 1"),
            ("lua", "-- note\nlocal x = 1"),
            ("sql", "-- note\nselect 1;"),
            ("haskell", "-- note\nmain = pure ()"),
            ("elixir", "# note\nx = 1"),
        ];
        for (lang, src) in cases {
            let ls = piece_lines(lang, src, &t);
            assert_eq!(ls.len(), 2, "{lang}: {ls:?}");
            assert!(
                ls[0].iter().any(|p| p.fg == Some(t.muted())),
                "{lang}: no comment on line 1: {ls:?}"
            );
            assert!(
                ls[1].iter().all(|p| p.fg != Some(t.muted())),
                "{lang}: comment leaked onto line 2: {ls:?}"
            );
        }
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn bat_only_languages_are_highlighted() {
        let t = Theme::dark_default();
        let cases = [
            ("toml", "key = \"value\""),
            ("dockerfile", "FROM debian:stable"),
            ("typescript", "const x: number = 1;"),
            ("nix", "{ pkgs }: pkgs.hello"),
            ("protobuf", "message M { int32 a = 1; }"),
            ("ini", "key = value"),
            ("graphql", "query Q { hero { name } }"),
            ("kotlin", "fun main() = println(1)"),
            ("zig", "const x: u8 = 1;"),
            ("terraform", "resource \"x\" \"y\" {}"),
        ];
        for (lang, src) in cases {
            let ls = piece_lines(lang, src, &t);
            assert_eq!(joined(&ls), src, "{lang}: text changed");
            assert!(
                ls.iter().flatten().any(|p| p.fg.is_some() || p.bold || p.italic),
                "{lang}: fell back to plain text: {ls:?}"
            );
        }
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn fence_aliases_resolve() {
        let t = Theme::dark_default();
        for (alias, canonical) in [
            ("console", "sh"),
            ("shell-session", "sh"),
            ("terminal", "sh"),
            ("Shell", "sh"),
            ("Python", "python"),
            ("language-rs", "rust"),
            ("rust,ignore", "rust"),
            ("JS", "javascript"),
        ] {
            let src = "# note\nx";
            let a = piece_lines(alias, src, &t);
            let b = piece_lines(canonical, src, &t);
            assert_eq!(
                a.iter().flatten().map(|p| p.fg).collect::<Vec<_>>(),
                b.iter().flatten().map(|p| p.fg).collect::<Vec<_>>(),
                "alias {alias:?} did not resolve like {canonical:?}"
            );
        }
        // The shell family must treat `# note` as a comment.
        for alias in ["console", "shell-session", "terminal", "Shell", "sh"] {
            let a = piece_lines(alias, "# note\npodman ps", &t);
            assert!(has_fg(&a, t.muted()), "alias {alias:?}: {a:?}");
        }
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn shebang_highlights_unlabelled_block() {
        let t = Theme::dark_default();
        let ls = piece_lines("", "#!/usr/bin/env python3\n# note\ndef f(): pass", &t);
        assert!(has_fg(&ls, t.muted()), "{ls:?}");
        assert!(has_fg(&ls, t.purple()), "{ls:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn html_tags_and_attributes() {
        let t = Theme::dark_default();
        let ls = lines("html", "<a href=\"#\">hi</a>", &t);
        assert!(has_fg(&ls, t.red()), "tag: {ls:?}");
        assert!(has_fg(&ls, t.yellow()), "attribute: {ls:?}");
        assert!(has_fg(&ls, t.green()), "string: {ls:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn strings_are_green_and_unquoted_stays_neutral() {
        let t = Theme::dark_default();
        let ls = lines("yaml", "key: \"v\"\nother: plain", &t);
        assert!(has_fg(&ls, t.green()), "{ls:?}");
        // `plain` is an unquoted scalar: neutral, not green.
        let scalar = ls[1]
            .iter()
            .find(|p| p.text.contains("plain"))
            .expect("scalar piece");
        assert_ne!(scalar.fg, Some(t.green()), "{scalar:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn doc_comment_and_underline_do_not_leak() {
        let t = Theme::dark_default();
        let ls = lines("rust", "/// doc\nfn f() {}", &t);
        assert!(ls[0].iter().all(|p| !p.underline), "{ls:?}");
        assert!(ls[1].iter().any(|p| p.fg == Some(t.purple())), "{ls:?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn unmatched_text_inherits_theme_fg() {
        let t = Theme::dark_default();
        let ls = lines("rust", "let abc = 1;", &t);
        let ident = ls[0]
            .iter()
            .find(|p| p.text.contains("abc"))
            .expect("identifier piece");
        assert_eq!(ident.fg, None, "{ident:?}");
    }

    /// Every bundled syntax must survive a line of generic input: a broken
    /// definition would otherwise surface as a panic in a random document.
    #[cfg(feature = "syntaxes")]
    #[test]
    fn every_syntax_parses() {
        let t = Theme::dark_default();
        let mut bad = Vec::new();
        for s in syn::SET.syntaxes() {
            let sample =
                "# note \"quoted\" 42\n/* block */\nkey = value; fn f(a) { return a; }\n";
            match syn::highlight_syntax(s, sample, &t) {
                Some(v) => {
                    let text: String = v.iter().flatten().map(|p| p.text.as_str()).collect::<String>();
                    if text.replace('\n', "") != sample.replace('\n', "") {
                        bad.push(format!("{}: text mismatch", s.name));
                    }
                }
                None => bad.push(format!("{}: no highlight", s.name)),
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[cfg(feature = "syntaxes")]
    #[test]
    fn syntax_count_is_large() {
        assert!(syntax_count() > 150, "only {} syntaxes", syntax_count());
    }
}
