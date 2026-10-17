# AGENTS.md — Missive

Instructions for any coding agent working in this repository. `CLAUDE.md` is a symbolic link
to this file: there is one contract, not two that drift apart.

## Project overview

Missive is a mail client for Linux (Wayland), written in Rust. It is built like a set of tools
first: other programs (a desktop shell, a file manager, a script) drive it and follow it through
a documented command line and event stream, and its window is one client among them.

- **A daemon** does the work: it keeps a local copy of each account in sync over IMAP, sends
  over SMTP, indexes mail for search, and answers on a Unix socket. It is the only process that
  talks to mail servers.
- **A local copy** (SQLite, with full-text search) makes reading and search instant and works
  offline. Synchronisation never loses mail: a message is only ever removed locally after the
  server says it is gone.
- **The command line** (`missive …`) drives everything: accounts, status, unread counts, search,
  sync, compose, `mailto:` addresses. Its `--json` outputs and the `watch` event stream (one JSON
  object per line) are a contract read by other programs.
- **A window** (to come) is one more client of the same daemon and the same local copy.
- **Accounts are generic**: IMAP and SMTP with a password or OAuth. A local bridge (for example
  Proton Mail Bridge, which serves IMAP and SMTP on localhost with its own self-signed
  certificate) is an ordinary account with an imported certificate.

Missive is used alone, on any Linux desktop. Nothing in it is specific to one machine or one
desktop: links open with the system's default handler (the XDG portal, else `xdg-open`, with the
environment Missive received), colours follow the system, nothing assumes a given browser,
shell or theme. A desktop integrates Missive from its own repository, through the command line
and the events; nothing of that desktop lives here.

