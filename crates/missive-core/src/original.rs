//! "View original": a message as a page for the system's browser, cleaned of everything that
//! runs or phones home.
//!
//! The page is opened in a real browser, with the user's sessions: the message's HTML is hostile
//! input. Everything is kept by allow-list, never removed by block-list:
//!
//! - the HTML goes through ammonia (html5ever's parser), with its elements and attributes plus
//!   those of mail layouts (`font`, table attributes, `style`, `class`, `id`); links keep
//!   `http`, `https`, `mailto` and `#` anchors; an image keeps its source only when it is a
//!   `data:` image of the message (a `cid:` part, inlined): remote images, tracking pixels
//!   included, never load;
//! - the stylesheets (`<style>`) and `style` attributes go through [`sheet`] and
//!   [`declarations`]: only `@media` rules and plain rules stay, with the properties of
//!   [`PROPERTIES`] and the functions of [`FUNCTIONS`]; `url()` only for a `data:` image;
//! - a Content-Security-Policy comes first in the page, as a second line of defence; it does not
//!   stop a browser from resolving the host names of the links ahead of a click, so DNS prefetch
//!   is turned off beside it.
//!
//! A message with only text becomes a plain-text page: escaped, never interpreted.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use ammonia::{Builder, Url, UrlRelative};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use html5ever::LocalName;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use mail_parser::{MessageParser, MimeHeaders};

/// No script, no load but the page's own `data:` images and styles, no form, no `<base>`.
const POLICY: &str = "default-src 'none'; img-src data:; style-src 'unsafe-inline'; \
                      base-uri 'none'; form-action 'none'";

/// The image types a page may show: no SVG, which is a document of its own.
const IMAGES: [&str; 4] = ["png", "gif", "jpeg", "webp"];

/// The deepest nesting of elements that is cleaned. The cleaning (the parser's tree) takes time
/// quadratic in the depth: 5 000 levels take 50 ms, 50 000 take 5 s. A browser flattens
/// anything past 512 levels itself; no real message gets near.
const MAX_DEPTH: usize = 1024;

/// The longest HTML that is cleaned, in bytes.
const MAX_HTML: usize = 2 * 1024 * 1024;

/// The page of a raw message.
pub fn page(raw: &[u8]) -> String {
    let body = crate::body::read(raw);
    let (style, content) = match (&body.html, &body.text) {
        (Some(html), _) => {
            // What cannot be cleaned in a bounded time is shown as text, never half cleaned.
            let notice = if html.len() > MAX_HTML {
                Some("The original is too large to clean: here is its text.")
            } else if too_deep(html) {
                Some("The original is too deeply nested to clean: here is its text.")
            } else {
                None
            };
            match notice {
                Some(notice) => plain(body.text.as_deref().unwrap_or(&body.readable), notice),
                None => {
                    let style: Vec<String> = stylesheets(html).iter().map(|s| sheet(s)).collect();
                    (style.concat(), clean(html, images(raw)))
                }
            }
        }
        (None, Some(text)) => plain(text, ""),
        (None, None) => (String::new(), String::new()),
    };
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{POLICY}\">\
         <meta http-equiv=\"x-dns-prefetch-control\" content=\"off\">\
         <style>{style}</style></head><body>{content}</body></html>\n"
    )
}

/// The style and content of a page showing text, escaped and never interpreted, under a line
/// saying why when `notice` is not empty.
fn plain(text: &str, notice: &str) -> (String, String) {
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p><em>{notice}</em></p>")
    };
    (
        "pre{white-space:pre-wrap}".to_owned(),
        format!("{notice}<pre>{}</pre>", escape(&text.replace("\r\n", "\n"))),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The image parts of a message, as `data:` URLs, by `Content-ID`.
fn images(raw: &[u8]) -> HashMap<String, String> {
    let Some(message) = MessageParser::default().parse(raw) else {
        return HashMap::new();
    };
    message
        .parts
        .iter()
        .filter_map(|part| {
            let id = part.content_id()?;
            let kind = part.content_type()?;
            let subtype = kind.subtype()?.to_ascii_lowercase();
            if !kind.ctype().eq_ignore_ascii_case("image") || !IMAGES.contains(&subtype.as_str()) {
                return None;
            }
            let data = STANDARD.encode(part.contents());
            Some((id.to_owned(), format!("data:image/{subtype};base64,{data}")))
        })
        .collect()
}

/// Whether a URL is a `data:` image this page may show, base64 and nothing else.
fn data_image(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("data:image/") else {
        return false;
    };
    IMAGES.iter().any(|kind| {
        rest.strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix(";base64,"))
            .is_some_and(|data| {
                data.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
            })
    })
}

