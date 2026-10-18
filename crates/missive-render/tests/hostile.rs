//! A hostile body must never crash or freeze the reader: whatever its nesting, the reduction ends
//! quickly, keeps the text, and gives a shallow document.

use std::time::{Duration, Instant};

use missive_render::reduce::{MAX_DEPTH, MAX_HTML, Refused, Theme, reduce};

const BUDGET: Duration = Duration::from_secs(2);

/// The reduction of `html`, checked to end within the budget (and a margin for a loaded machine).
fn reduced(html: &str) -> Result<String, Refused> {
    let start = Instant::now();
    let out = reduce(html, Theme::Light, 800, BUDGET);
    assert!(start.elapsed() < BUDGET * 2, "took {:?}", start.elapsed());
    out
}

/// How deep the elements of the reduced body nest.
fn depth(reduced: &str) -> usize {
    let body = &reduced[reduced.find("<body>").unwrap() + "<body>".len()..];
    let (mut depth, mut deepest) = (0usize, 0usize);
    for tag in body.split('<').skip(1) {
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if matches!(name.as_str(), "br" | "hr" | "img") {
            continue;
        }
        if tag.starts_with('/') {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
            deepest = deepest.max(depth);
        }
    }
    deepest
}

/// `count` nested `tag`s, each holding a word of its own: `w0` to `w{count - 1}`.
fn nested(tag: &str, count: usize) -> String {
    (0..count).map(|i| format!("<{tag}>w{i} ")).collect()
}

fn assert_kept(out: &str, count: usize) {
    for i in [0, count / 2, count - 1] {
        assert!(out.contains(&format!("w{i} ")), "w{i} is missing");
    }
    assert!(depth(out) <= MAX_DEPTH, "nests {} deep", depth(out));
}

#[test]
fn five_thousand_nested_divs_are_flattened_with_their_text() {
    let out = reduced(&nested("div", 5_000)).unwrap();
    assert_kept(&out, 5_000);
}

#[test]
fn three_thousand_nested_quotes_are_flattened_with_their_text() {
    let out = reduced(&nested("blockquote", 3_000)).unwrap();
    assert_kept(&out, 3_000);
}

#[test]
fn twenty_thousand_nested_bold_runs_are_flattened_with_their_text() {
    let out = reduced(&nested("b", 20_000)).unwrap();
    assert_kept(&out, 20_000);
}

#[test]
fn two_hundred_thousand_nested_divs_end_quickly_with_their_text() {
    // A megabyte: a word at every level would pass the size cap.
    let html = format!("first {}last", "<div>".repeat(200_000));
    let out = reduced(&html).unwrap();
    assert!(out.contains("first") && out.contains("last"), "{out}");
    assert!(depth(&out) <= MAX_DEPTH, "nests {} deep", depth(&out));
}

#[test]
fn nesting_that_slips_past_the_count_is_refused_not_crashed() {
    // In SVG every element nests, where a `tr` would close the one before it in HTML.
    let html = format!("<svg>{}</svg>", "<tr>".repeat(5_000));
    assert_eq!(reduced(&html), Err(Refused::TooDeep));
}

#[test]
fn a_body_larger_than_the_cap_is_refused() {
    assert_eq!(reduced(&"x".repeat(MAX_HTML + 1)), Err(Refused::TooLarge));
}

#[test]
fn a_reduction_over_its_budget_is_refused() {
    let html = "<p>A paragraph of an ordinary message.</p>".repeat(20_000);
    let out = reduce(&html, Theme::Light, 800, Duration::from_millis(1));
    assert_eq!(out, Err(Refused::TooLong));
}

#[test]
fn ordinary_nesting_is_kept_as_it_is() {
    let out =
        reduced("<blockquote><blockquote><p>Quoted <b>twice</b></p></blockquote></blockquote>")
            .unwrap();
    assert!(
        out.contains(
            "<blockquote><blockquote><p>Quoted <b>twice</b></p></blockquote></blockquote>"
        ),
        "{out}"
    );
}

#[test]
fn the_deepest_tree_allowed_is_reduced_without_overflow() {
    // Each table brings a row group the count before parsing does not see: about 130 deep.
    let tables = format!("{}cell", "<table><tr><td>".repeat(32));
    assert!(reduced(&tables).unwrap().contains("cell"));
    // Just under the tree's limit, in SVG, where nothing closes what came before.
    let svg = format!("<svg>{}</svg>", "<tr>".repeat(155));
    assert!(reduced(&svg).is_ok());
}
