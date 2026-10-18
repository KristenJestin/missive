//! The reduced form: the message cut down to simple content, drawn in a system-like theme.
//!
//! What is kept: paragraphs, headings, lists, quotes, preformatted text, emphasis, links, the
//! message's own images (`cid:`, `data:`), and tables only when they hold data. Everything else
//! (the author's CSS, layout tables, remote images, forms, scripts) is dropped; a dropped remote
//! image leaves its alternative text. What the author's CSS hides (a newsletter's preheader, most
//! often) is not content: Blitz computes the styles, without fetching anything, to find it.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use blitz_dom::{BaseDocument, DocumentConfig, ElementData, Node, NodeData};
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};

use crate::flatten::flatten;

#[derive(Clone, Copy)]
pub enum Theme {
    Light,
    Dark,
}

/// The largest body reduced, in bytes.
pub const MAX_HTML: usize = 2 << 20;

/// How deep the elements of the reduced document nest at most: a text view may parse and lay
/// them out recursively (gpui-base's does). Deeper content goes into the nearest kept element.
pub const MAX_DEPTH: usize = 32;

/// The stack the reduction runs on. On it, Blitz styled and laid out about 400 nested elements
/// in a debug build, and 2 000 in a release build, before overflowing (measured).
const STACK: usize = 8 << 20;

/// How many open elements the body may nest in before Blitz parses it: deeper tags are removed,
/// their text kept. Blitz's parser also gets slow with depth (5 000 nested `div`s take seconds).
const FLATTEN_AT: usize = 96;

/// How deep Blitz's tree may be: the count before parsing is an estimate (a table gains a
/// `tbody`, SVG nests what HTML closes). Deeper, the reduction is refused rather than overflow.
const MAX_TREE: usize = 160;

/// Why a body has no reduced form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Larger than [`MAX_HTML`].
    TooLarge,
    /// Nested deeper than the reduction can follow safely.
    TooDeep,
    /// Not done within its time budget.
    TooLong,
    /// The reduction failed (it panicked).
    Failed,
}

/// Colours close to the GNOME (Adwaita) defaults: a stand-in for the system's.
fn stylesheet(theme: &Theme) -> String {
    let (scheme, background, text, link, muted, rule) = match theme {
        Theme::Light => (
            "light", "#ffffff", "#1e1e1e", "#1c71d8", "#5e5c64", "#deddda",
        ),
        Theme::Dark => (
            "dark", "#242424", "#ffffff", "#78aeed", "#c0bfbc", "#5e5c64",
        ),
    };
    format!(
        ":root {{ color-scheme: {scheme}; }}
html {{ background: {background}; color: {text}; }}
body {{ margin: 0; padding: 24px 32px; font-family: sans-serif; font-size: 15px; line-height: 1.5; }}
a {{ color: {link}; }}
h1, h2, h3, h4, h5, h6 {{ line-height: 1.25; margin: 16px 0 8px; }}
blockquote {{ margin: 8px 0; padding-left: 12px; border-left: 3px solid {rule}; color: {muted}; }}
img {{ max-width: 100%; height: auto; }}
table {{ border-collapse: collapse; margin: 8px 0; }}
td, th {{ border: 1px solid {rule}; padding: 4px 8px; text-align: left; vertical-align: top; }}
pre {{ white-space: pre-wrap; font-family: monospace; }}
hr {{ border: none; border-top: 1px solid {rule}; }}
.alt {{ color: {muted}; font-style: italic; }}
"
    )
}

