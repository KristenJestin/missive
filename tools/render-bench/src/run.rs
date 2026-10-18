//! `render-bench run`: every message, every way, each in a child process of this binary.
//!
//! A child per message and way measures that render alone (its peak memory comes from `wait4`),
//! and a crash, an abort or a hang ends that child, never the run.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::WAYS;

pub struct Options {
    pub width: u32,
    pub timeout: Duration,
}

/// The line a child prints last when it has drawn its PNG.
pub const DRAWN_PREFIX: &str = "render-bench drawn:";

/// A child exits with this code when the message cannot be read: the message failed, not the way.
pub const UNREADABLE_EXIT: i32 = 3;

struct Measure {
    outcome: &'static str,
    millis: u128,
    peak_kib: i64,
    drawn: Option<String>,
}

pub fn run(emls: &Path, out: &Path, options: &Options) -> Result<(), String> {
    let mut files: Vec<PathBuf> = fs::read_dir(emls)
        .map_err(|error| format!("cannot read {}: {error}", emls.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("eml"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no .eml file in {}", emls.display()));
    }
    let png_dir = out.join("png");
    let log_dir = out.join("logs");
    for dir in [&png_dir, &log_dir] {
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    }
    let exe = std::env::current_exe().map_err(|error| format!("cannot find myself: {error}"))?;

    let mut messages = String::from("message,file\n");
    let mut measures =
        String::from("message,way,outcome,millis,peak_kib,height,clipped,blocked,embedded\n");
    let mut rows = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let number = format!("{:03}", index + 1);
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        writeln!(messages, "{number},{}", csv_field(&name)).unwrap();
        let mut cells = Vec::new();
        for way in WAYS {
            let png = png_dir.join(format!("{number}-{way}.png"));
            let log = log_dir.join(format!("{number}-{way}.log"));
            // A PNG left by an earlier run must not pass for this one's.
            let _ = fs::remove_file(&png);
            let measure = render_in_child(&exe, way, file, &png, &log, options)?;
            let drawn = parse_drawn(measure.drawn.as_deref());
            writeln!(
                measures,
                "{number},{way},{},{},{},{},{},{},{}",
                measure.outcome,
                measure.millis,
                measure.peak_kib,
                drawn.height,
                drawn.clipped,
                drawn.blocked,
                drawn.embedded
            )
            .unwrap();
            eprintln!(
                "{number} {way:<13} {:<9} {:>6} ms {:>7} KiB",
                measure.outcome, measure.millis, measure.peak_kib
            );
            cells.push((way, measure, drawn));
        }
        rows.push((number, name, cells));
    }

    write(&out.join("messages.csv"), &messages)?;
    write(&out.join("measures.csv"), &measures)?;
    write(
        &out.join("run.txt"),
        &format!(
            "width={}\ntimeout_s={}\n",
            options.width,
            options.timeout.as_secs_f64()
        ),
    )?;
    // The grading is the maintainer's work: a second run never overwrites it.
    let grading = out.join("grading.csv");
    if !grading.exists() {
        let mut csv = String::from("message,way,grade,note\n");
        for (number, _, _) in &rows {
            for way in WAYS {
                writeln!(csv, "{number},{way},,").unwrap();
            }
        }
        write(&grading, &csv)?;
    }
    write(&out.join("sheet.html"), &sheet(&rows, options))?;
    eprintln!(
        "{} messages, {} ways: open {} in a browser, grade in {}",
        rows.len(),
        WAYS.len(),
        out.join("sheet.html").display(),
        grading.display()
    );
    Ok(())
}

fn render_in_child(
    exe: &Path,
    way: &str,
    eml: &Path,
    png: &Path,
    log: &Path,
    options: &Options,
) -> Result<Measure, String> {
    let log_file = File::create(log).map_err(|error| format!("cannot create the log: {error}"))?;
    let log_err = log_file
        .try_clone()
        .map_err(|error| format!("cannot create the log: {error}"))?;
    let start = Instant::now();
    let child = Command::new(exe)
        .arg("render")
        .arg(way)
        .arg(eml)
        .arg(png)
        .arg("--width")
        .arg(options.width.to_string())
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .map_err(|error| format!("cannot start a renderer: {error}"))?;
    let pid = child.id() as libc::pid_t;

    let (stop, stopped) = mpsc::channel::<()>();
    let timeout = options.timeout;
    let watchdog = thread::spawn(move || {
        let late = stopped.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout);
        if late {
            // SAFETY: the child is not reaped before this thread is joined, so pid is still ours.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        late
    });
    // Wait for the end without reaping the child (WNOWAIT): the watchdog may still signal it.
    loop {
        // SAFETY: siginfo_t is plain data, zeroes are a valid value; pid is our own child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if waited == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("cannot wait for a renderer: {error}"));
        }
    }
    let millis = start.elapsed().as_millis();
    drop(stop);
    let timed_out = watchdog.join().unwrap_or(false);

    let mut status = 0;
    // SAFETY: rusage is plain data, zeroes are a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: pid is our own child, ended and not reaped yet; status and usage are valid pointers.
    if unsafe { libc::wait4(pid, &mut status, 0, &mut usage) } != pid {
        return Err(format!(
            "cannot reap a renderer: {}",
            std::io::Error::last_os_error()
        ));
    }
    // The child is reaped: `child` must not wait for it again, and dropping it does not.
    drop(child);

    let exited = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
    let drawn = fs::read_to_string(log).ok().and_then(|text| {
        text.lines().rev().find_map(|line| {
            line.strip_prefix(DRAWN_PREFIX)
                .map(str::trim)
                .map(String::from)
        })
    });
    let outcome = if timed_out {
        "timed-out"
    } else if exited == Some(0) && drawn.is_some() {
        "rendered"
    } else if exited == Some(UNREADABLE_EXIT) {
        "unreadable"
    } else {
        "crashed"
    };
    Ok(Measure {
        outcome,
        millis,
        // ru_maxrss is in kibibytes on Linux.
        peak_kib: usage.ru_maxrss,
        drawn,
    })
}

