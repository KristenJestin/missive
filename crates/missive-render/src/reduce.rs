//! The reduced form: the message cut down to simple content, drawn in a system-like theme.
//!
//! What is kept: paragraphs, headings, lists, quotes, preformatted text, emphasis, links, the
//! message's own images (`cid:`, `data:`), and tables only when they hold data. Everything else
//! (the author's CSS, layout tables, remote images, forms, scripts) is dropped; a dropped remote
//! image leaves its alternative text. What the author's CSS hides (a newsletter's preheader, most
//! often) is not content: Blitz computes the styles, without fetching anything, to find it.

use blitz_dom::{BaseDocument, DocumentConfig, ElementData, Node, NodeData};
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};

pub enum Theme {
    Light,
    Dark,
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

/// The reduced document, ready to be drawn.
pub fn reduce(html: &str, theme: &Theme, width: u32) -> String {
    // No net provider: Blitz's default one drops every request.
    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(width, 600, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );
    document.resolve(0.0);
    let mut reducer = Reducer {
        document: &document,
        out: String::new(),
        in_data_table: false,
    };
    reducer.children(document.root_node());
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>{}</style></head><body>{}</body></html>",
        stylesheet(theme),
        reducer.out
    )
}

struct Reducer<'a> {
    document: &'a BaseDocument,
    out: String,
    in_data_table: bool,
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
        for &child in &node.children {
            if let Some(child) = self.document.get_node(child) {
                self.node(child);
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
                    self.out.push_str("<a href=\"");
                    escape_into(&mut self.out, href);
                    self.out.push_str("\">");
                    self.children(node);
                    self.out.push_str("</a>");
                }
                _ => self.children(node),
            },
            "img" => self.image(element),
            "table" => self.table(node),
            "tr" | "thead" | "tbody" | "tfoot" | "td" | "th" if self.in_data_table => {
                self.out.push_str(&format!("<{tag}"));
                for span in ["colspan", "rowspan"] {
                    if let Some(value) = element
                        .attrs()
                        .iter()
                        .find(|attr| &*attr.name.local == span)
                    {
                        self.out.push_str(&format!(" {span}=\""));
                        escape_into(&mut self.out, &value.value);
                        self.out.push('"');
                    }
                }
                self.out.push('>');
                self.children(node);
                self.out.push_str(&format!("</{tag}>"));
            }
            "td" | "th" => self.wrapped("div", node),
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
        self.out.push_str(&format!("<{tag}>"));
        self.children(node);
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
            self.out.push_str("<span class=\"alt\">[");
            escape_into(&mut self.out, alt.trim());
            self.out.push_str("]</span>");
        }
    }

    fn table(&mut self, node: &Node) {
        let outer = self.in_data_table;
        self.in_data_table = self.holds_data(node);
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
        let reduced = reduce(html, &Theme::Light, 800);
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
    fn the_dark_theme_sets_a_dark_background() {
        assert!(reduce("<p>x</p>", &Theme::Dark, 800).contains("background: #242424"));
    }
}