/// The reduced document, ready to be drawn. It runs on a thread of its own, with a known stack;
/// past `budget` it is refused, and the thread is left to end on its own (the bounds above keep
/// that short).
pub fn reduce(html: &str, theme: Theme, width: u32, budget: Duration) -> Result<String, Refused> {
    if html.len() > MAX_HTML {
        return Err(Refused::TooLarge);
    }
    let html = html.to_owned();
    let (send, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("reduce".to_owned())
        .stack_size(STACK)
        .spawn(move || {
            let _ = send.send(reduced(&html, &theme, width));
        })
        .map_err(|_| Refused::Failed)?;
    match receive.recv_timeout(budget) {
        Ok(reduced) => reduced,
        Err(RecvTimeoutError::Timeout) => Err(Refused::TooLong),
        Err(RecvTimeoutError::Disconnected) => Err(Refused::Failed),
    }
}

fn reduced(html: &str, theme: &Theme, width: u32) -> Result<String, Refused> {
    let html = flatten(html, FLATTEN_AT);
    // No net provider: Blitz's default one drops every request.
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            viewport: Some(Viewport::new(width, 600, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );
    if depth(&document) > MAX_TREE {
        return Err(Refused::TooDeep);
    }
    document.resolve(0.0);
    let mut reducer = Reducer {
        document: &document,
        out: String::new(),
        in_data_table: false,
        depth: 0,
    };
    reducer.children(document.root_node());
    Ok(format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>{}</style></head><body>{}</body></html>",
        stylesheet(theme),
        reducer.out
    ))
}

/// How deep a document's tree is, found without recursion.
fn depth(document: &BaseDocument) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(document.root_node(), 1)];
    while let Some((node, depth)) = pending.pop() {
        deepest = deepest.max(depth);
        pending.extend(
            node.children
                .iter()
                .filter_map(|&child| document.get_node(child))
                .map(|child| (child, depth + 1)),
        );
    }
    deepest
}

struct Reducer<'a> {
    document: &'a BaseDocument,
    out: String,
    in_data_table: bool,
    /// How many elements of `out` are open.
    depth: usize,
}

/// Kept as they are, without their attributes.
const KEPT: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "blockquote",
    "pre",
    "code",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "sub",
    "sup",
];

/// Kept as a plain block, so that what they separate stays separate.
const BLOCKS: &[&str] = &[
    "div",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "center",
    "nav",
    "aside",
    "address",
    "figure",
    "figcaption",
];

/// Dropped with everything inside them.
const DROPPED: &[&str] = &[
    "head", "script", "style", "title", "meta", "link", "iframe", "object", "embed", "noscript",
    "template", "svg", "math", "form", "input", "select", "textarea", "video", "audio", "canvas",
];

