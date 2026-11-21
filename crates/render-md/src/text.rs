//! Lays out non-pixel blocks as styled terminal lines at a given width.

use crate::doc::{Alert, Align, Item, Node, Span, Style};
use crate::highlight;
use docmodel::{display_width, Attrs, Cell, Line, Theme};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub struct Ctx<'a> {
    pub theme: &'a Theme,
}

impl Ctx<'_> {
    fn span_attrs(&self, s: &Style) -> Attrs {
        let t = self.theme;
        Attrs {
            fg: if s.code {
                Some(t.palette[11])
            } else if s.link {
                Some(t.accent())
            } else {
                None
            },
            bg: s.code.then(|| t.code_bg()),
            bold: s.bold,
            italic: s.italic,
            underline: s.link,
            strike: s.strike,
        }
    }

    pub fn node(&self, node: &Node, width: usize) -> Vec<Line> {
        let width = width.max(4);
        let t = self.theme;
        match node {
            Node::Para(spans) => self.wrap(spans, width, Attrs::default()),
            Node::Image { url } => {
                let mut l = Line::default();
                l.cells.push(Cell {
                    text: format!("🖼 {url}"),
                    attrs: Attrs {
                        fg: Some(t.muted()),
                        italic: true,
                        ..Default::default()
                    },
                    url: None,
                });
                vec![l]
            }
            Node::Heading { level, spans } => {
                let attrs = match level {
                    1..=3 => Attrs {
                        fg: Some(t.accent()),
                        bold: true,
                        ..Default::default()
                    },
                    _ => Attrs {
                        bold: true,
                        ..Default::default()
                    },
                };
                let mut lines = Vec::new();
                if *level <= 2 {
                    // Pixel headings fall back to this when graphics are off.
                    let mark = if *level == 1 { "█ " } else { "▌ " };
                    let mut inner = self.wrap(spans, width - 2, attrs);
                    for (i, l) in inner.drain(..).enumerate() {
                        let mut p = Line::default();
                        p.cells.push(Cell {
                            text: if i == 0 { mark.into() } else { "  ".into() },
                            attrs: Attrs {
                                fg: Some(t.accent()),
                                ..Default::default()
                            },
                            url: None,
                        });
                        lines.push(Line::prefixed(&p.cells, l));
                    }
                } else {
                    lines = self.wrap(spans, width, attrs);
                }
                lines
            }
            Node::Code { lang, text } => self.code(lang, text, width),
            Node::Mermaid(text) => self.code("", text, width),
            Node::Html(html) => {
                let attrs = Attrs {
                    fg: Some(t.muted()),
                    ..Default::default()
                };
                html.trim_end()
                    .lines()
                    .flat_map(|l| hard_wrap(l, width, attrs))
                    .collect()
            }
            Node::Rule => {
                let mut l = Line::default();
                l.cells.push(Cell {
                    text: "─".repeat(width),
                    attrs: Attrs {
                        fg: Some(t.faint()),
                        ..Default::default()
                    },
                    url: None,
                });
                vec![l]
            }
            Node::Quote { alert, children } => self.quote(*alert, children, width),
            Node::List { start, items } => self.list(*start, items, width),
            Node::Table { aligns, head, rows } => self.table(aligns, head, rows, width),
        }
    }

    fn children(&self, nodes: &[Node], width: usize, tight: bool) -> Vec<Line> {
        let mut out = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            if i > 0 && !(tight && matches!(n, Node::List { .. })) {
                out.push(Line::default());
            }
            out.extend(self.node(n, width));
        }
        out
    }

    fn quote(&self, alert: Option<Alert>, children: &[Node], width: usize) -> Vec<Line> {
        let t = self.theme;
        let (color, title) = match alert {
            Some(Alert::Note) => (t.accent(), Some("Note")),
            Some(Alert::Tip) => (t.green(), Some("Tip")),
            Some(Alert::Important) => (t.purple(), Some("Important")),
            Some(Alert::Warning) => (t.yellow(), Some("Warning")),
            Some(Alert::Caution) => (t.red(), Some("Caution")),
            None => (docmodel::Rgb::mix(t.fg, t.bg, 0.55), None),
        };
        let bar = Cell {
            text: "▎ ".into(),
            attrs: Attrs {
                fg: Some(color),
                ..Default::default()
            },
            url: None,
        };
        let mut out = Vec::new();
        if let Some(title) = title {
            let mut l = Line::default();
            l.cells.push(Cell {
                text: title.into(),
                attrs: Attrs {
                    fg: Some(color),
                    bold: true,
                    ..Default::default()
                },
                url: None,
            });
            out.push(Line::prefixed(std::slice::from_ref(&bar), l));
        }
        for l in self.children(children, width - 2, false) {
            let mut l = l;
            if alert.is_none() {
                for c in &mut l.cells {
                    c.attrs.italic = true;
                    c.attrs.fg.get_or_insert(t.muted());
                }
            }
            out.push(Line::prefixed(std::slice::from_ref(&bar), l));
        }
        out
    }

    fn list(&self, start: Option<u64>, items: &[Item], width: usize) -> Vec<Line> {
        let t = self.theme;
        let tight = items.iter().all(|i| {
            i.children
                .iter()
                .filter(|c| !matches!(c, Node::List { .. }))
                .count()
                <= 1
        });
        let last = start.map(|s| s + items.len() as u64 - 1).unwrap_or(0);
        let marker_w = match start {
            Some(_) => last.to_string().len() + 2,
            None => 2,
        };
        let mut out = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if i > 0 && !tight {
                out.push(Line::default());
            }
            let marker = match (start, item.task) {
                (_, Some(true)) => "☑ ".to_string(),
                (_, Some(false)) => "☐ ".to_string(),
                (Some(s), None) => format!("{:>w$}. ", s + i as u64, w = marker_w - 2),
                (None, None) => "• ".to_string(),
            };
            let marker_w = marker_w.max(display_width(&marker));
            let attrs = Attrs {
                fg: Some(if item.task == Some(true) {
                    t.green()
                } else {
                    t.accent()
                }),
                ..Default::default()
            };
            let mut body = self.children(&item.children, width.saturating_sub(marker_w), tight);
            if item.task == Some(true) {
                for l in &mut body {
                    for c in &mut l.cells {
                        c.attrs.fg = Some(t.muted());
                    }
                }
            }
            if body.is_empty() {
                body.push(Line::default());
            }
            for (j, l) in body.into_iter().enumerate() {
                let mut p = Line::default();
                if j == 0 {
                    p.cells.push(Cell {
                        text: format!("{marker:<marker_w$}"),
                        attrs,
                        url: None,
                    });
                } else {
                    p.cells.push(Cell {
                        text: " ".repeat(marker_w),
                        attrs: Attrs::default(),
                        url: None,
                    });
                }
                out.push(Line::prefixed(&p.cells, l));
            }
        }
        out
    }

    fn code(&self, lang: &str, text: &str, width: usize) -> Vec<Line> {
        let bg = self.theme.code_bg();
        let base = Attrs {
            bg: Some(bg),
            ..Default::default()
        };
        let inner = width.saturating_sub(2).max(1);
        let mut out = Vec::new();
        // Tabs move the cursor without erasing, which leaves stale glyphs on
        // screen when a row is rewritten; expand them up front.
        let expanded = text.replace('\t', "    ");
        let lines = highlight::lines(lang, &expanded, self.theme);
        for pieces in lines {
            for mut l in wrap_pieces(&pieces, inner, base) {
                l.cells.insert(
                    0,
                    Cell {
                        text: " ".into(),
                        attrs: base,
                        url: None,
                    },
                );
                l.fill = Some(bg);
                out.push(l);
            }
        }
        out
    }

    fn table(
        &self,
        aligns: &[Align],
        head: &[Vec<Span>],
        rows: &[Vec<Vec<Span>>],
        width: usize,
    ) -> Vec<Line> {
        let t = self.theme;
        let ncols = head
            .len()
            .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return Vec::new();
        }
        let cell_w = |c: &Vec<Span>| c.iter().map(|s| display_width(&s.text)).sum::<usize>();
        let mut want: Vec<usize> = (0..ncols)
            .map(|i| {
                let h = head.get(i).map(cell_w).unwrap_or(0);
                rows.iter()
                    .filter_map(|r| r.get(i))
                    .map(cell_w)
                    .fold(h, usize::max)
                    .max(1)
            })
            .collect();
        let sep = 3;
        let avail = width.saturating_sub(sep * (ncols - 1)).max(ncols);
        // Shrink the widest columns first until the table fits.
        while want.iter().sum::<usize>() > avail {
            let (i, _) = want.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            if want[i] <= 3 {
                break;
            }
            want[i] -= 1;
        }
        let border = Attrs {
            fg: Some(t.faint()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let emit_row = |cells: &[Vec<Span>], base: Attrs, out: &mut Vec<Line>| {
            let wrapped: Vec<Vec<Line>> = (0..ncols)
                .map(|i| {
                    cells
                        .get(i)
                        .map(|c| self.wrap(c, want[i], base))
                        .unwrap_or_default()
                })
                .collect();
            let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(0).max(1);
            for r in 0..height {
                let mut line = Line::default();
                for i in 0..ncols {
                    if i > 0 {
                        line.cells.push(Cell {
                            text: " │ ".into(),
                            attrs: border,
                            url: None,
                        });
                    }
                    let content = wrapped[i].get(r).cloned().unwrap_or_default();
                    let pad = want[i].saturating_sub(content.width());
                    let (l, rgt) = match aligns.get(i).copied().unwrap_or(Align::Left) {
                        Align::Left => (0, pad),
                        Align::Right => (pad, 0),
                        Align::Center => (pad / 2, pad - pad / 2),
                    };
                    if l > 0 {
                        line.cells.push(Cell {
                            text: " ".repeat(l),
                            attrs: Attrs::default(),
                            url: None,
                        });
                    }
                    line.cells.extend(content.cells);
                    if rgt > 0 {
                        line.cells.push(Cell {
                            text: " ".repeat(rgt),
                            attrs: Attrs::default(),
                            url: None,
                        });
                    }
                }
                out.push(line);
            }
        };
        emit_row(
            head,
            Attrs {
                bold: true,
                ..Default::default()
            },
            &mut out,
        );
        let mut rule = Line::default();
        let parts: Vec<String> = want.iter().map(|w| "─".repeat(*w)).collect();
        rule.cells.push(Cell {
            text: parts.join("─┼─"),
            attrs: border,
            url: None,
        });
        out.push(rule);
        for row in rows {
            emit_row(row, Attrs::default(), &mut out);
        }
        // Close the table with a rule so it does not look like loose text.
        let mut bottom = Line::default();
        let parts: Vec<String> = want.iter().map(|w| "─".repeat(*w)).collect();
        bottom.cells.push(Cell {
            text: parts.join("─┴─"),
            attrs: border,
            url: None,
        });
        out.push(bottom);
        out
    }

    /// Greedy word wrap that keeps span styles.
    pub fn wrap(&self, spans: &[Span], width: usize, base: Attrs) -> Vec<Line> {
        let mut lines = vec![Line::default()];
        let mut col = 0usize;
        // Whitespace waits here until the next word decides whether it fits.
        let mut pending_space: Option<Cell> = None;
        for span in spans {
            let mut attrs = self.span_attrs(&span.style);
            attrs.fg = attrs.fg.or(base.fg);
            attrs.bold |= base.bold;
            attrs.italic |= base.italic;
            for piece in split_words(&span.text) {
                match piece {
                    Piece::Newline => {
                        lines.push(Line::default());
                        col = 0;
                        pending_space = None;
                    }
                    Piece::Space => {
                        if col > 0 {
                            pending_space = Some(Cell {
                                text: " ".into(),
                                attrs: if span.style.code {
                                    attrs
                                } else {
                                    Attrs {
                                        underline: false,
                                        strike: false,
                                        ..attrs
                                    }
                                },
                                url: span.url.clone(),
                            });
                        }
                    }
                    Piece::Word(w) => {
                        let ww = w.width();
                        let space = pending_space.as_ref().map_or(0, |_| 1);
                        if col > 0 && col + space + ww > width {
                            lines.push(Line::default());
                            col = 0;
                            pending_space = None;
                        }
                        let line = lines.last_mut().unwrap();
                        if let Some(sp) = pending_space.take() {
                            line.cells.push(sp);
                            col += 1;
                        }
                        if ww > width {
                            // A word longer than the line: break it by cells.
                            // CJK has no spaces, so this is the normal path for
                            // Chinese text; keep closing punctuation off the
                            // start of the next line (禁则处理).
                            let mut chunk = String::new();
                            for ch in w.chars() {
                                let cw = ch.width().unwrap_or(0);
                                // Hanging punctuation (禁则): never start a
                                // line with a closing mark; let it overflow by
                                // at most one column instead.
                                if col + cw > width && !is_closing_punct(ch) {
                                    let line = lines.last_mut().unwrap();
                                    line.cells.push(Cell {
                                        text: std::mem::take(&mut chunk),
                                        attrs,
                                        url: span.url.clone(),
                                    });
                                    lines.push(Line::default());
                                    col = 0;
                                }
                                chunk.push(ch);
                                col += cw;
                            }
                            lines.last_mut().unwrap().cells.push(Cell {
                                text: chunk,
                                attrs,
                                url: span.url.clone(),
                            });
                        } else {
                            let line = lines.last_mut().unwrap();
                            match line.cells.last_mut() {
                                Some(last) if last.attrs == attrs && last.url == span.url => {
                                    last.text.push_str(w)
                                }
                                _ => line.cells.push(Cell {
                                    text: w.to_string(),
                                    attrs,
                                    url: span.url.clone(),
                                }),
                            }
                            col += ww;
                        }
                    }
                }
            }
        }
        lines
    }
}

