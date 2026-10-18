//! Bounds how deep a body nests before Blitz parses it. Blitz styles and lays a document out
//! recursively, and its parser (html5ever) looks through every open element for each new one: a
//! few kilobytes of `<div>` would overflow the stack or take minutes. This pass reads the tags
//! without building a tree and drops the start and end tags of elements opened deeper than a cap;
//! their text stays, in the nearest element kept.
//!
//! The count is an estimate (it does not rebuild the tree as html5ever does): the reduction checks
//! the depth of Blitz's own tree too.

use std::collections::HashMap;

/// Elements without content: they never nest.
const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "image", "img",
    "input", "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// Elements whose content is text up to their end tag: nothing in it nests.
const RAW: &[&str] = &[
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
    "script",
    "style",
    "textarea",
    "title",
    "xmp",
];

/// Whether a start tag closes the element open just before it, as these are commonly left open.
fn closes(new: &str, open: &str) -> bool {
    match new {
        "p" | "li" | "option" => open == new,
        "dt" | "dd" => matches!(open, "dt" | "dd"),
        "td" | "th" => matches!(open, "td" | "th"),
        "tr" => matches!(open, "tr" | "td" | "th"),
        _ => false,
    }
}

/// `html` with the tags of elements opened inside `cap` others removed.
pub fn flatten(html: &str, cap: usize) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    // The elements kept and open, and how many of each name: an end tag closes up to its own.
    let mut open: Vec<String> = Vec::new();
    let mut kept: HashMap<String, usize> = HashMap::new();
    // Elements removed and not closed yet, by name: their end tags are removed too.
    let mut dropped: HashMap<String, usize> = HashMap::new();
    let mut copied = 0;
    let mut at = 0;
    while let Some(lt) = find(bytes, at, b"<") {
        let rest = &bytes[lt + 1..];
        if rest.starts_with(b"!--") {
            at = find(bytes, lt + 4, b"-->").map_or(bytes.len(), |end| end + 3);
            continue;
        }
        if rest.starts_with(b"!") || rest.starts_with(b"?") {
            at = find(bytes, lt + 1, b">").map_or(bytes.len(), |end| end + 1);
            continue;
        }
        let end_tag = rest.starts_with(b"/");
        let name_start = lt + 1 + usize::from(end_tag);
        if !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            at = lt + 1;
            continue;
        }
        let name_end = bytes[name_start..]
            .iter()
            .position(|&b| b.is_ascii_whitespace() || b == b'/' || b == b'>')
            .map_or(bytes.len(), |end| name_start + end);
        let Some(close) = tag_end(bytes, name_end) else {
            break;
        };
        at = close;
        let name = html[name_start..name_end].to_ascii_lowercase();
        if end_tag {
            if let Some(count) = dropped.get_mut(&name)
                && *count > 0
            {
                *count -= 1;
                out.push_str(&html[copied..lt]);
                copied = close;
            } else if kept.get(&name).is_some_and(|&count| count > 0) {
                while let Some(top) = open.pop() {
                    *kept.get_mut(&top).unwrap() -= 1;
                    if top == name {
                        break;
                    }
                }
            }
            continue;
        }
        if VOID.contains(&name.as_str()) {
            continue;
        }
        if RAW.contains(&name.as_str()) {
            at = raw_end(bytes, close, &name).unwrap_or(bytes.len());
            continue;
        }
        while let Some(top) = open.last()
            && closes(&name, top)
        {
            *kept.get_mut(top).unwrap() -= 1;
            open.pop();
        }
        if open.len() >= cap {
            *dropped.entry(name).or_default() += 1;
            out.push_str(&html[copied..lt]);
            copied = close;
            continue;
        }
        *kept.entry(name.clone()).or_default() += 1;
        open.push(name);
    }
    out.push_str(&html[copied..]);
    out
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

/// Where a tag ends, after its `>`, past the quoted values of its attributes.
fn tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote = None;
    let mut after_equals = false;
    for (at, &b) in bytes.iter().enumerate().skip(from) {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'>' => return Some(at + 1),
            None if (b == b'"' || b == b'\'') && after_equals => quote = Some(b),
            None => {}
        }
        if !b.is_ascii_whitespace() {
            after_equals = b == b'=';
        }
    }
    None
}

/// Where the end tag of a raw text element starts.
fn raw_end(bytes: &[u8], from: usize, name: &str) -> Option<usize> {
    let mut at = from;
    while let Some(lt) = find(bytes, at, b"</") {
        let end = lt + 2 + name.len();
        if bytes
            .get(lt + 2..end)
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name.as_bytes()))
        {
            return Some(lt);
        }
        at = lt + 2;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_beyond_the_cap_are_removed_and_their_text_kept() {
        assert_eq!(
            flatten("<div>a<div>b<div>c</div>d</div>e</div>", 2),
            "<div>a<div>bcd</div>e</div>"
        );
    }

    #[test]
    fn what_is_left_open_commonly_does_not_count() {
        let html = "<p>a<p>b<ul><li>c<li>d</ul><table><tr><td>e<td>f<tr><td>g</table>";
        assert_eq!(flatten(html, 4), html);
    }

    #[test]
    fn a_tag_in_a_comment_a_script_or_a_quoted_value_does_not_count() {
        let html = "<!-- <div><div> --><script>if (a<b) { '<div><div>' }</script>\
                    <a title=\"<div><div>\" href='x>'>t</a>";
        assert_eq!(flatten(html, 1), html);
    }
}