/// A message on the sheet: its number, its file name, and each way's measure.
type Row = (String, String, Vec<(&'static str, Measure, Drawn)>);

#[derive(Default)]
struct Drawn {
    height: u32,
    clipped: bool,
    blocked: usize,
    embedded: usize,
}

fn parse_drawn(line: Option<&str>) -> Drawn {
    let mut drawn = Drawn::default();
    for pair in line.unwrap_or("").split_whitespace() {
        match pair.split_once('=') {
            Some(("height", value)) => drawn.height = value.parse().unwrap_or(0),
            Some(("clipped", value)) => drawn.clipped = value == "true",
            Some(("blocked", value)) => drawn.blocked = value.parse().unwrap_or(0),
            Some(("embedded", value)) => drawn.embedded = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    drawn
}

fn sheet(rows: &[Row], options: &Options) -> String {
    let mut html = String::new();
    html.push_str(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>render-bench</title><style>\n\
         body { font-family: sans-serif; margin: 16px; }\n\
         table { border-collapse: collapse; }\n\
         th, td { border: 1px solid #999; padding: 6px; vertical-align: top; }\n\
         .shot { width: 400px; max-height: 900px; overflow: auto; display: block; }\n\
         .shot img { width: 400px; display: block; }\n\
         .facts { font-size: 12px; color: #444; }\n\
         .failed { color: #a00; font-weight: bold; }\n\
         </style></head><body>\n<h1>render-bench</h1>\n",
    );
    writeln!(
        html,
        "<p>Width {} px, timeout {} s. Each image opens at full size. Grade every message and way \
         in <code>grading.csv</code>: <b>faithful</b> (looks as its author meant), <b>readable</b> \
         (everything is there and usable, but looks different), <b>broken</b> (something is \
         missing, unreadable or wrong), and a note in words.</p>",
        options.width,
        options.timeout.as_secs_f64()
    )
    .unwrap();
    html.push_str("<table><tr><th>Message</th>");
    for way in WAYS {
        write!(html, "<th>{way}</th>").unwrap();
    }
    html.push_str("</tr>\n");
    for (number, name, cells) in rows {
        write!(html, "<tr><td><b>{number}</b><br>{}</td>", escape(name)).unwrap();
        for (way, measure, drawn) in cells {
            html.push_str("<td>");
            if measure.outcome == "rendered" {
                write!(
                    html,
                    "<a class=\"shot\" href=\"png/{number}-{way}.png\"><img loading=\"lazy\" src=\"png/{number}-{way}.png\"></a>"
                )
                .unwrap();
            } else {
                write!(html, "<p class=\"failed\">{}</p>", measure.outcome).unwrap();
            }
            write!(
                html,
                "<p class=\"facts\">{} ms, {:.1} MiB peak, {} px high{}, {} remote loads blocked, {} embedded</p></td>",
                measure.millis,
                measure.peak_kib as f64 / 1024.0,
                drawn.height,
                if drawn.clipped { " (cut)" } else { "" },
                drawn.blocked,
                drawn.embedded
            )
            .unwrap();
        }
        html.push_str("</tr>\n");
    }
    html.push_str("</table></body></html>\n");
    html
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}