/// Closing punctuation that must not begin a line in Chinese typography.
fn is_closing_punct(ch: char) -> bool {
    matches!(
        ch,
        '。' | '，'
            | '、'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '】'
            | '》'
            | '」'
            | '』'
            | '”'
            | '’'
            | '…'
            | '·'
            | '～'
    ) || matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

enum Piece<'a> {
    Word(&'a str),
    Space,
    Newline,
}

fn split_words(s: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c == ' ' || c == '\n' || c == '\t' {
            if let Some(st) = start.take() {
                out.push(Piece::Word(&s[st..i]));
            }
            out.push(if c == '\n' {
                Piece::Newline
            } else {
                Piece::Space
            });
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push(Piece::Word(&s[st..]));
    }
    out
}

fn hard_wrap(s: &str, width: usize, attrs: Attrs) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut col = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if col + cw > width && !cur.is_empty() {
            lines.push(Line {
                cells: vec![Cell {
                    text: std::mem::take(&mut cur),
                    attrs,
                    url: None,
                }],
                fill: None,
            });
            col = 0;
        }
        // Never emit a raw tab: it advances without erasing.
        if ch == '\t' {
            let n = 4 - col % 4;
            cur.push_str(&" ".repeat(n));
            col += n;
            continue;
        }
        cur.push(ch);
        col += cw;
    }
    lines.push(Line {
        cells: vec![Cell {
            text: cur,
            attrs,
            url: None,
        }],
        fill: None,
    });
    lines
}

