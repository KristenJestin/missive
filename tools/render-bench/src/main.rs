//! render-bench: renders a folder of `.eml` files with each way Missive could draw mail, headless,
//! and measures them, for issue #7. See README.md.

mod net;
mod render;
mod run;
mod summary;

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use blitz_traits::shell::ColorScheme;
use missive_render::{message, reduce};

/// The ways, in the order of the contact sheet's columns.
pub const WAYS: [&str; 3] = ["blitz", "reduced-light", "reduced-dark"];

const USAGE: &str = "\
usage:
  render-bench run <eml folder> <out folder> [--width <px>] [--timeout <seconds>]
  render-bench summary <out folder>

run      renders every .eml file of the folder with each way (blitz, reduced-light,
         reduced-dark) into <out folder>: png/, sheet.html (open it in a browser),
         grading.csv (fill it in), measures.csv. Width 800 px, timeout 20 s by default.
summary  prints counts and medians from measures.csv and grading.csv, without any
         content from the messages: it can be pasted in a public issue.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["run", emls, out, options @ ..] => parse_options(options).and_then(|(width, timeout)| {
            run::run(
                Path::new(emls),
                Path::new(out),
                &run::Options { width, timeout },
            )
        }),
        ["summary", out] => summary::summary(Path::new(out)).map(|text| print!("{text}")),
        // One message, one way: what `run` starts a child process for.
        ["render", way, eml, png, options @ ..] => {
            return match parse_options(options) {
                Ok((width, _)) => render_one(way, Path::new(eml), Path::new(png), width),
                Err(error) => {
                    eprintln!("render-bench: {error}");
                    ExitCode::from(2)
                }
            };
        }
        ["--help" | "-h" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("render-bench: {error}");
            ExitCode::from(2)
        }
    }
}

fn parse_options(options: &[&str]) -> Result<(u32, Duration), String> {
    let mut width = 800;
    let mut timeout = Duration::from_secs(20);
    let mut options = options.iter();
    while let Some(&option) = options.next() {
        let value = options.next().ok_or(format!("{option} needs a value"))?;
        match option {
            "--width" => {
                width = value
                    .parse()
                    .ok()
                    .filter(|&w| (100..=4000).contains(&w))
                    .ok_or("--width is a number of pixels, 100 to 4000")?;
            }
            "--timeout" => {
                timeout = value
                    .parse::<f64>()
                    .ok()
                    .filter(|&s| s > 0.0 && s < 86_400.0)
                    .map(Duration::from_secs_f64)
                    .ok_or("--timeout is a number of seconds")?;
            }
            _ => return Err(format!("unknown option {option}\n{USAGE}")),
        }
    }
    Ok((width, timeout))
}

fn render_one(way: &str, eml: &Path, png: &Path, width: u32) -> ExitCode {
    let message = match std::fs::read(eml)
        .map_err(|error| format!("cannot read the file: {error}"))
        .and_then(|raw| message::read(&raw))
    {
        Ok(message) => message,
        Err(error) => {
            eprintln!("render-bench: unreadable: {error}");
            return ExitCode::from(run::UNREADABLE_EXIT as u8);
        }
    };
    let (html, scheme) = match way {
        "blitz" => (message.html, ColorScheme::Light),
        "reduced-light" => (
            reduce::reduce(&message.html, &reduce::Theme::Light, width),
            ColorScheme::Light,
        ),
        "reduced-dark" => (
            reduce::reduce(&message.html, &reduce::Theme::Dark, width),
            ColorScheme::Dark,
        ),
        _ => {
            eprintln!("render-bench: unknown way {way}");
            return ExitCode::from(2);
        }
    };
    match render::draw(&html, message.parts, width, scheme, png) {
        Ok(drawn) => {
            println!(
                "{} height={} clipped={} blocked={} embedded={}",
                run::DRAWN_PREFIX,
                drawn.height,
                drawn.clipped,
                drawn.blocked,
                drawn.embedded
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("render-bench: {error}");
            ExitCode::from(2)
        }
    }
}