/// The message's HTML, cleaned, with its `cid:` images inlined.
fn clean(html: &str, images: HashMap<String, String>) -> String {
    let mut builder = Builder::default();
    builder
        .add_tags(["font", "big"])
        // Their text would show as text (a noscript is parsed as if scripts ran); the stylesheets
        // are cleaned apart, by `sheet`.
        .add_clean_content_tags(["title", "noscript"])
        .add_generic_attributes([
            "style", "class", "id", "dir", "align", "valign", "bgcolor", "width", "height",
        ])
        .add_tag_attributes("a", ["name"])
        .add_tag_attributes("font", ["color", "face", "size"])
        .add_tag_attributes("img", ["border", "hspace", "vspace"])
        .add_tag_attributes("table", ["border", "cellpadding", "cellspacing"])
        .add_tag_attributes("td", ["colspan", "rowspan", "nowrap"])
        .add_tag_attributes("th", ["colspan", "rowspan", "nowrap"])
        // `cid` and `data` reach the filter, which keeps them for images only.
        .url_schemes(HashSet::from(["http", "https", "mailto", "cid", "data"]))
        // A relative address would point into the user's files, next to the page.
        .url_relative(UrlRelative::Custom(Box::new(anchor)))
        .attribute_filter(
            move |element, attribute, value| match (element, attribute) {
                (_, "style") => {
                    Some(declarations(value).into()).filter(|v: &Cow<str>| !v.is_empty())
                }
                ("img", "src") => {
                    if data_image(value) {
                        return Some(value.into());
                    }
                    let id = value.strip_prefix("cid:")?;
                    images.get(id).map(|url| url.clone().into())
                }
                (_, "href") => match Url::parse(value) {
                    Ok(url) if !matches!(url.scheme(), "http" | "https" | "mailto") => None,
                    _ => Some(value.into()),
                },
                _ => Some(value.into()),
            },
        );
    builder.clean(html).to_string()
}

/// A relative address is kept only when it is an anchor of the page itself.
fn anchor(url: &str) -> Option<Cow<'_, str>> {
    url.starts_with('#').then_some(Cow::Borrowed(url))
}

/// The text of each `<style>` of an HTML document, in order. The tokenizer is told, as the
/// parser does, which elements hold raw text, so that a `<style>` written inside a script or a
/// title is no stylesheet.
fn stylesheets(html: &str) -> Vec<String> {
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));
    let tokenizer = Tokenizer::new(Styles::default(), TokenizerOpts::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.sheets.take()
}

#[derive(Default)]
struct Styles {
    inside: Cell<bool>,
    sheets: RefCell<Vec<String>>,
}

impl TokenSink for Styles {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) => {
                self.inside.set(false);
                if tag.kind == TagKind::StartTag {
                    if &*tag.name == "style" {
                        self.inside.set(true);
                        self.sheets.borrow_mut().push(String::new());
                    }
                    if let Some(result) = raw_text(&tag.name) {
                        return result;
                    }
                }
            }
            Token::CharacterTokens(text) if self.inside.get() => {
                if let Some(sheet) = self.sheets.borrow_mut().last_mut() {
                    sheet.push_str(&text);
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// The state a tokenizer goes to after the start tag of an element that holds raw text.
fn raw_text(name: &str) -> Option<TokenSinkResult<()>> {
    Some(match name {
        "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
            TokenSinkResult::RawData(RawKind::Rawtext)
        }
        "script" => TokenSinkResult::RawData(RawKind::ScriptData),
        "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
        "plaintext" => TokenSinkResult::Plaintext,
        _ => return None,
    })
}

/// Whether the elements of an HTML document nest deeper than [`MAX_DEPTH`], in one linear pass
/// that only counts and changes nothing of what is cleaned afterwards. It is counted twice: as
/// the parser reads raw text (a `</div>` inside a script closes nothing), and with every tag
/// taken for markup (a `<title>` inside an `<svg>` is no raw text for the parser, and the
/// tokenizer cannot know). Either count over the cap is enough, so that the document is never
/// counted shallower than the parser sees it.
fn too_deep(html: &str) -> bool {
    [true, false].into_iter().any(|raw| {
        let input = BufferQueue::default();
        input.push_back(StrTendril::from_slice(html));
        let tokenizer = Tokenizer::new(
            Depth {
                raw,
                ..Depth::default()
            },
            TokenizerOpts::default(),
        );
        let _ = tokenizer.feed(&input);
        tokenizer.sink.deep.get()
    })
}

/// Elements with no end tag, which open nothing.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

#[derive(Default)]
struct Depth {
    raw: bool,
    open: RefCell<Vec<LocalName>>,
    deep: Cell<bool>,
}

impl TokenSink for Depth {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let Token::TagToken(tag) = token else {
            return TokenSinkResult::Continue;
        };
        let mut open = self.open.borrow_mut();
        match tag.kind {
            TagKind::StartTag => {
                if !VOID.contains(&&*tag.name) {
                    open.push(tag.name.clone());
                    if open.len() > MAX_DEPTH {
                        self.deep.set(true);
                    }
                }
                if self.raw
                    && let Some(result) = raw_text(&tag.name)
                {
                    return result;
                }
            }
            // The nearest open element of that name, and what is inside it, closes; an end tag
            // matching nothing is ignored, as the parser does.
            TagKind::EndTag => {
                if let Some(at) = open.iter().rposition(|name| *name == tag.name) {
                    open.truncate(at);
                }
            }
        }
        TokenSinkResult::Continue
    }
}

