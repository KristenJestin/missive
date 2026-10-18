//! The harness end to end, through its binary, on the invented corpus.

use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const WAYS: [&str; 3] = ["blitz", "reduced-light", "reduced-dark"];

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// Runs the bench as the maintainer will: no display of any kind.
fn bench(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_render-bench"))
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}

fn run(emls: &Path, out: &Path, extra: &[&str]) {
    let mut args = vec!["run", emls.to_str().unwrap(), out.to_str().unwrap()];
    args.extend_from_slice(extra);
    let output = bench(&args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `measures.csv` as (message, way) → its fields.
fn measures(out: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(out.join("measures.csv"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| line.split(',').map(String::from).collect())
        .collect()
}

fn outcome(rows: &[Vec<String>], message: &str, way: &str) -> String {
    rows.iter()
        .find(|row| row[0] == message && row[1] == way)
        .map(|row| row[2].clone())
        .unwrap_or_else(|| panic!("no row for {message} {way}"))
}

#[test]
fn the_harness_runs_headless_and_writes_pngs_sheet_and_measures() {
    let out = tempfile::tempdir().unwrap();
    run(&corpus(), out.path(), &[]);

    let count = fs::read_dir(corpus()).unwrap().count();
    let rows = measures(out.path());
    assert_eq!(rows.len(), count * WAYS.len());
    for row in &rows {
        let (message, way, outcome) = (&row[0], &row[1], &row[2]);
        let png = out.path().join("png").join(format!("{message}-{way}.png"));
        if outcome == "rendered" {
            let bytes = fs::read(&png).unwrap();
            assert!(bytes.starts_with(b"\x89PNG"), "{}", png.display());
            assert!(
                row[4].parse::<u64>().unwrap() > 0,
                "no peak memory: {row:?}"
            );
        } else {
            assert!(!png.exists());
        }
    }
    let rendered = rows.iter().filter(|row| row[2] == "rendered").count();
    // Every invented message but the broken one renders with every way.
    assert_eq!(rendered, (count - 1) * WAYS.len());

    let sheet = fs::read_to_string(out.path().join("sheet.html")).unwrap();
    assert!(sheet.contains("png/001-blitz.png"));
    assert!(sheet.contains("png/001-reduced-dark.png"));
    let grading = fs::read_to_string(out.path().join("grading.csv")).unwrap();
    assert!(grading.starts_with("message,way,grade,note\n"));
    assert_eq!(grading.lines().count(), 1 + count * WAYS.len());

    let summary = bench(&["summary", out.path().to_str().unwrap()]);
    assert!(summary.status.success());
    let summary = String::from_utf8(summary.stdout).unwrap();
    assert!(summary.contains(&format!("{count} messages")), "{summary}");
}

#[test]
fn nothing_is_fetched_for_remote_images_stylesheets_fonts_or_the_tracking_pixel() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let local = listener.local_addr().unwrap().to_string();

    // The corpus's remote hosts, pointed at the listener.
    let emls = tempfile::tempdir().unwrap();
    for entry in fs::read_dir(corpus()).unwrap() {
        let path = entry.unwrap().path();
        let mut raw = fs::read(&path).unwrap();
        for host in ["images.example.org", "track.example.org", "cdn.example.org"] {
            raw = replace(&raw, host.as_bytes(), local.as_bytes());
        }
        fs::write(emls.path().join(path.file_name().unwrap()), raw).unwrap();
    }
    let remote = fs::read_to_string(emls.path().join("09-tracking-pixel.eml")).unwrap();
    assert!(remote.contains(&format!("https://{local}/open.gif")));

    let out = tempfile::tempdir().unwrap();
    run(emls.path(), out.path(), &[]);

    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("the listener was contacted: {other:?}"),
    }
    // The remote URLs were seen and refused, not missed.
    let rows = measures(out.path());
    for message in ["002", "008", "009"] {
        let row = rows
            .iter()
            .find(|row| row[0] == message && row[1] == "blitz")
            .unwrap();
        assert!(
            row[7].parse::<u32>().unwrap() > 0,
            "nothing blocked: {row:?}"
        );
    }
}

#[test]
fn a_malformed_message_does_not_stop_the_run() {
    let emls = tempfile::tempdir().unwrap();
    for name in ["11-broken.eml", "03-invoice.eml"] {
        fs::copy(corpus().join(name), emls.path().join(name)).unwrap();
    }
    fs::write(
        emls.path().join("00-not-mail.eml"),
        [0u8, 159, 146, 150, 0, 7],
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    run(emls.path(), out.path(), &[]);

    let rows = measures(out.path());
    assert_eq!(outcome(&rows, "001", "blitz"), "unreadable");
    assert_eq!(outcome(&rows, "003", "blitz"), "unreadable");
    for way in WAYS {
        assert_eq!(outcome(&rows, "002", way), "rendered");
    }
}

#[test]
fn a_message_that_times_out_does_not_stop_the_run() {
    let emls = tempfile::tempdir().unwrap();
    let mut huge = String::from(
        "From: a@example.org\r\nTo: b@example.org\r\nSubject: huge\r\n\
         Content-Type: text/html; charset=utf-8\r\n\r\n<table>",
    );
    for row in 0..20_000 {
        huge.push_str(&format!(
            "<tr><td>{row}</td><td>cell</td><td><b>bold</b> cell</td></tr>\r\n"
        ));
    }
    huge.push_str("</table>\r\n");
    fs::write(emls.path().join("01-huge.eml"), huge).unwrap();
    fs::copy(
        corpus().join("03-invoice.eml"),
        emls.path().join("02-invoice.eml"),
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    run(emls.path(), out.path(), &["--timeout", "1.5"]);

    let rows = measures(out.path());
    assert_eq!(outcome(&rows, "001", "blitz"), "timed-out");
    for way in WAYS {
        assert_eq!(outcome(&rows, "002", way), "rendered");
    }
}

fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while !rest.is_empty() {
        if rest.starts_with(from) {
            out.extend_from_slice(to);
            rest = &rest[from.len()..];
        } else {
            out.push(rest[0]);
            rest = &rest[1..];
        }
    }
    out
}