impl Reducer<'_> {
    fn children(&mut self, node: &Node) {
        if self.has_breaks(node) {
            return self.lines(node);
        }
        for &child in &node.children {
            if let Some(child) = self.document.get_node(child) {
                self.node(child);
            }
        }
    }

    /// Whether a line break is among the children of a node.
    fn has_breaks(&self, node: &Node) -> bool {
        node.children
            .iter()
            .any(|&child| self.document.get_node(child).is_some_and(is_break))
    }

    /// The children of a node cut at its line breaks, each line a block of its own and a blank
    /// line an empty one: a text view that lays a paragraph out as one run of text (gpui-base's)
    /// would otherwise join the lines of a plain-text message.
    fn lines(&mut self, node: &Node) {
        // Too deep for a block per line, the lines are joined by breaks.
        let blocks = self.depth < MAX_DEPTH;
        self.depth += usize::from(blocks);
        let mut line = String::new();
        let mut lines = Vec::new();
        for &child in &node.children {
            let Some(child) = self.document.get_node(child) else {
                continue;
            };
            if is_break(child) {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            let outer = std::mem::take(&mut self.out);
            self.node(child);
            line.push_str(&std::mem::replace(&mut self.out, outer));
        }
        // A last break ends the last line; it does not start another.
        if !line.trim().is_empty() {
            lines.push(line);
        }
        self.depth -= usize::from(blocks);
        if !blocks {
            self.out.push_str(&lines.join("<br>"));
            return;
        }
        for line in lines {
            if line.trim().is_empty() {
                // A no-break space, not a `<br>`: gpui-base draws a lone break as nothing.
                self.out.push_str("<div>\u{a0}</div>");
            } else {
                self.out.push_str(&format!("<div>{line}</div>"));
            }
        }
    }

    fn node(&mut self, node: &Node) {
        match &node.data {
            NodeData::Text(text) => escape_into(&mut self.out, &text.content),
            NodeData::Element(element) => self.element(node, element),
            _ => {}
        }
    }

    fn element(&mut self, node: &Node, element: &ElementData) {
        let tag = element.name.local.as_ref();
        if DROPPED.contains(&tag) || self.is_hidden(node) {
            return;
        }
        match tag {
            "br" | "hr" => self.out.push_str(&format!("<{tag}>")),
            "a" => match element.attr(blitz_dom::local_name!("href")) {
                Some(href) if is_followable(href) => {
                    let mut attributes = " href=\"".to_owned();
                    escape_into(&mut attributes, href);
                    attributes.push('"');
                    self.wrapped_with("a", &attributes, node);
                }
                _ => self.children(node),
            },
            "img" => self.image(element),
            "table" => self.table(node),
            "tr" | "thead" | "tbody" | "tfoot" | "td" | "th" if self.in_data_table => {
                let mut attributes = String::new();
                for span in ["colspan", "rowspan"] {
                    if let Some(value) = element
                        .attrs()
                        .iter()
                        .find(|attr| &*attr.name.local == span)
                    {
                        attributes.push_str(&format!(" {span}=\""));
                        escape_into(&mut attributes, &value.value);
                        attributes.push('"');
                    }
                }
                self.wrapped_with(tag, &attributes, node);
            }
            "td" | "th" => self.wrapped("div", node),
            // A paragraph cut into lines holds blocks, which a paragraph cannot.
            "p" if self.has_breaks(node) => self.wrapped("div", node),
            _ if KEPT.contains(&tag) => self.wrapped(tag, node),
            _ if BLOCKS.contains(&tag) => self.wrapped("div", node),
            _ => self.children(node),
        }
    }

    fn is_hidden(&self, node: &Node) -> bool {
        self.document.resolved_style_value(node.id, "display") == "none"
            || self.document.resolved_style_value(node.id, "visibility") == "hidden"
    }

    fn wrapped(&mut self, tag: &str, node: &Node) {
        self.wrapped_with(tag, "", node);
    }

    /// The node's children in a `tag`, unless the output is already as deep as it may be.
    fn wrapped_with(&mut self, tag: &str, attributes: &str, node: &Node) {
        if self.depth >= MAX_DEPTH {
            return self.children(node);
        }
        self.out.push_str(&format!("<{tag}{attributes}>"));
        self.depth += 1;
        self.children(node);
        self.depth -= 1;
        self.out.push_str(&format!("</{tag}>"));
    }

    fn image(&mut self, element: &ElementData) {
        let src = element.attr(blitz_dom::local_name!("src")).unwrap_or("");
        let alt = element.attr(blitz_dom::local_name!("alt")).unwrap_or("");
        if src.starts_with("cid:") || src.starts_with("data:") {
            self.out.push_str("<img src=\"");
            escape_into(&mut self.out, src);
            self.out.push_str("\" alt=\"");
            escape_into(&mut self.out, alt);
            self.out.push_str("\">");
        } else if !alt.trim().is_empty() {
            let styled = self.depth < MAX_DEPTH;
            if styled {
                self.out.push_str("<span class=\"alt\">");
            }
            self.out.push('[');
            escape_into(&mut self.out, alt.trim());
            self.out.push(']');
            if styled {
                self.out.push_str("</span>");
            }
        }
    }

    fn table(&mut self, node: &Node) {
        let outer = self.in_data_table;
        // A table, its row group, a row and a cell: a data table needs room for all four.
        self.in_data_table = self.depth + 4 <= MAX_DEPTH && self.holds_data(node);
        if self.in_data_table {
            self.wrapped("table", node);
        } else {
            self.wrapped("div", node);
        }
        self.in_data_table = outer;
    }

    /// A table holds data when it has header cells, or is a plain grid of text (two rows and two
    /// columns at least, nothing nested); a table marked as presentation, or holding tables or
    /// images, lays the message out.
    fn holds_data(&self, table: &Node) -> bool {
        if let Some(element) = table.element_data()
            && element
                .attr(blitz_dom::local_name!("role"))
                .is_some_and(|role| role.eq_ignore_ascii_case("presentation"))
        {
            return false;
        }
        let mut shape = Shape::default();
        self.measure(table, &mut shape);
        if shape.nested_table || shape.image {
            return false;
        }
        shape.header || (shape.rows >= 2 && shape.columns >= 2)
    }

    fn measure(&self, node: &Node, shape: &mut Shape) {
        for &child in &node.children {
            let Some(child) = self.document.get_node(child) else {
                continue;
            };
            let Some(element) = child.element_data() else {
                continue;
            };
            match element.name.local.as_ref() {
                "table" => {
                    shape.nested_table = true;
                    continue;
                }
                "tr" => {
                    shape.rows += 1;
                    let cells = child
                        .children
                        .iter()
                        .filter_map(|&id| self.document.get_node(id)?.element_data())
                        .filter(|cell| matches!(cell.name.local.as_ref(), "td" | "th"))
                        .count();
                    shape.columns = shape.columns.max(cells);
                }
                "th" => shape.header = true,
                "img" => shape.image = true,
                _ => {}
            }
            self.measure(child, shape);
        }
    }
}