/// The properties a declaration may set, with their families (`border` is `border-left` too), or
/// begin with when they end in `-`: boxes, colours,
/// text, tables, flex and grid. None of them positions content over the page (`position`), runs
/// anything (`behavior`, `-moz-binding`) or holds generated content (`content`, `cursor`).
const PROPERTIES: &[&str] = &[
    "align-",
    "aspect-ratio",
    "background",
    "border",
    "box-shadow",
    "box-sizing",
    "caption-side",
    "clear",
    "color",
    "column-gap",
    "direction",
    "display",
    "empty-cells",
    "flex",
    "float",
    "font",
    "gap",
    "grid",
    "height",
    "hyphens",
    "justify-",
    "letter-spacing",
    "line-height",
    "list-style",
    "margin",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "object-fit",
    "opacity",
    "order",
    "outline",
    "overflow",
    "padding",
    "row-gap",
    "tab-size",
    "table-layout",
    "text-",
    "unicode-bidi",
    "vertical-align",
    "visibility",
    "white-space",
    "width",
    "word-break",
    "word-spacing",
    "word-wrap",
    "-webkit-text-size-adjust",
    "-ms-text-size-adjust",
];

/// The functions a value may call: colours, lengths, gradients, grid tracks, and `url()` for a
/// `data:` image only. An unnamed parenthesis (inside `calc()`) is allowed too.
const FUNCTIONS: &[&str] = &[
    "",
    "rgb",
    "rgba",
    "hsl",
    "hsla",
    "hwb",
    "calc",
    "min",
    "max",
    "clamp",
    "linear-gradient",
    "radial-gradient",
    "conic-gradient",
    "repeating-linear-gradient",
    "repeating-radial-gradient",
    "repeat",
    "minmax",
    "fit-content",
    "url",
];

fn allowed_property(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        && PROPERTIES.iter().any(|allowed| {
            if allowed.ends_with('-') {
                name.len() > allowed.len() && name.starts_with(allowed)
            } else {
                // A property, and its family: `border` is `border-left-color` too.
                name.strip_prefix(allowed)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
            }
        })
}

/// What the browser could read otherwise than this cleaner goes first: escapes (`\75rl(` is
/// `url(`), comments (each becomes a space, so that none can join two words), the HTML comment
/// markers some mails wrap their styles in, line breaks (which end a string), and `<` (with
/// which a stylesheet could close its element). What is left has strings that end at their
/// quote, and nothing else to hide a word.
fn normalise(css: &str) -> String {
    let css = css
        .replace('\\', "")
        .replace("<!--", " ")
        .replace("-->", " ")
        .replace('<', "");
    let mut out = String::with_capacity(css.len());
    let mut rest = css.as_str();
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out.replace(['\n', '\r', '\x0c'], " ")
}

/// The position of the first of `stops` in `css` from `from` that is outside strings and,
/// with `nested`, outside parentheses; `css.len()` when there is none. `None` when a string or
/// a parenthesis is still open at the end.
fn until(css: &str, from: usize, stops: &[u8], nested: bool) -> Option<usize> {
    let bytes = css.as_bytes();
    let mut depth = 0usize;
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' | b'\'' => {
                i += 1 + css[i + 1..].find(b as char)?;
            }
            b'(' if nested => depth += 1,
            b')' if nested => depth = depth.checked_sub(1)?,
            _ if depth == 0 && stops.contains(&b) => return Some(i),
            _ => {}
        }
        i += 1;
    }
    (depth == 0).then_some(bytes.len())
}

