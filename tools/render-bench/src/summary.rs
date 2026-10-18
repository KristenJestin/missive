//! `render-bench summary`: counts and medians per way, safe to paste in a public issue.
//!
//! It reads only `measures.csv`, `grading.csv` and `run.txt`, and prints only numbers, the
//! names of the ways and the grading words: never a file name, a note or anything from a message.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::WAYS;

const GRADES: [&str; 3] = ["faithful", "readable", "broken"];
const OUTCOMES: [&str; 4] = ["rendered", "timed-out", "crashed", "unreadable"];

#[derive(Default)]
struct Way {
    outcomes: BTreeMap<String, usize>,
    millis: Vec<f64>,
    peak_mib: Vec<f64>,
    clipped: usize,
    with_blocked: usize,
    grades: BTreeMap<String, usize>,
}

pub fn summary(out: &Path) -> Result<String, String> {
    let read = |name: &str| {
        fs::read_to_string(out.join(name)).map_err(|error| format!("cannot read {name}: {error}"))
    };
    let measures = parse_csv(&read("measures.csv")?);
    let grading = parse_csv(&read("grading.csv")?);
    let run = read("run.txt").unwrap_or_default();

    let mut ways: BTreeMap<&str, Way> = WAYS.iter().map(|&way| (way, Way::default())).collect();
    let mut messages = std::collections::BTreeSet::new();
    let header = measures.first().cloned().unwrap_or_default();
    let column = |name: &str| header.iter().position(|field| field == name);
    let (Some(c_message), Some(c_way), Some(c_outcome), Some(c_millis), Some(c_peak)) = (
        column("message"),
        column("way"),
        column("outcome"),
        column("millis"),
        column("peak_kib"),
    ) else {
        return Err("measures.csv has not the expected columns".into());
    };
    let c_clipped = column("clipped");
    let c_blocked = column("blocked");
    for row in measures.iter().skip(1) {
        let Some(way) = row.get(c_way).and_then(|way| ways.get_mut(way.as_str())) else {
            continue;
        };
        if let Some(message) = row.get(c_message) {
            messages.insert(message.clone());
        }
        let outcome = row.get(c_outcome).cloned().unwrap_or_default();
        *way.outcomes.entry(outcome.clone()).or_default() += 1;
        if outcome == "rendered" {
            if let Some(millis) = row.get(c_millis).and_then(|v| v.parse().ok()) {
                way.millis.push(millis);
            }
            if let Some(kib) = row.get(c_peak).and_then(|v| v.parse::<f64>().ok()) {
                way.peak_mib.push(kib / 1024.0);
            }
            if c_clipped
                .and_then(|c| row.get(c))
                .is_some_and(|v| v == "true")
            {
                way.clipped += 1;
            }
            if c_blocked
                .and_then(|c| row.get(c))
                .and_then(|v| v.parse::<usize>().ok())
                .is_some_and(|n| n > 0)
            {
                way.with_blocked += 1;
            }
        }
    }
    for row in grading.iter().skip(1) {
        let (Some(way), Some(grade)) = (row.get(1), row.get(2)) else {
            continue;
        };
        let Some(way) = ways.get_mut(way.trim()) else {
            continue;
        };
        let grade = grade.trim().to_ascii_lowercase();
        let grade = if grade.is_empty() {
            "ungraded"
        } else if let Some(known) = GRADES.iter().find(|&&known| known == grade) {
            known
        } else {
            "unknown grade"
        };
        *way.grades.entry(grade.to_string()).or_default() += 1;
    }

    let mut text = String::new();
    let setting = |key: &str| {
        run.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or("?")
            .to_string()
    };
    writeln!(
        text,
        "render-bench: {} messages, width {} px, timeout {} s\n",
        messages.len(),
        setting("width"),
        setting("timeout_s")
    )
    .unwrap();
    text.push_str("| way | rendered | timed out | crashed | unreadable | median ms | max ms | median peak MiB | max peak MiB | cut at max height | with remote loads blocked |\n");
    text.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    for (name, way) in &ways {
        let count = |outcome: &str| way.outcomes.get(outcome).copied().unwrap_or(0);
        let others: usize = way
            .outcomes
            .iter()
            .filter(|(outcome, _)| !OUTCOMES.contains(&outcome.as_str()))
            .map(|(_, n)| n)
            .sum();
        writeln!(
            text,
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            count("rendered"),
            count("timed-out"),
            count("crashed") + others,
            count("unreadable"),
            number(median(&way.millis), 0),
            number(max(&way.millis), 0),
            number(median(&way.peak_mib), 1),
            number(max(&way.peak_mib), 1),
            way.clipped,
            way.with_blocked
        )
        .unwrap();
    }
    text.push_str("\n| way | faithful | readable | broken | ungraded | unknown grade |\n");
    text.push_str("|---|---|---|---|---|---|\n");
    for (name, way) in &ways {
        let count = |grade: &str| way.grades.get(grade).copied().unwrap_or(0);
        writeln!(
            text,
            "| {name} | {} | {} | {} | {} | {} |",
            count("faithful"),
            count("readable"),
            count("broken"),
            count("ungraded"),
            count("unknown grade")
        )
        .unwrap();
    }
    Ok(text)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    })
}

fn max(values: &[f64]) -> Option<f64> {
    values.iter().copied().max_by(f64::total_cmp)
}

fn number(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:.decimals$}"))
}

/// RFC 4180, enough for a file the maintainer may have saved from a spreadsheet: quoted fields,
/// doubled quotes, commas and line breaks inside quotes.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (true, _) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, _) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_field_keeps_its_commas_quotes_and_lines() {
        let rows = parse_csv("a,b\n1,\"x, \"\"y\"\"\nz\"\n");
        assert_eq!(rows[1], vec!["1", "x, \"y\"\nz"]);
    }

    #[test]
    fn the_median_of_an_even_count_is_the_mean_of_the_middle_two() {
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn the_summary_prints_counts_and_never_a_note_or_a_file_name() {
        let out = tempfile::tempdir().unwrap();
        fs::write(
            out.path().join("measures.csv"),
            "message,way,outcome,millis,peak_kib,height,clipped,blocked,embedded\n\
             001,blitz,rendered,100,20480,900,false,2,0\n\
             002,blitz,rendered,300,40960,900,true,0,0\n\
             003,blitz,timed-out,20000,90000,0,false,0,0\n",
        )
        .unwrap();
        fs::write(
            out.path().join("grading.csv"),
            "message,way,grade,note\n\
             001,blitz,Faithful,\"Invoice from shop.example.org, total wrong\"\n\
             002,blitz,broken,the secret subject\n\
             003,blitz,,\n",
        )
        .unwrap();
        fs::write(
            out.path().join("messages.csv"),
            "message,file\n001,secret-name.eml\n",
        )
        .unwrap();
        fs::write(out.path().join("run.txt"), "width=800\ntimeout_s=20\n").unwrap();

        let text = summary(out.path()).unwrap();

        assert!(
            text.contains("| blitz | 2 | 1 | 0 | 0 | 200 | 300 | 30.0 | 40.0 | 1 | 1 |"),
            "{text}"
        );
        assert!(text.contains("| blitz | 1 | 0 | 1 | 1 | 0 |"), "{text}");
        assert!(text.contains("3 messages, width 800 px"), "{text}");
        for private in ["example.org", "secret", "Invoice", ".eml"] {
            assert!(!text.contains(private), "{private} leaked: {text}");
        }
    }
}