## Commands

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
tools/commit-message.sh --range origin/dev..HEAD
git config core.hooksPath .githooks     # once per clone: the commit-msg and pre-commit hooks
```

## Principles

### 1. Think before coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them; don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If the code and the issue disagree, the issue wins; if the issue is wrong, say so instead of
  quietly deviating.

### 2. Simplicity first

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what the task asks. No abstractions for single-use code.
- No configurability that was not requested.
- No error handling for impossible scenarios. If 200 lines could be 50, rewrite.

### 3. Surgical changes

**Touch only what you must. Clean up only your own mess.**

- Don't "improve" adjacent code, comments or formatting. Don't refactor what isn't broken.
- Match existing style. If you notice unrelated dead code, mention it, don't delete it.
- Every changed line must trace directly to the task.

### 4. The failing test first

**Define success criteria. Loop until verified.**

- Write the test that fails first, see it fail, then make it pass.
- "Fix the bug" → a test that reproduces it, then the fix.
- Every scenario of the issue gets a test named after it. A scenario without a test is a
  defect, not a TODO.

## Code style

- Rust 2024, `cargo fmt`, `cargo clippy -- -D warnings` clean.
- Code, identifiers, comments, docs, issues and commits are in English.
- Building blocks: `async-imap` (IMAP), `mail-parser` (reading messages), `mail-builder`
  (writing them), `lettre` (SMTP), SQLite with FTS5 (local copy and search), `zbus` for D-Bus and
  the Secret Service (pure Rust, no libdbus). Adding another dependency of this weight is said in
  the pull request.
- A comment says why, not what.

## Secrets (non-negotiable)

- Passwords, app passwords and OAuth tokens live only in the keyring (Secret Service). Never in
  a file, a log, a message, a `Display`, a `Debug`, a panic, a socket answer or an output.
- A password typed by the user is read without echo or from standard input, never from the
  command line, which every process can read.
- Never look for credentials outside Missive's own login (no reading of other applications'
  configuration or keyring entries).
- Mail content is private: never log a message body, a subject or an address. Logs say what
  happened (counts, folders by role, durations, error kinds), not what the mail says.

## Mail safety

- **Never lose or alter mail on the server** by mistake. Destructive actions (delete, expunge,
  move) happen only when the user asks for them, and are tested against the fake server.
- **Rendering never runs code and never phones home**: no script, no remote resource fetched
  unless the user allowed it for that message or sender. Tracking pixels stay blocked.
- A certificate is trusted only from the system store or an explicit import by the user for that
  account; never "accept invalid certificates" globally.

## Connection states

- Each account is in one of: `no-login`, `auth-refused`, `unreachable`, `syncing`, `ready`;
  always told apart, never a silent error. `unreachable` never asks to log in again.
- `search`, `unread` and `status` read the local copy: they touch neither the network nor the
  keyring.

## Testing

- **Tests never touch a real mail server, the real session bus, the real keyring or the real
  configuration.** They talk to a fake local IMAP/SMTP server, run their own private
  `dbus-daemon` when they need one, and use temporary folders.
- For a manual try, use `dbus-run-session -- <command>` and a temporary configuration; stop every
  test daemon you started.
- What only a real account can show (a real mailbox, a real bridge) is checked by hand, read
  only unless the issue says otherwise, and reported in the pull request.
- A test that fails only under load is rerun alone before the failure is called real.

## How the work is organised

| Who | What |
|---|---|
| The maintainer | decides what is built, tries each tranche on their machine, and merges what they have not delegated |
| The lead agent | turns the maintainer's decisions into issues, launches the developing agents, reviews their pull requests, keeps the tracker honest, and never hides a failure |
| Developing agents | implement one issue each: the failing test first, then the code, the verification and a pull request; they never merge |
| CI | `verify`, `commit-messages` and `shape` on every pull request |

### The tracker

- **Issues are the tickets, and a ticket stands alone.** It holds the what, the where, the "how
  to verify" and the base branch, with no local path. If it lacks one of them, ask rather than
  guess.
- The plan is cut into **tranches**, labels `tranche:1`, `tranche:2`…, beside `type:*` and
  `area:*`. A tranche ends on something the maintainer can use, and they accept it on their
  machine.
- **`dev` is the integration branch** and the default branch: a branch starts from
  `origin/dev` and its pull request targets it. `main` holds released versions.

## Git and pull requests (non-negotiable)

- **Never commit, merge, rebase, push or force-push on `main` or `dev` directly.** If you are
  on one of these branches, create a branch first (`feat/<n>-<topic>`, `fix/<n>-<topic>`). The
  `pre-commit` hook refuses it.
- Never rewrite published history. No `--no-verify`.
- One commit = one intent. No `wip` commits. Don't mix formatting and logic in one commit.
- Commits use the Git user configured on the machine.
- The subject is `<type>(<scope>): <subject>`: the **scope is required**, the subject is 72
  characters at most, starts lowercase, has no final period, and the type is one of feat, fix,
  refactor, test, docs, chore, build, ci, perf. `tools/commit-message.sh` is the judge (the
  `commit-msg` hook and CI run it).
- Scopes: `core`, `imap`, `smtp`, `store`, `search`, `sync`, `daemon`, `cli`, `render`, `gui`,
  `repo`, `ci`.
- **Pull requests are merged by squash only.** The title is a plain Angular subject. The
  description is written in the first person as the maintainer and ends with `Closes #<n>`.

## Security and privacy

- **This repository is public.** Nothing private goes into it, an issue, a pull request or a
  commit: no address, account name, mailbox content, server, private path or host. Examples and
  test fixtures use invented data (`example.org`, `example.com`).
- Secrets are never printed, logged or committed.
- Nothing is published outside the pull request: no gist, no upload, no paste service.
- **A permission denial is never worked around**: do what remains, say what was refused.

## Working on a machine

- The machine is the maintainer's own, in use while you work. Their current mail client and
  their mail bridge keep running: never stop, restart or reconfigure them, never touch their
  configuration or keyring entries. Run your own instance with a temporary configuration,
  runtime folder and keyring.
- **Never open a window on the maintainer's screens.** A window is tried, measured or captured
  in a headless nested compositor, or not at all.
- No artificial load: no stress runs, no parallel benchmark loops. Heavy builds run at low
  priority (`nice -n 19`), one at a time.
- Worktrees, logs and every other scratch file live outside the clone.
- Before removing a worktree, check it holds no uncommitted change and no unpushed commit
  (`git log HEAD --not --remotes`).

## GitHub gotchas

- Prefer REST (`gh api repos/…`) to GraphQL, which gets rate-limited.
- A body holding backticks or apostrophes is written to a file and passed with `--body-file`.

## When done

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, plus the
manual checks the issue asks for, and report the real output. If something fails, say so;
don't claim green.