/// The position of the `}` that closes the block opened at `open`, or `css.len()`.
fn closing(css: &str, open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while let Some(at) = until(css, i, b"{}", false).filter(|&at| at < css.len()) {
        if css.as_bytes()[at] == b'{' {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return at;
            }
        }
        i = at + 1;
    }
    css.len()
}

/// A stylesheet, cleaned: its plain rules and `@media` rules, each with its allowed
/// declarations; every other at-rule (`@import`, `@font-face`…) goes.
fn sheet(css: &str) -> String {
    let mut out = String::new();
    rules(&normalise(css), 0, &mut out);
    out
}

fn rules(css: &str, depth: u8, out: &mut String) {
    let mut i = 0;
    while i < css.len() {
        let Some(stop) = until(css, i, b"{;}", false).filter(|&stop| stop < css.len()) else {
            return;
        };
        let prelude = css[i..stop].trim();
        if css.as_bytes()[stop] != b'{' {
            // An at-rule without a block (`@import`, `@charset`), or a stray character.
            i = stop + 1;
            continue;
        }
        let end = closing(css, stop);
        let inner = &css[stop + 1..end];
        i = end + 1;
        if let Some(media) = prelude
            .strip_prefix('@')
            .filter(|rule| rule.len() > 5 && rule[..5].eq_ignore_ascii_case("media"))
        {
            let query_ok = media
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b" ():,.-_%".contains(&b));
            let mut nested = String::new();
            if query_ok && depth < 4 {
                rules(inner, depth + 1, &mut nested);
            }
            if !nested.is_empty() {
                out.push_str(&format!("{prelude}{{{nested}}}"));
            }
        } else if !prelude.is_empty()
            && !prelude.starts_with('@')
            && !prelude.to_ascii_lowercase().contains("url(")
        {
            let kept = clean_declarations(inner);
            if !kept.is_empty() {
                out.push_str(&format!("{prelude}{{{kept}}}"));
            }
        }
    }
}

/// The declarations of a `style` attribute, cleaned.
fn declarations(css: &str) -> String {
    clean_declarations(&normalise(css))
}

fn clean_declarations(css: &str) -> String {
    let mut kept = Vec::new();
    let mut i = 0;
    while i < css.len() {
        // A string or a parenthesis open to the end: what is left is dropped.
        let Some(end) = until(css, i, b";", true) else {
            break;
        };
        let declaration = &css[i..end];
        i = end + 1;
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        if allowed_property(&name) && !value.is_empty() && allowed_value(value) {
            kept.push(format!("{name}:{value}"));
        }
    }
    kept.join(";")
}