/// Hard-wrap already-colored code pieces at `width` columns.
fn wrap_pieces(pieces: &[highlight::Piece], width: usize, base: Attrs) -> Vec<Line> {
    let mut out = Vec::new();
    let mut line = Line::default();
    let mut cur = String::new();
    let mut col = 0usize;
    let push = |line: &mut Line, cur: &mut String, attrs: Attrs| {
        if !cur.is_empty() {
            line.cells.push(Cell {
                text: std::mem::take(cur),
                attrs,
                url: None,
            });
        }
    };
    for p in pieces {
        let mut attrs = base;
        attrs.fg = p.fg.or(base.fg);
        attrs.bold |= p.bold;
        attrs.italic |= p.italic;
        for ch in p.text.chars() {
            let cw = ch.width().unwrap_or(0);
            if col + cw > width && !cur.is_empty() {
                push(&mut line, &mut cur, attrs);
                out.push(std::mem::take(&mut line));
                col = 0;
            }
            cur.push(ch);
            col += cw;
        }
    }
    push(&mut line, &mut cur, base);
    out.push(line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    fn render(md: &str, width: usize) -> Vec<String> {
        let theme = Theme::dark_default();
        let ctx = Ctx { theme: &theme };
        let d = doc::parse(md);
        let mut out = Vec::new();
        for (i, b) in d.blocks.iter().enumerate() {
            if i > 0 {
                out.push(String::new());
            }
            out.extend(
                ctx.node(&b.node, width)
                    .iter()
                    .map(|l| plain(l).trim_end().to_string()),
            );
        }
        out
    }

    fn plain(l: &Line) -> String {
        l.cells.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn wraps_paragraphs() {
        let out = render("the quick brown fox jumps over the lazy dog", 15);
        assert_eq!(
            out,
            vec!["the quick brown", "fox jumps over", "the lazy dog"]
        );
        assert!(out.iter().all(|l| l.width() <= 15));
    }

    #[test]
    fn lists_nest_and_number() {
        let out = render("1. one\n2. two\n   - inner item\n3. three\n", 40);
        assert_eq!(out, vec!["1. one", "2. two", "   • inner item", "3. three"]);
    }

    #[test]
    fn tasks_and_quotes() {
        let out = render("- [x] shipped\n- [ ] next\n\n> quoted\n", 40);
        assert_eq!(out, vec!["☑ shipped", "☐ next", "", "▎ quoted"]);
    }

    #[test]
    fn table_columns_align_with_cjk_and_emoji() {
        let theme = Theme::dark_default();
        let ctx = Ctx { theme: &theme };
        let md = "| 格式 | 引擎 | 可搜索 |\n|---|---|---|\n| Markdown | pulldown-cmark | ✅ |\n| PDF | mupdf | ⏳ |\n| 图片 | image + resvg | ❌ |\n";
        let d = doc::parse(md);
        let lines = ctx.node(&d.blocks[0].node, 60);
        // Every row must occupy the same number of columns, otherwise the
        // terminal draws the right edge ragged.
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "ragged table: {widths:?} in {lines:#?}"
        );
        // Column starts must line up, which is the real complaint.
        let pipe_col = |l: &Line| -> Option<usize> {
            let mut col = 0;
            for c in &l.cells {
                for ch in c.text.chars() {
                    if ch == '│' {
                        return Some(col);
                    }
                    col += docmodel::char_cols(ch);
                }
            }
            None
        };
        let starts: Vec<usize> = lines.iter().filter_map(pipe_col).collect();
        assert!(starts.len() >= 4, "expected data rows: {lines:#?}");
        assert!(
            starts.windows(2).all(|w| w[0] == w[1]),
            "column drift: {starts:?}"
        );
    }

    #[test]
    fn tables_fit_width() {
        let out = render(
            "| name | value |\n|---|--:|\n| alpha | 1 |\n| beta | 22 |\n",
            40,
        );
        assert_eq!(out[0], "name  │ value");
        assert_eq!(out[1], "──────┼──────");
        assert_eq!(out[2], "alpha │     1");
        let narrow = render(
            "| a long header | another long header |\n|---|---|\n| x | y |\n",
            20,
        );
        assert!(narrow.iter().all(|l| l.width() <= 20), "{narrow:?}");
    }

    #[test]
    fn code_blocks_hard_wrap() {
        let out = render("```\nabcdefghij\n```\n", 8);
        assert_eq!(out, vec![" abcdef", " ghij"]);
    }

    #[test]
    fn code_tabs_become_spaces() {
        // A raw tab would advance the cursor without erasing old glyphs.
        let out = render("```\n\tfmt.Println()\n```\n", 40);
        assert!(!out[0].contains('\t'), "{out:?}");
        assert!(out[0].starts_with("     fmt"), "{out:?}");
    }

    #[test]
    fn closing_punctuation_never_starts_a_line() {
        let out = render("这是一句需要折行的中文，后面接标点符号。", 12);
        for line in &out {
            assert!(
                !line.starts_with(['。', '，', '、', '）']),
                "line starts with punctuation: {line:?} in {out:?}"
            );
        }
    }

    #[test]
    fn cjk_paragraph_wraps_by_cell_width() {
        let out = render("终端里的中文段落排版需要按显示宽度折行而不是字符数", 20);
        assert!(out.iter().all(|l| l.width() <= 20), "{out:?}");
        assert!(out.len() > 1);
    }
}