#[derive(Default)]
struct Shape {
    rows: usize,
    columns: usize,
    header: bool,
    image: bool,
    nested_table: bool,
}

fn is_break(node: &Node) -> bool {
    node.element_data()
        .is_some_and(|element| &*element.name.local == "br")
}

fn is_followable(href: &str) -> bool {
    let lower = href.trim().to_ascii_lowercase();
    lower.starts_with("https:") || lower.starts_with("http:") || lower.starts_with("mailto:")
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(html: &str) -> String {
        let reduced = reduce(html, Theme::Light, 800, Duration::from_secs(10)).unwrap();
        let start = reduced.find("<body>").unwrap() + "<body>".len();
        let end = reduced.rfind("</body>").unwrap();
        reduced[start..end].to_string()
    }

    #[test]
    fn a_table_with_header_cells_stays_a_table() {
        let out = body(
            "<table><tr><th>Item</th><th>Price</th></tr><tr><td>Tea</td><td>4</td></tr></table>",
        );
        assert!(out.contains("<table>"), "{out}");
        assert!(out.contains("<th>Item</th>"), "{out}");
    }

    #[test]
    fn a_layout_table_becomes_blocks() {
        let out = body(
            "<table role=\"presentation\"><tr><td>Left</td><td>Right</td></tr>\
             <tr><td>Below</td><td>Too</td></tr></table>",
        );
        assert!(!out.contains("<table"), "{out}");
        assert!(out.contains("<div>Left</div>"), "{out}");
    }

    #[test]
    fn a_table_holding_a_table_lays_out() {
        let out = body(
            "<table><tr><td><table><tr><th>A</th></tr></table></td></tr><tr><td>x</td></tr></table>",
        );
        assert_eq!(out.matches("<table>").count(), 1, "{out}");
    }

    #[test]
    fn a_remote_image_leaves_its_alternative_text() {
        let out = body("<p><img src=\"https://images.example.org/a.png\" alt=\"Our logo\"></p>");
        assert!(!out.contains("example.org"), "{out}");
        assert!(out.contains("[Our logo]"), "{out}");
    }

    #[test]
    fn an_image_of_the_message_is_kept() {
        let out = body("<img src=\"cid:logo@example.org\" alt=\"\">");
        assert!(out.contains("src=\"cid:logo@example.org\""), "{out}");
    }

    #[test]
    fn hidden_text_and_scripts_are_dropped() {
        let out = body(
            "<style>.pre { display: none }</style><div class=\"pre\">preheader</div>\
             <div style=\"display: none\">inline</div><script>alert(1)</script><p>Shown</p>",
        );
        assert!(!out.contains("preheader"), "{out}");
        assert!(!out.contains("inline"), "{out}");
        assert!(!out.contains("alert"), "{out}");
        assert!(out.contains("<p>Shown</p>"), "{out}");
    }

    #[test]
    fn a_link_keeps_only_its_target() {
        let out = body(
            "<a href=\"https://www.example.org/a\" style=\"color:red\" onclick=\"x()\">Go</a>\
             <a href=\"javascript:x()\">No</a>",
        );
        assert!(
            out.contains("<a href=\"https://www.example.org/a\">Go</a>"),
            "{out}"
        );
        assert!(!out.contains("javascript"), "{out}");
    }

    #[test]
    fn each_line_broken_by_a_line_break_is_a_block_and_a_blank_line_stays() {
        let out = body("<p>Hi all,<br><br>Notes:<br>1. <b>Agreed</b>.<br></p>");
        assert_eq!(
            out,
            "<div><div>Hi all,</div><div>\u{a0}</div><div>Notes:</div>\
             <div>1. <b>Agreed</b>.</div></div>"
        );
    }

    #[test]
    fn the_dark_theme_sets_a_dark_background() {
        let reduced = reduce("<p>x</p>", Theme::Dark, 800, Duration::from_secs(10)).unwrap();
        assert!(reduced.contains("background: #242424"));
    }
}
