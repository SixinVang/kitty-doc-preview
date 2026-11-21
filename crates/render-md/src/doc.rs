//! Markdown to a list of top-level blocks, each with a content hash.
//!
//! The hash of a block's source text is what everything downstream keys on:
//! keeping the scroll position across edits, marking changed blocks, and
//! skipping work for blocks that did not change.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use std::ops::Range;
use xxhash_rust::xxh3::xxh3_64;
use yaml_rust2::{Yaml, YamlLoader};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub url: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Node {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Para(Vec<Span>),
    /// A paragraph that is just an image; rendered as pixels when possible.
    Image {
        url: String,
    },
    Code {
        lang: String,
        text: String,
    },
    Mermaid(String),
    Quote {
        alert: Option<Alert>,
        children: Vec<Node>,
    },
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    /// A YAML frontmatter block (`---` delimited, at the top of the file),
    /// flattened to displayable key/value pairs.
    Frontmatter(Vec<Meta>),
    Rule,
    Table {
        aligns: Vec<Align>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Html(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alert {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

/// One frontmatter field. Nested mappings are flattened into dotted keys and
/// sequences are joined into a single value; the raw text is kept only when
/// the YAML does not parse.
#[derive(Clone, PartialEq, Debug)]
pub struct Meta {
    pub key: String,
    pub value: String,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Item {
    pub task: Option<bool>,
    pub children: Vec<Node>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Block {
    pub hash: u64,
    /// Byte range of the block's source in the original document. Used to map
    /// between editor lines and layout rows; empty when pulldown-cmark did not
    /// report one.
    pub src: Range<usize>,
    pub node: Node,
}

pub struct Doc {
    pub blocks: Vec<Block>,
}

pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

enum Container {
    Root,
    Quote(Option<Alert>),
    List(Option<u64>),
    Item(Option<bool>),
}

enum InlineTarget {
    Para,
    Heading(u8),
    Cell,
}

struct Table {
    aligns: Vec<Align>,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    in_head: bool,
}

struct Builder<'a> {
    src: &'a str,
    stack: Vec<(Container, Vec<Node>)>,
    items: Vec<Vec<Item>>,
    inline: Option<(InlineTarget, Vec<Span>)>,
    bold: u8,
    italic: u8,
    strike: u8,
    links: Vec<String>,
    images: Vec<String>,
    para_image: Option<String>,
    para_chars: usize,
    image_chars: usize,
    code: Option<(String, String)>,
    html: Option<String>,
    frontmatter: Option<String>,
    table: Option<Table>,
    top_range: Option<Range<usize>>,
    blocks: Vec<Block>,
}

pub fn parse(src: &str) -> Doc {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_GFM;
    let mut b = Builder {
        src,
        stack: vec![(Container::Root, Vec::new())],
        items: Vec::new(),
        inline: None,
        bold: 0,
        italic: 0,
        strike: 0,
        links: Vec::new(),
        images: Vec::new(),
        para_image: None,
        para_chars: 0,
        image_chars: 0,
        code: None,
        html: None,
        frontmatter: None,
        table: None,
        top_range: None,
        blocks: Vec::new(),
    };
    for (ev, range) in Parser::new_ext(src, opts).into_offset_iter() {
        b.event(ev, range);
    }
    b.close_implicit_para();
    Doc { blocks: b.blocks }
}

impl<'a> Builder<'a> {
    fn at_top(&self) -> bool {
        self.stack.len() == 1
            && self.inline.is_none()
            && self.code.is_none()
            && self.table.is_none()
    }

    fn push_node(&mut self, node: Node, range: Option<Range<usize>>) {
        if self.stack.len() == 1 {
            let range = self.top_range.take().or(range).unwrap_or(0..0);
            let hash = xxh3_64(self.src[range.clone()].as_bytes());
            self.blocks.push(Block {
                hash,
                src: range,
                node,
            });
        } else {
            self.stack.last_mut().unwrap().1.push(node);
        }
    }

    fn close_implicit_para(&mut self) {
        if matches!(self.inline, Some((InlineTarget::Para, _))) && self.table.is_none() {
            // Only implicit (tight list) paragraphs are still open here.
            let (_, spans) = self.inline.take().unwrap();
            if !spans.is_empty() {
                self.push_node(Node::Para(spans), None);
            }
        }
    }

    fn text(&mut self, text: &str, extra: Style) {
        if let Some(frontmatter) = &mut self.frontmatter {
            frontmatter.push_str(text);
            return;
        }
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
            return;
        }
        if let Some(html) = &mut self.html {
            html.push_str(text);
            return;
        }
        if self.inline.is_none() {
            self.inline = Some((InlineTarget::Para, Vec::new()));
        }
        let style = Style {
            bold: self.bold > 0 || extra.bold,
            italic: self.italic > 0 || extra.italic,
            strike: self.strike > 0,
            code: extra.code,
            link: !self.links.is_empty() || extra.link,
        };
        let url = self.links.last().cloned();
        let is_para = matches!(
            self.inline.as_ref().map(|(t, _)| t),
            Some(InlineTarget::Para)
        );
        if is_para {
            let n = text.chars().count();
            if self.images.is_empty() {
                self.para_chars += n;
            } else {
                self.image_chars += n;
                self.para_image = self.images.last().cloned();
            }
        }
        let spans = &mut self.inline.as_mut().unwrap().1;
        match spans.last_mut() {
            Some(last) if last.style == style && last.url == url => last.text.push_str(text),
            _ => spans.push(Span {
                text: text.to_string(),
                style,
                url,
            }),
        }
    }

    fn event(&mut self, ev: Event<'a>, range: Range<usize>) {
        match ev {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t, Style::default()),
            Event::Code(t) => self.text(
                &t,
                Style {
                    code: true,
                    ..Default::default()
                },
            ),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(
                &t,
                Style {
                    code: true,
                    ..Default::default()
                },
            ),
            Event::Html(t) | Event::InlineHtml(t) => {
                if self.html.is_some() {
                    self.text(&t, Style::default());
                }
            }
            Event::SoftBreak => self.text(" ", Style::default()),
            Event::HardBreak => self.text("\n", Style::default()),
            Event::Rule => {
                self.close_implicit_para();
                self.push_node(Node::Rule, Some(range));
            }
            Event::TaskListMarker(done) => {
                if let Some((Container::Item(task), _)) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            Event::FootnoteReference(t) => self.text(&format!("[{t}]"), Style::default()),
        }
    }

    fn start(&mut self, tag: Tag<'a>, range: Range<usize>) {
        let inline_tag = matches!(
            tag,
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. }
        );
        if !inline_tag && !matches!(tag, Tag::TableHead | Tag::TableRow | Tag::TableCell) {
            self.close_implicit_para();
            if self.at_top() {
                self.top_range = Some(range);
            }
        }
        match tag {
            Tag::Paragraph => self.inline = Some((InlineTarget::Para, Vec::new())),
            Tag::Heading { level, .. } => {
                self.inline = Some((InlineTarget::Heading(heading_level(level)), Vec::new()))
            }
            Tag::BlockQuote(kind) => self
                .stack
                .push((Container::Quote(kind.map(alert)), Vec::new())),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => self.html = Some(String::new()),
            Tag::MetadataBlock(_) => self.frontmatter = Some(String::new()),
            Tag::List(start) => {
                self.stack.push((Container::List(start), Vec::new()));
                self.items.push(Vec::new());
            }
            Tag::Item => self.stack.push((Container::Item(None), Vec::new())),
            Tag::Table(aligns) => {
                self.table = Some(Table {
                    aligns: aligns.into_iter().map(align).collect(),
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    in_head: false,
                })
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => self.inline = Some((InlineTarget::Cell, Vec::new())),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
            Tag::Image { dest_url, .. } => {
                self.images.push(dest_url.to_string());
                self.text(
                    "[image: ",
                    Style {
                        italic: true,
                        ..Default::default()
                    },
                );
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if let Some((_, spans)) = self.inline.take() {
                    let node =
                        if self.para_chars == 0 && self.para_image.is_some() && !spans.is_empty() {
                            Node::Image {
                                url: self.para_image.take().unwrap(),
                            }
                        } else {
                            Node::Para(spans)
                        };
                    self.push_node(node, None);
                }
                self.para_image = None;
                self.para_chars = 0;
                self.image_chars = 0;
            }
            TagEnd::Heading(_) => {
                if let Some((InlineTarget::Heading(level), spans)) = self.inline.take() {
                    self.push_node(Node::Heading { level, spans }, None);
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    let node = if lang.eq_ignore_ascii_case("mermaid") {
                        Node::Mermaid(text)
                    } else {
                        Node::Code { lang, text }
                    };
                    self.push_node(node, None);
                }
            }
            TagEnd::HtmlBlock => {
                if let Some(html) = self.html.take() {
                    self.push_node(Node::Html(html), None);
                }
            }
            TagEnd::MetadataBlock(_) => {
                if let Some(raw) = self.frontmatter.take() {
                    // Malformed YAML still shows up, highlighted, instead of
                    // silently disappearing or leaking into the next block.
                    let node = match meta_from_yaml(&raw) {
                        Some(entries) => Node::Frontmatter(entries),
                        None => Node::Code {
                            lang: "yaml".into(),
                            text: raw,
                        },
                    };
                    self.push_node(node, None);
                }
            }
            TagEnd::BlockQuote(_) => {
                self.close_implicit_para();
                if let Some((Container::Quote(alert), children)) = self.stack.pop() {
                    self.push_node(Node::Quote { alert, children }, None);
                }
            }
            TagEnd::Item => {
                self.close_implicit_para();
                if let Some((Container::Item(task), children)) = self.stack.pop() {
                    if let Some(items) = self.items.last_mut() {
                        items.push(Item { task, children });
                    }
                }
            }
            TagEnd::List(_) => {
                self.close_implicit_para();
                if let Some((Container::List(start), _)) = self.stack.pop() {
                    let items = self.items.pop().unwrap_or_default();
                    self.push_node(Node::List { start, items }, None);
                }
            }
            TagEnd::TableCell => {
                if let (Some((_, spans)), Some(t)) = (self.inline.take(), &mut self.table) {
                    t.row.push(spans);
                } else if let Some(t) = &mut self.table {
                    t.row.push(Vec::new());
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    if t.in_head {
                        t.head = row;
                    } else {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_node(
                        Node::Table {
                            aligns: t.aligns,
                            head: t.head,
                            rows: t.rows,
                        },
                        None,
                    );
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => {
                self.text(
                    "]",
                    Style {
                        italic: true,
                        ..Default::default()
                    },
                );
                self.images.pop();
            }
            _ => {}
        }
    }
}

/// Flatten a YAML frontmatter document into displayable key/value pairs.
/// Returns `None` when the text is not YAML (the caller falls back to a
/// highlighted code block) or is not a mapping at the top level.
fn meta_from_yaml(raw: &str) -> Option<Vec<Meta>> {
    let mut docs = YamlLoader::load_from_str(raw).ok()?;
    // A comments-only block yields no documents at all.
    if docs.is_empty() {
        return Some(Vec::new());
    }
    if docs.len() != 1 {
        return None;
    }
    let mut out = Vec::new();
    match docs.pop()? {
        Yaml::Hash(hash) => {
            for (k, v) in &hash {
                flatten_meta(k, v, "", &mut out)?;
            }
        }
        // An empty frontmatter block is still a frontmatter block.
        Yaml::Null => {}
        _ => return None,
    }
    Some(out)
}

fn flatten_meta(key: &Yaml, value: &Yaml, prefix: &str, out: &mut Vec<Meta>) -> Option<()> {
    let key = match key {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Null => "null".to_string(),
        _ => return None,
    };
    let key = if prefix.is_empty() {
        key
    } else {
        format!("{prefix}.{key}")
    };
    match value {
        // Nested maps read better as dotted keys than as inline braces.
        Yaml::Hash(hash) if !hash.is_empty() && hash.keys().all(scalar_key) => {
            for (k, v) in hash {
                flatten_meta(k, v, &key, out)?;
            }
        }
        _ => out.push(Meta {
            key,
            value: value_text(value),
        }),
    }
    Some(())
}

fn scalar_key(k: &Yaml) -> bool {
    matches!(
        k,
        Yaml::String(_) | Yaml::Real(_) | Yaml::Integer(_) | Yaml::Boolean(_) | Yaml::Null
    )
}

/// One value as a single display string. Block scalars keep their newlines so
/// the layout can wrap them with a hanging indent.
fn value_text(y: &Yaml) -> String {
    match y {
        // Block scalars keep a trailing newline that means nothing on screen.
        Yaml::String(s) | Yaml::Real(s) => s.trim_end().to_string(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Null => String::new(),
        Yaml::Array(items) => items
            .iter()
            .map(value_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Yaml::Hash(hash) => hash
            .iter()
            .map(|(k, v)| format!("{}: {}", value_text(k), value_text(v)))
            .collect::<Vec<_>>()
            .join(", "),
        // A resolved alias would need the anchor map; the parser only gives id.
        Yaml::Alias(id) => format!("*{id}"),
        Yaml::BadValue => String::new(),
    }
}

fn heading_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn align(a: Alignment) -> Align {
    match a {
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
        _ => Align::Left,
    }
}

fn alert(k: BlockQuoteKind) -> Alert {
    match k {
        BlockQuoteKind::Note => Alert::Note,
        BlockQuoteKind::Tip => Alert::Tip,
        BlockQuoteKind::Important => Alert::Important,
        BlockQuoteKind::Warning => Alert::Warning,
        BlockQuoteKind::Caution => Alert::Caution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_top_level_blocks() {
        let d =
            parse("# Title\n\nSome *text*.\n\n```mermaid\nflowchart LR\nA-->B\n```\n\n- a\n- b\n");
        assert_eq!(d.blocks.len(), 4);
        assert!(matches!(d.blocks[0].node, Node::Heading { level: 1, .. }));
        assert!(matches!(&d.blocks[2].node, Node::Mermaid(s) if s.contains("A-->B")));
        match &d.blocks[3].node {
            Node::List { items, .. } => {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[0].children[0], Node::Para(s) if plain(s) == "a"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hashes_are_stable_and_local() {
        let a = parse("# T\n\nfirst\n\nsecond\n");
        let b = parse("# T\n\nfirst, edited\n\nsecond\n");
        assert_eq!(a.blocks[0].hash, b.blocks[0].hash);
        assert_ne!(a.blocks[1].hash, b.blocks[1].hash);
        assert_eq!(a.blocks[2].hash, b.blocks[2].hash);
    }

    #[test]
    fn tables_and_tasks() {
        let d = parse("| a | b |\n|---|--:|\n| 1 | 2 |\n\n- [x] done\n- [ ] todo\n");
        match &d.blocks[0].node {
            Node::Table { aligns, head, rows } => {
                assert_eq!(aligns, &[Align::Left, Align::Right]);
                assert_eq!(plain(&head[1]), "b");
                assert_eq!(plain(&rows[0][0]), "1");
            }
            other => panic!("{other:?}"),
        }
        match &d.blocks[1].node {
            Node::List { items, .. } => {
                assert_eq!(items[0].task, Some(true));
                assert_eq!(items[1].task, Some(false));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_lists_and_quotes() {
        let d = parse("> [!NOTE]\n> hi\n\n1. one\n   - inner\n2. two\n");
        assert!(matches!(
            &d.blocks[0].node,
            Node::Quote {
                alert: Some(Alert::Note),
                ..
            }
        ));
        match &d.blocks[1].node {
            Node::List {
                start: Some(1),
                items,
            } => {
                assert!(matches!(
                    &items[0].children[1],
                    Node::List { start: None, .. }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn block_and_inline_images() {
        let d = parse("![](shot.png)\n");
        assert!(matches!(&d.blocks[0].node, Node::Image { url } if url == "shot.png"));
        let d = parse("see ![](shot.png) here\n");
        assert!(matches!(&d.blocks[0].node, Node::Para(_)));
    }

    #[test]
    fn blocks_carry_source_ranges() {
        let src = "# Title\n\nSome *text*.\n\n```rust\nfn main() {}\n```\n\n- a\n- b\n\n> quote\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let d = parse(src);
        assert_eq!(d.blocks.len(), 6);
        for b in &d.blocks {
            assert!(b.src.start < b.src.end, "empty range for {:?}", b.node);
            assert!(b.src.end <= src.len());
            assert_eq!(xxh3_64(src[b.src.clone()].as_bytes()), b.hash);
        }
        for pair in d.blocks.windows(2) {
            assert!(
                pair[0].src.end <= pair[1].src.start,
                "overlapping ranges {:?} and {:?}",
                pair[0].src,
                pair[1].src
            );
        }
        assert_eq!(src[d.blocks[0].src.clone()].trim_end(), "# Title");
    }

    #[test]
    fn source_ranges_cover_nested_blocks() {
        let src = "> - a\n> - b\n\n1. one\n2. two\n";
        let d = parse(src);
        assert_eq!(d.blocks.len(), 2);
        assert!(src[d.blocks[0].src.clone()].contains("- a"));
        assert!(src[d.blocks[1].src.clone()].contains("2. two"));
        assert!(d.blocks[0].src.end <= d.blocks[1].src.start);
    }
}

#[cfg(test)]
mod frontmatter_tests {
    use super::*;

    const SRC: &str = "---\ntitle: Hello\ntags: [rust, cli]\nauthor:\n  name: Ada\ndraft: false\nsummary: |\n  line one\n  line two\n---\n\n# Heading\n";

    fn entries(d: &Doc) -> &[Meta] {
        match &d.blocks[0].node {
            Node::Frontmatter(m) => m,
            other => panic!("expected frontmatter, got {other:?}"),
        }
    }

    #[test]
    fn yaml_frontmatter_becomes_a_metadata_block() {
        let d = parse(SRC);
        assert_eq!(d.blocks.len(), 2);
        let got: Vec<(&str, &str)> = entries(&d)
            .iter()
            .map(|m| (m.key.as_str(), m.value.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("title", "Hello"),
                ("tags", "rust, cli"),
                ("author.name", "Ada"),
                ("draft", "false"),
                ("summary", "line one\nline two"),
            ]
        );
        assert!(matches!(d.blocks[1].node, Node::Heading { .. }));
    }

    #[test]
    fn frontmatter_owns_its_source_range() {
        let d = parse(SRC);
        let src = &SRC[d.blocks[0].src.clone()];
        assert!(src.starts_with("---\ntitle"), "{src:?}");
        assert!(src.trim_end().ends_with("---"), "{src:?}");
        assert_eq!(xxh3_64(src.as_bytes()), d.blocks[0].hash);
        assert!(d.blocks[0].src.end <= d.blocks[1].src.start);
    }

    #[test]
    fn comments_only_frontmatter_is_an_empty_card() {
        let d = parse("---\n# nothing but a comment\n---\n\nbody\n");
        assert_eq!(entries(&d).len(), 0);
        assert!(matches!(d.blocks[1].node, Node::Para(_)));
    }

    #[test]
    fn a_lone_delimiter_pair_is_not_frontmatter() {
        // pulldown-cmark only treats a non-empty block as metadata; without
        // this the empty block would silently swallow the next paragraph.
        let d = parse("---\n---\n\nbody\n");
        assert!(matches!(d.blocks[0].node, Node::Rule));
        assert!(matches!(d.blocks[1].node, Node::Rule));
        assert!(matches!(d.blocks[2].node, Node::Para(_)));
    }

    #[test]
    fn invalid_yaml_falls_back_to_a_code_block() {
        let d = parse("---\ntitle: [unclosed\n---\n\nbody\n");
        assert!(matches!(&d.blocks[0].node, Node::Code { lang, .. } if lang == "yaml"));
    }

    #[test]
    fn a_rule_later_in_the_document_stays_a_rule() {
        let d = parse("intro\n\n---\n\nafter\n");
        assert!(d.blocks.iter().any(|b| matches!(b.node, Node::Rule)));
    }
}