/// Whether every function of a value, inside its strings too, is allowed, and every `url()` a
/// `data:` image. Strings are searched as well, so that nothing in the page looks like a call
/// but what was checked.
fn allowed_value(value: &str) -> bool {
    if value.contains(['{', '}']) {
        return false;
    }
    value.match_indices('(').all(|(at, _)| {
        let start = value[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .map_or(0, |p| p + 1);
        let name = value[start..at].to_ascii_lowercase();
        if !FUNCTIONS.contains(&name.as_str()) {
            return false;
        }
        if name != "url" {
            return true;
        }
        let Some(close) = value[at..].find(')') else {
            return false;
        };
        let argument = value[at + 1..at + close].trim();
        let argument = ['"', '\'']
            .iter()
            .find_map(|q| argument.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(argument);
        data_image(argument)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message whose only part is this HTML.
    fn html(body: &str) -> String {
        page(
            format!(
                "From: Carol Sender <carol.sender@example.com>\r\nSubject: Hi\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\r\n{body}\r\n"
            )
            .as_bytes(),
        )
    }

    /// What the page holds after its head, where the policy is.
    fn body(page: &str) -> &str {
        &page[page.find("</head>").expect("a head")..]
    }

    fn lacks(page: &str, words: &[&str]) {
        let lower = page.to_ascii_lowercase();
        for word in words {
            assert!(!lower.contains(word), "{word:?} is still in\n{page}");
        }
    }

    #[test]
    fn the_page_starts_with_a_policy_that_forbids_scripts_and_remote_loads() {
        let page = html("<p>Hello</p>");
        let head = &page[..page.find("</head>").unwrap()];
        assert!(page.starts_with("<!DOCTYPE html>"), "{page}");
        assert!(head.contains("<meta charset=\"utf-8\">"), "{page}");
        assert!(
            head.contains(
                "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
                 img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\">"
            ),
            "{page}"
        );
        // Before anything of the message's.
        assert!(head.find("Content-Security-Policy") < head.find("<style>"));
        assert!(body(&page).contains("<p>Hello</p>"));
    }

    #[test]
    fn the_page_turns_dns_prefetch_off_before_any_content() {
        for page in [
            html("<p>Hello</p>"),
            html("<style>p { color: red }</style><p>Hello</p>"),
            page(b"Subject: Hi\r\n\r\nHello\r\n"),
        ] {
            let head = &page[..page.find("</head>").unwrap()];
            let control = "<meta http-equiv=\"x-dns-prefetch-control\" content=\"off\">";
            assert!(head.contains(control), "{page}");
            assert!(head.find(control) < head.find("<style>"), "{page}");
            assert!(head.find(control) < page.find("<body>"), "{page}");
        }
    }

    #[test]
    fn no_other_prefetch_hint_is_let_through() {
        let page = html(
            "<link rel=\"dns-prefetch\" href=\"//a.example.net\">\
             <link rel=\"preconnect\" href=\"https://b.example.net\">\
             <link rel=\"prefetch\" href=\"https://c.example.net/x\">\
             <link rel=\"preload\" href=\"https://d.example.net/x\" as=\"image\">\
             <link rel=\"prerender\" href=\"https://e.example.net/x\">\
             <a href=\"https://f.example.net/\" ping=\"https://g.example.net/p\" \
             rel=\"prefetch\">link</a>",
        );
        lacks(
            &page,
            &[
                "<link",
                "example.net/p",
                "a.example.net",
                "b.example.net",
                "c.example.net",
                "d.example.net",
                "e.example.net",
                "g.example.net",
                "ping",
                "prefetch\" href",
            ],
        );
        // The link itself stays, with no relation but the one ammonia sets.
        assert!(body(&page).contains("f.example.net"), "{page}");
        assert!(!body(&page).contains("rel=\"prefetch"), "{page}");
    }

    #[test]
    fn scripts_in_every_form_are_removed() {
        let page = html(
            "<script>alert(1)</script><SCRIPT SRC=\"https://evil.example.net/x.js\"></SCRIPT>\
             <p onclick=\"alert(2)\" onmouseover=alert(3)>Text</p>\
             <img src=\"x\" onerror=\"alert(4)\">\
             <a href=\"javascript:alert(5)\">a</a><a href=\"JaVaScRiPt:alert(6)\">b</a>\
             <a href=\"&#106;avascript:alert(7)\">c</a><a href=\" javascript:alert(8)\">d</a>\
             <a href=\"data:text/html;base64,PHNjcmlwdD5hbGVydCg5KTwvc2NyaXB0Pg==\">e</a>\
             <a href=\"vbscript:alert(10)\">f</a>\
             <svg onload=\"alert(11)\"><script>alert(12)</script></svg>\
             <math><mtext><script>alert(13)</script></mtext></math>\
             <iframe srcdoc=\"<script>alert(14)</script>\"></iframe>\
             <object data=\"https://evil.example.net/o\"></object>\
             <embed src=\"https://evil.example.net/e\">\
             <form action=\"https://evil.example.net/f\"><input name=\"p\"><button>Go</button></form>\
             <noscript><p>no</p></noscript><template><script>alert(15)</script></template>",
        );
        lacks(
            &page,
            &[
                "<script",
                "alert",
                "onclick",
                "onmouseover",
                "onerror",
                "onload",
                "javascript",
                "vbscript",
                "data:text",
                "<svg",
                "<math",
                "<iframe",
                "srcdoc",
                "<object",
                "<embed",
                "<form",
                "<input",
                "<button",
                "evil.example.net",
            ],
        );
        assert!(body(&page).contains("Text"));
    }

    #[test]
    fn a_style_with_expression_or_import_is_removed() {
        let page = html(
            "<style>@import url(https://evil.example.net/a.css); @import 'https://evil.example.net/b.css';\
             p { color: red; width: expression(alert(1)); behavior: url(x.htc); -moz-binding: url(x.xml#y) }\
             @font-face { font-family: F; src: url(https://evil.example.net/f.woff) }\
             </style><p style=\"color: blue; width: expression(alert(2))\">x</p>",
        );
        lacks(
            &page,
            &[
                "@import",
                "expression",
                "behavior",
                "binding",
                "@font-face",
                "evil.example.net",
            ],
        );
        assert!(page.contains("p{color:red}"), "{page}");
        assert!(page.contains("style=\"color:blue\""), "{page}");
    }

    #[test]
    fn a_property_outside_the_allow_list_is_removed() {
        let page = html(
            "<p style=\"position: fixed; top: 0; z-index: 9; color: red; filter: url(#f); \
             content: 'x'; mask: none; border-left-color: blue; text-align: center\">x</p>",
        );
        assert!(
            page.contains("style=\"color:red;border-left-color:blue;text-align:center\""),
            "{page}"
        );
    }

    #[test]
    fn remote_images_and_a_tracking_pixel_are_removed() {
        let page = html(
            "<p>News</p><img src=\"https://images.example.net/banner.png\" alt=\"Banner\">\
             <img src=\"http://tracker.example.net/open?u=42\" width=\"1\" height=\"1\">\
             <img src=\"//tracker.example.net/p.gif\"><img src=\"/local/file.png\">\
             <img srcset=\"https://images.example.net/2x.png 2x\">\
             <picture><source srcset=\"https://images.example.net/s.webp\"></picture>\
             <table background=\"https://images.example.net/bg.png\"><tr><td>Cell</td></tr></table>\
             <video poster=\"https://images.example.net/poster.png\"></video>\
             <link rel=\"stylesheet\" href=\"https://evil.example.net/s.css\">",
        );
        lacks(
            &page,
            &[
                "images.example.net",
                "tracker.example.net",
                "evil.example.net",
                "/local/",
                "<link",
            ],
        );
        // The image goes, what it said stays.
        assert!(body(&page).contains("alt=\"Banner\""), "{page}");
        assert!(body(&page).contains("Cell"));
    }

    #[test]
    fn a_css_url_pointing_outside_the_message_is_removed() {
        let page = html(
            "<style>.a { background: url(https://evil.example.net/1.png) }\
             .b { background-image: url( \"https://evil.example.net/2.png\" ) }\
             .c { background: \\75rl(https://evil.example.net/3.png) }\
             .d { background: u/**/rl(https://evil.example.net/4.png) }\
             .e { list-style: image-set(\"https://evil.example.net/5.png\" 1x) }\
             .f { background: URL(cid:x) }\
             .g { background: url(data:image/png;base64,AAAA); color: green }\
             .h { content: url(https://evil.example.net/6.png); cursor: url(https://evil.example.net/7.cur) }\
             </style>\
             <p style=\"background: url('https://evil.example.net/8.png'); color: red\">x</p>\
             <p style=\"background: u&#114;l(https://evil.example.net/9.png)\">y</p>",
        );
        lacks(&page, &["evil.example.net", "cid:", "image-set"]);
        // A data image of the message stays.
        assert!(
            page.contains(".g{background:url(data:image/png;base64,AAAA);color:green}"),
            "{page}"
        );
        assert!(page.contains("style=\"color:red\""), "{page}");
    }

    #[test]
    fn a_stylesheet_cannot_close_its_own_element() {
        let page = html(
            "<style>p { font-family: \"<\\/style><script>alert(1)</script>\" }</style><p>x</p>",
        );
        lacks(&page, &["<script", "alert(1)</"]);
        assert_eq!(page.matches("</style>").count(), 1, "{page}");
    }

    #[test]
    fn meta_refresh_and_base_are_removed() {
        let page = html(
            "<html><head><meta http-equiv=\"refresh\" content=\"0; url=https://evil.example.net/\">\
             <base href=\"https://evil.example.net/\"><title>Title</title></head>\
             <body><a href=\"page\">relative</a></body></html>",
        );
        lacks(
            &page,
            &["refresh", "<base", "evil.example.net", "href=\"page\""],
        );
        assert!(body(&page).contains("relative"));
    }

    #[test]
    fn cid_images_are_inlined_as_data_urls() {
        let page = page(
            b"From: Carol Sender <carol.sender@example.com>\r\nSubject: Logo\r\nMIME-Version: 1.0\r\n\
              Content-Type: multipart/related; boundary=\"r\"\r\n\r\n\
              --r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
              <p><img src=\"cid:logo@example.com\" alt=\"Logo\"><img src=\"cid:script@example.com\">\
              <img src=\"cid:missing@example.com\"></p>\r\n\
              --r\r\nContent-Type: image/png\r\nContent-ID: <logo@example.com>\r\n\
              Content-Transfer-Encoding: base64\r\n\r\niVBORw0K\r\n\
              --r\r\nContent-Type: text/html\r\nContent-ID: <script@example.com>\r\n\r\n\
              <script>alert(1)</script>\r\n--r--\r\n",
        );
        assert!(
            page.contains("<img src=\"data:image/png;base64,iVBORw0K\" alt=\"Logo\">"),
            "{page}"
        );
        // Only an image part becomes an image; a missing one is no image at all.
        lacks(&page, &["cid:", "data:text", "alert"]);
        assert_eq!(page.matches("<img src=").count(), 1, "{page}");
    }

    #[test]
    fn links_stay_clickable() {
        let page = html(
            "<a href=\"https://example.org/read?a=1&amp;b=2\">Read</a> \
             <a href=\"mailto:carol.sender@example.com\">Write</a> <a href=\"#top\">Top</a>",
        );
        let body = body(&page);
        assert!(
            body.contains("<a href=\"https://example.org/read?a=1&amp;b=2\" rel=\"noopener noreferrer\">Read</a>"),
            "{page}"
        );
        assert!(
            body.contains("href=\"mailto:carol.sender@example.com\""),
            "{page}"
        );
        assert!(body.contains("href=\"#top\""), "{page}");
    }

    #[test]
    fn the_layout_is_kept() {
        let page = html(
            "<html><head><style><!-- .box { padding: 4px; border: 1px solid #ccc } \
             @media screen and (max-width: 600px) { .box { width: 100% !important } } --></style>\
             </head><body><table width=\"600\" cellpadding=\"0\" bgcolor=\"#ffffff\" align=\"center\">\
             <tr><td class=\"box\" style=\"font-family: Arial, 'Helvetica Neue', sans-serif; \
             color: rgb(10, 20, 30)\"><font color=\"red\">Red</font></td></tr></table></body></html>",
        );
        assert!(
            page.contains(
                "<style>.box{padding:4px;border:1px solid #ccc}\
                 @media screen and (max-width: 600px){.box{width:100% !important}}</style>"
            ),
            "{page}"
        );
        let body = body(&page);
        assert!(
            body.contains(
                "<table width=\"600\" cellpadding=\"0\" bgcolor=\"#ffffff\" align=\"center\">"
            ),
            "{page}"
        );
        assert!(
            body.contains(
                "<td class=\"box\" style=\"font-family:Arial, 'Helvetica Neue', sans-serif;\
                 color:rgb(10, 20, 30)\">"
            ),
            "{page}"
        );
        assert!(body.contains("<font color=\"red\">Red</font>"), "{page}");
    }

    #[test]
    fn a_text_only_message_is_an_escaped_plain_text_page() {
        let page = page(
            b"From: Carol Sender <carol.sender@example.com>\r\nSubject: Plain\r\n\
              Content-Type: text/plain; charset=utf-8\r\n\r\n\
              Line <one> & \"two\"\r\n<script>alert(1)</script>\r\n",
        );
        assert!(page.contains("Content-Security-Policy"));
        assert!(
            body(&page).contains(
                "<pre>Line &lt;one&gt; &amp; \"two\"\n&lt;script&gt;alert(1)&lt;/script&gt;\n</pre>"
            ),
            "{page}"
        );
        lacks(&page, &["<script"]);
    }

    #[test]
    fn a_message_that_is_none_gives_an_empty_page() {
        let page = page(b"\x00\x01\xff\xfe not a mail\r\n\x80");
        assert!(page.contains("Content-Security-Policy"));
        assert!(body(&page).starts_with("</head><body>"), "{page}");
    }

    /// The page of an HTML body of `inside` between `depth` times `open` and `close`.
    fn nested(open: &str, close: &str, depth: usize, inside: &str) -> String {
        html(&format!(
            "{}{inside}{}",
            open.repeat(depth),
            close.repeat(depth)
        ))
    }

    #[test]
    fn a_very_deeply_nested_message_is_cleaned_in_bounded_time_and_keeps_its_text() {
        let start = std::time::Instant::now();
        let page = nested("<div>", "</div>", 200_000, "Deep needle text");
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs_f32() < 1.0, "took {elapsed:?}");
        assert!(body(&page).contains("Deep needle text"));
        assert!(page.contains("Content-Security-Policy"));
        // Past the size cap here, or the depth cap: either way the text, not the markup.
        assert!(
            body(&page).contains("to clean: here is its text"),
            "{}",
            body(&page)
        );
        assert!(!body(&page).contains("<div"));
    }

    #[test]
    fn nesting_under_the_size_cap_but_far_over_the_depth_cap_is_shown_as_text() {
        let start = std::time::Instant::now();
        let page = nested("<div>", "</div>", 100_000, "Deep needle text");
        assert!(
            start.elapsed().as_secs_f32() < 1.0,
            "took {:?}",
            start.elapsed()
        );
        assert!(body(&page).contains("too deeply nested"), "{}", body(&page));
        assert!(body(&page).contains("Deep needle text"));
    }

    #[test]
    fn a_script_or_a_remote_image_beyond_the_depth_cap_is_still_absent() {
        let page = nested(
            "<div>",
            "</div>",
            MAX_DEPTH + 10,
            "<script>alert(1)</script><img src=\"https://evil.example.net/p.gif\" \
             onerror=\"alert(2)\"><a href=\"javascript:alert(3)\">x</a>Kept",
        );
        lacks(
            &page,
            &[
                "<script",
                "<img",
                "<a ",
                "onerror",
                "javascript:",
                "evil.example.net",
            ],
        );
        assert!(body(&page).contains("Kept"), "{}", body(&page));
    }

    #[test]
    fn a_message_at_the_depth_cap_is_still_cleaned_with_its_layout() {
        let page = nested(
            "<div>",
            "</div>",
            MAX_DEPTH - 1,
            "<p style=\"color: red\">Text</p><script>alert(1)</script>",
        );
        let body = body(&page);
        assert!(body.contains("<p style=\"color:red\">Text</p>"), "{page}");
        assert!(!body.contains("too deeply"), "{page}");
        lacks(&page, &["<script", "alert"]);
    }

    #[test]
    fn nesting_hidden_from_a_tokenizer_that_reads_raw_text_is_still_counted() {
        // The parser leaves the svg title at its first `<div>`: the divs nest, while a
        // tokenizer that took the title for raw text would see none of them.
        let page = nested("<div>", "", MAX_DEPTH + 10, "x");
        assert!(body(&page).contains("too deeply"));
        let svg = html(&format!(
            "<svg><title>{}Hidden</title></svg><script>alert(1)</script>",
            "<div>".repeat(MAX_DEPTH + 10)
        ));
        assert!(body(&svg).contains("too deeply"), "{}", body(&svg));
        lacks(&svg, &["<script", "<svg"]);
    }

    #[test]
    fn closing_tags_written_as_script_text_do_not_hide_the_nesting() {
        // A parser reads `</div>` inside a script as text: the div stays open.
        let page = nested("<div><script></div></script>", "", MAX_DEPTH + 10, "x");
        assert!(body(&page).contains("too deeply"), "{}", body(&page));
        lacks(&page, &["<script"]);
    }

    #[test]
    fn a_flat_message_with_many_elements_is_not_too_deep() {
        let page =
            html(&"<p><b>a</b><br><br><img src=\"data:image/png;base64,AAAA\"></p>".repeat(5_000));
        assert!(!body(&page).contains("too deeply"));
        assert!(body(&page).contains("<p><b>a</b>"));
    }

    #[test]
    fn a_message_over_the_size_cap_is_its_text_with_a_line_saying_so() {
        let big = format!(
            "<p>Start</p><script>alert(1)</script>{}<p>End</p>",
            "<p>filler</p>".repeat(MAX_HTML / 13 + 1)
        );
        let page = html(&big);
        let body = body(&page);
        assert!(body.contains("too large to clean"), "{}", &body[..200]);
        assert!(body.contains("Start") && body.contains("End"));
        lacks(body, &["<script", "<p>filler"]);
    }

    #[test]
    fn the_text_alternative_is_preferred_when_the_html_is_too_deep() {
        let page = page(
            format!(
                "Subject: Hi\r\nMIME-Version: 1.0\r\n\
                 Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
                 --b\r\nContent-Type: text/plain\r\n\r\nPlain <words> here\r\n\
                 --b\r\nContent-Type: text/html\r\n\r\n{}Html words{}\r\n--b--\r\n",
                "<div>".repeat(MAX_DEPTH + 10),
                "</div>".repeat(MAX_DEPTH + 10)
            )
            .as_bytes(),
        );
        assert!(
            body(&page).contains("Plain &lt;words&gt; here"),
            "{}",
            body(&page)
        );
        assert!(body(&page).contains("too deeply"));
    }
}
