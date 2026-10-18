# render-bench

A headless bench for issue #7 (how Missive should draw a message's HTML). It renders a folder of
`.eml` files with each candidate way to a PNG at a fixed width, measures time and memory, and
writes a contact sheet to compare the ways side by side and a grading file to fill in. It never
opens a window, never needs a display or a GPU, and never fetches anything.

It is a tool apart from the client: its own Cargo workspace, outside the main one, so that the
client's build and CI never carry a browser engine.

## The ways

| Way | What it is |
|---|---|
| `blitz` | The message's HTML drawn by [Blitz](https://github.com/DioxusLabs/blitz) (html5ever, Stylo, Taffy, Parley), painted on the CPU by `anyrender_vello_cpu`, light colour scheme. |
| `reduced-light` | The message reduced to simple content (paragraphs, headings, lists, quotes, links, the message's own images, tables only when they hold data), drawn by Blitz under a light system-like stylesheet. |
| `reduced-dark` | The same, under a dark stylesheet. |

The reduced form drops the author's CSS, layout tables, remote images (their alternative text
stays, in brackets), forms and scripts, and what the author's CSS hides (a preheader).

WebKitGTK is not a way here: it needs the WebKitGTK libraries and a headless display server
(or its offscreen mode) to run, none of which this bench assumes.

## Nothing is fetched

- No network crate is in the dependency tree: every Blitz crate is taken without its default
  features, so neither `blitz-net` nor an HTTP client is built. Check with
  `cargo tree -i blitz-net` (and `reqwest`, `ureq`, `hyper`, `tokio`): no match.
- Blitz asks for every resource through a provider that only answers `cid:` URLs (from the
  message's own parts) and `data:` URLs. Every other request (remote images, stylesheets, fonts,
  tracking pixels, frames) is refused and counted as a "remote load blocked".
- A test points the invented corpus's remote images, stylesheets, fonts and tracking pixel at a
  local listener and checks that it is never contacted.

## Build

```
cd tools/render-bench
cargo build --release
```

The binary is `target/release/render-bench`, alone. It needs `libfontconfig` (present on every
Linux desktop) to find the system fonts.

## Use

```
render-bench run <eml folder> <out folder> [--width <px>] [--timeout <seconds>]
render-bench summary <out folder>
```

`run` renders every `.eml` file of the folder, in name order, with each way (800 px wide, 20 s at
most per message and way by default) and writes into the out folder:

- `png/<number>-<way>.png`: the renders, cut at 12 000 px high.
- `sheet.html`: the contact sheet, to open in a browser from disk.
- `grading.csv`: `message,way,grade,note`, one row per message and way, to fill in with
  `faithful`, `readable` or `broken` and a note in words. A second run never overwrites it.
- `measures.csv`: per message and way, the outcome (`rendered`, `timed-out`, `crashed`,
  `unreadable`), the time in milliseconds, the peak memory in KiB, the height, whether it was cut,
  the remote loads blocked and the resources served from the message.
- `messages.csv`: which file each number is.
- `logs/`: what each render printed.

`summary` prints, per way, the outcomes, the median and maximum time and peak memory, and the
grades counted. It reads only `measures.csv`, `grading.csv` and `run.txt` and prints only numbers
and words: no file name, no note, nothing from a message, so it can be pasted in a public issue.
The out folder itself holds the messages' content: it stays on the machine that ran it.

## How it measures

Each message and way is rendered in a child process of the same binary. The time is the child's
wall-clock time from start to exit (parse, style, layout, paint and PNG encoding, plus a process
start of a few milliseconds); the peak memory is the child's own maximum resident set size,
read with `wait4`. A crash, a panic or a hang ends that child only: a hang past the timeout is
killed and recorded as `timed-out`, and the run goes on.

## The corpus

`corpus/` holds invented messages written for this bench (`example.org`, `example.com`): a table
layout with inline CSS, a newsletter with media queries and dark-mode metadata, an invoice, a
transactional message with a button, a reply with nested quotes, plain text, `cid:` and `data:`
images, remote images, tracking pixels, an ISO-8859-1 message and a broken one. No real mail.
