//! The IMAP side of the fake server: a line-based parser, no literals.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use super::{Action, FolderState, Io, Shared, StoredMessage};

const SYSTEM_FLAGS: &str = "(\\Answered \\Flagged \\Deleted \\Seen \\Draft)";

pub(super) async fn serve(
    shared: Arc<Shared>,
    acceptor: TlsAcceptor,
    stream: Box<dyn Io>,
    tls: bool,
) {
    let mut session = Session {
        changes: shared.changes.subscribe(),
        shared,
        tls,
        user: None,
        selected: None,
        condstore: false,
        out: Vec::new(),
    };
    let mut io = BufReader::new(stream);
    let greeting = format!(
        "* OK [CAPABILITY {}] fake IMAP ready\r\n",
        session.capabilities()
    );
    session.out.extend_from_slice(greeting.as_bytes());
    let mut line = Vec::new();
    loop {
        if flush(&mut io, &mut session.out).await.is_err() {
            return;
        }
        line.clear();
        match io.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let rule = session.shared.rule(text);
        let flow = match rule {
            Some(Action::Refuse) => {
                session.refuse(text, Some("NO [UNAVAILABLE] the test refused this command"))
            }
            Some(Action::BareBad) => session.refuse(text, Some("BAD")),
            Some(Action::Stall) => session.refuse(text, None),
            _ => session.command(text),
        };
        if !session.shared.fetch_delay.is_zero() && is_fetch(text) {
            tokio::time::sleep(session.shared.fetch_delay).await;
        }
        if let Some(Action::Truncate(lines)) = rule {
            truncate(&mut session.out, lines);
        }
        if let Some(Action::Cut(lines)) = rule {
            cut(&mut session.out, lines);
            session.send("* BYE the test cut this answer short");
            if flush(&mut io, &mut session.out).await.is_ok() {
                let _ = io.shutdown().await;
            }
            return;
        }
        match flow {
            Flow::Continue => {}
            Flow::Close => {
                let _ = flush(&mut io, &mut session.out).await;
                return;
            }
            Flow::StartTls => {
                if flush(&mut io, &mut session.out).await.is_err() {
                    return;
                }
                let Some(stream) = super::start_tls(io.into_inner(), &acceptor).await else {
                    return;
                };
                io = BufReader::new(stream);
                session.tls = true;
            }
            Flow::Idle(tag) => {
                if !session.idle(&mut io, &tag).await {
                    return;
                }
            }
        }
    }
}

/// Whether a command line is a `FETCH` or a `UID FETCH`.
fn is_fetch(line: &str) -> bool {
    let mut words = line.split(' ').skip(1);
    let name = match words.next() {
        Some(word) if word.eq_ignore_ascii_case("UID") => words.next(),
        word => word,
    };
    name.is_some_and(|name| name.eq_ignore_ascii_case("FETCH"))
}

/// Keeps the first `lines` lines of an answer.
fn cut(out: &mut Vec<u8>, lines: usize) {
    let mut kept = 0;
    for _ in 0..lines {
        match out[kept..].windows(2).position(|w| w == b"\r\n") {
            Some(end) => kept += end + 2,
            None => break,
        }
    }
    out.truncate(kept);
}

/// Keeps the first `lines` lines of an answer and its last line, the tagged one.
fn truncate(out: &mut Vec<u8>, lines: usize) {
    let body = out.len().saturating_sub(2);
    let tagged = out[..body]
        .windows(2)
        .rposition(|w| w == b"\r\n")
        .map_or(0, |end| end + 2);
    let tagged = out.split_off(tagged);
    cut(out, lines);
    out.extend_from_slice(&tagged);
}

async fn flush(io: &mut BufReader<Box<dyn Io>>, out: &mut Vec<u8>) -> std::io::Result<()> {
    if !out.is_empty() {
        io.write_all(out).await?;
        out.clear();
    }
    io.flush().await
}

enum Flow {
    Continue,
    Close,
    StartTls,
    Idle(String),
}

struct Session {
    shared: Arc<Shared>,
    changes: watch::Receiver<()>,
    tls: bool,
    user: Option<String>,
    selected: Option<Selected>,
    /// Whether the client enabled CONDSTORE, which adds `MODSEQ` to flag updates.
    condstore: bool,
    /// What is written to the client before the next read.
    out: Vec<u8>,
}

/// The selected folder as this session last told the client about it: sequence numbers are
/// positions in `view`.
struct Selected {
    folder: String,
    read_only: bool,
    uidvalidity: u32,
    view: Vec<Seen>,
}

struct Seen {
    uid: u32,
    flags: Vec<String>,
}

impl Session {
    fn send(&mut self, line: impl AsRef<str>) {
        self.out.extend_from_slice(line.as_ref().as_bytes());
        self.out.extend_from_slice(b"\r\n");
    }

    fn capabilities(&self) -> String {
        let mut capabilities = String::from("IMAP4rev1");
        if !self.tls {
            capabilities.push_str(" STARTTLS LOGINDISABLED");
        }
        capabilities.push_str(" IDLE SPECIAL-USE");
        if self.shared.condstore {
            capabilities.push_str(" ENABLE CONDSTORE");
        }
        if self.shared.moves {
            capabilities.push_str(" MOVE");
        }
        if self.shared.uidplus {
            capabilities.push_str(" UIDPLUS");
        }
        capabilities
    }

    fn command(&mut self, line: &str) -> Flow {
        let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
        let tag = tag.to_owned();
        let (mut name, mut args) = rest.split_once(' ').unwrap_or((rest, ""));
        let mut uid = false;
        if name.eq_ignore_ascii_case("UID") {
            uid = true;
            (name, args) = args.split_once(' ').unwrap_or((args, ""));
        }
        let name = name.to_ascii_uppercase();
        self.log(&name, rest);

        let Some(args) = tokens(args) else {
            self.send(format!("{tag} BAD unparsable arguments"));
            return Flow::Continue;
        };
        let needs_login = !matches!(
            name.as_str(),
            "CAPABILITY" | "NOOP" | "LOGOUT" | "STARTTLS" | "LOGIN"
        );
        if needs_login && self.user.is_none() {
            self.send(format!("{tag} BAD log in first"));
            return Flow::Continue;
        }
        let needs_selected = matches!(
            name.as_str(),
            "FETCH" | "SEARCH" | "STORE" | "COPY" | "MOVE" | "EXPUNGE"
        );
        if needs_selected && self.selected.is_none() {
            self.send(format!("{tag} BAD select a folder first"));
            return Flow::Continue;
        }

        let result = match (uid, name.as_str()) {
            (false, "CAPABILITY") => {
                self.send(format!("* CAPABILITY {}", self.capabilities()));
                Ok(())
            }
            (false, "NOOP") => {
                if !self.sync() {
                    return Flow::Close;
                }
                Ok(())
            }
            (false, "LOGOUT") => {
                self.send("* BYE logging out");
                self.send(format!("{tag} OK LOGOUT completed"));
                return Flow::Close;
            }
            (false, "STARTTLS") => {
                if self.tls {
                    Err("BAD already in TLS".to_owned())
                } else {
                    self.send(format!("{tag} OK begin TLS negotiation now"));
                    return Flow::StartTls;
                }
            }
            (false, "LOGIN") => self.login(&args),
            (false, "ENABLE") => self.enable(&args),
            (false, "LIST") => self.list(&args),
            (false, "STATUS") => self.status(&args),
            (false, "SELECT") => return self.select(&tag, &args, false),
            (false, "EXAMINE") => return self.select(&tag, &args, true),
            (_, "FETCH") => self.fetch(&args, uid),
            (_, "SEARCH") => self.search(&args, uid),
            (_, "STORE") => self.store(&args, uid),
            (true, "COPY") => return self.copy(&tag, &args, false),
            (true, "MOVE") if self.shared.moves => return self.copy(&tag, &args, true),
            (true, "EXPUNGE") if self.shared.uidplus => self.expunge(&args),
            (false, "IDLE") => {
                self.send("+ idling");
                return Flow::Idle(tag);
            }
            _ => Err("BAD unknown command".to_owned()),
        };
        match result {
            Ok(()) => self.send(format!("{tag} OK {name} completed")),
            Err(answer) => self.send(format!("{tag} {answer}")),
        }
        Flow::Continue
    }

    /// Answers `answer` (or nothing) to a command a test asked to refuse, without running it.
    fn refuse(&mut self, line: &str, answer: Option<&str>) -> Flow {
        let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
        let name = rest
            .split(' ')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        self.log(&name, rest);
        if let Some(answer) = answer {
            self.send(format!("{tag} {answer}"));
        }
        Flow::Continue
    }

    /// Logs the command without its tag, and without the password of a `LOGIN`.
    fn log(&self, name: &str, command: &str) {
        let logged = if name == "LOGIN" {
            let user = tokens(&command[command.len().min(6)..])
                .and_then(|args| args.first().and_then(Token::string))
                .unwrap_or_default();
            format!("LOGIN {} ***", quote(&user))
        } else {
            command.to_owned()
        };
        self.shared.state().imap_log.push(logged);
    }

    fn login(&mut self, args: &[Token]) -> Result<(), String> {
        if !self.tls {
            return Err("NO [PRIVACYREQUIRED] start TLS first".to_owned());
        }
        let (Some(user), Some(password)) = (
            args.first().and_then(Token::string),
            args.get(1).and_then(Token::string),
        ) else {
            return Err("BAD LOGIN takes a user and a password".to_owned());
        };
        if let Some(text) = &self.shared.imap_login_no {
            return Err(format!("NO {text}"));
        }
        if !self.shared.authenticate(&user, &password) {
            return Err("NO [AUTHENTICATIONFAILED] invalid credentials".to_owned());
        }
        self.user = Some(user);
        Ok(())
    }

    fn enable(&mut self, args: &[Token]) -> Result<(), String> {
        let wants_condstore = args
            .iter()
            .filter_map(Token::string)
            .any(|a| a.eq_ignore_ascii_case("CONDSTORE"));
        if wants_condstore && self.shared.condstore {
            self.condstore = true;
            self.send("* ENABLED CONDSTORE");
        } else {
            self.send("* ENABLED");
        }
        Ok(())
    }

    fn list(&mut self, args: &[Token]) -> Result<(), String> {
        let (Some(reference), Some(pattern)) = (
            args.first().and_then(Token::string),
            args.get(1).and_then(Token::string),
        ) else {
            return Err("BAD LIST takes a reference and a pattern".to_owned());
        };
        if pattern.is_empty() {
            self.send("* LIST (\\Noselect) \"/\" \"\"");
            return Ok(());
        }
        let pattern = format!("{reference}{pattern}");
        let answers: Vec<String> = self
            .shared
            .state()
            .folders
            .iter()
            .filter(|f| matches(pattern.as_bytes(), f.name.as_bytes()))
            .map(|f| {
                let attributes = f.special_use.map(|u| u.attribute()).unwrap_or_default();
                format!("* LIST ({attributes}) \"/\" {}", quote(&f.name))
            })
            .collect();
        for answer in answers {
            self.send(answer);
        }
        Ok(())
    }

    fn status(&mut self, args: &[Token]) -> Result<(), String> {
        let (Some(name), Some(Token::List(items))) =
            (args.first().and_then(Token::string), args.get(1))
        else {
            return Err("BAD STATUS takes a folder and a list".to_owned());
        };
        let mut state = self.shared.state();
        let Some(folder) = state.find(&name) else {
            return Err("NO [NONEXISTENT] no such folder".to_owned());
        };
        let mut values = Vec::new();
        for item in items.iter().filter_map(Token::string) {
            let item = item.to_ascii_uppercase();
            let value = match item.as_str() {
                "MESSAGES" => folder.messages.len() as u64,
                "RECENT" => 0,
                "UIDNEXT" => folder.uidnext.into(),
                "UIDVALIDITY" => folder.uidvalidity.into(),
                "UNSEEN" => folder.messages.iter().filter(|m| !seen(m)).count() as u64,
                "HIGHESTMODSEQ" if self.shared.condstore => folder.highest_modseq,
                _ => return Err(format!("BAD unknown STATUS item {item}")),
            };
            values.push(format!("{item} {value}"));
        }
        let answer = format!("* STATUS {} ({})", quote(&name), values.join(" "));
        drop(state);
        self.send(answer);
        Ok(())
    }

    /// `SELECT` or `EXAMINE`, whose tagged answer tells the access the client got.
    fn select(&mut self, tag: &str, args: &[Token], read_only: bool) -> Flow {
        self.selected = None;
        let Some(name) = args.first().and_then(Token::string) else {
            self.send(format!("{tag} BAD SELECT takes a folder"));
            return Flow::Continue;
        };
        if let Some(Token::List(parameters)) = args.get(1) {
            let condstore = parameters
                .iter()
                .filter_map(Token::string)
                .any(|p| p.eq_ignore_ascii_case("CONDSTORE"));
            if condstore && self.shared.condstore {
                self.condstore = true;
            }
        }
        self.changes.borrow_and_update();
        let mut state = self.shared.state();
        let Some(folder) = state.find(&name) else {
            drop(state);
            self.send(format!("{tag} NO [NONEXISTENT] no such folder"));
            return Flow::Continue;
        };
        // In the order Gmail sends them: UIDVALIDITY before EXISTS, so that an answer cut short
        // can hold one without the other.
        let mut lines = vec![
            format!("* FLAGS {SYSTEM_FLAGS}"),
            if read_only {
                "* OK [PERMANENTFLAGS ()] read only".to_owned()
            } else {
                format!("* OK [PERMANENTFLAGS {SYSTEM_FLAGS}] flags allowed")
            },
            format!("* OK [UIDVALIDITY {}] UIDs valid", folder.uidvalidity),
            format!("* {} EXISTS", folder.messages.len()),
            "* 0 RECENT".to_owned(),
        ];
        if let Some(position) = folder.messages.iter().position(|m| !seen(m)) {
            lines.push(format!("* OK [UNSEEN {}] first unseen", position + 1));
        }
        lines.push(format!(
            "* OK [UIDNEXT {}] predicted next UID",
            folder.uidnext
        ));
        if self.shared.condstore {
            lines.push(format!(
                "* OK [HIGHESTMODSEQ {}] highest",
                folder.highest_modseq
            ));
        }
        let selected = Selected {
            folder: folder.name.clone(),
            read_only,
            uidvalidity: folder.uidvalidity,
            view: folder
                .messages
                .iter()
                .map(|m| Seen {
                    uid: m.uid,
                    flags: m.flags.clone(),
                })
                .collect(),
        };
        drop(state);
        for line in lines {
            self.send(line);
        }
        self.selected = Some(selected);
        let (access, command) = if read_only {
            ("READ-ONLY", "EXAMINE")
        } else {
            ("READ-WRITE", "SELECT")
        };
        self.send(format!("{tag} OK [{access}] {command} completed"));
        Flow::Continue
    }

    fn fetch(&mut self, args: &[Token], uid: bool) -> Result<(), String> {
        let (Some(set), Some(items)) = (args.first().and_then(Token::raw), args.get(1)) else {
            return Err("BAD FETCH takes a set and items".to_owned());
        };
        let mut items = match items {
            Token::List(list) => list
                .iter()
                .map(|t| t.raw().and_then(Item::parse))
                .collect::<Option<Vec<_>>>(),
            other => other.raw().and_then(Item::parse_macro),
        }
        .ok_or("BAD unsupported FETCH item")?;
        let changed_since = match args.get(2) {
            None => None,
            Some(Token::List(modifier)) => match modifier.as_slice() {
                [name, value]
                    if self.shared.condstore
                        && name
                            .raw()
                            .is_some_and(|n| n.eq_ignore_ascii_case("CHANGEDSINCE")) =>
                {
                    Some(
                        value
                            .raw()
                            .and_then(|v| v.parse::<u64>().ok())
                            .ok_or("BAD CHANGEDSINCE takes a number")?,
                    )
                }
                _ => return Err("BAD unsupported FETCH modifier".to_owned()),
            },
            Some(_) => return Err("BAD unsupported FETCH modifier".to_owned()),
        };
        if uid && !items.contains(&Item::Uid) {
            items.insert(0, Item::Uid);
        }
        if changed_since.is_some() && !items.contains(&Item::ModSeq) {
            items.push(Item::ModSeq);
        }

        let selected = self.selected.as_mut().expect("checked by the caller");
        let set = Set::parse(set).ok_or("BAD invalid set")?;
        let max = if uid {
            selected.view.last().map_or(0, |m| m.uid)
        } else {
            selected.view.len() as u32
        };
        let marks_seen = !selected.read_only && items.iter().any(Item::marks_seen);
        let mut state = self.shared.state();
        let refused_uids = state.refused_uids.clone();
        let folder = state.folder(&selected.folder);
        let mut answer = Vec::new();
        let mut changed = false;
        let mut refused = false;
        for (index, entry) in selected.view.iter_mut().enumerate() {
            let sequence = index as u32 + 1;
            if !set.contains(if uid { entry.uid } else { sequence }, max) {
                continue;
            }
            if uid && refused_uids.contains(&entry.uid) {
                refused = true;
                continue;
            }
            // A message expunged meanwhile is only told at the next NOOP; it has no data now.
            let Some(message) = folder.messages.iter_mut().find(|m| m.uid == entry.uid) else {
                continue;
            };
            if changed_since.is_some_and(|since| message.modseq <= since) {
                continue;
            }
            let mut items = items.clone();
            if marks_seen && !seen(message) {
                folder.highest_modseq += 1;
                message.modseq = folder.highest_modseq;
                message.flags.push("\\Seen".to_owned());
                changed = true;
                if !items.contains(&Item::Flags) {
                    items.push(Item::Flags);
                }
            }
            entry.flags = message.flags.clone();
            answer.extend_from_slice(format!("* {sequence} FETCH (").as_bytes());
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    answer.push(b' ');
                }
                item.write(message, &mut answer);
            }
            answer.extend_from_slice(b")\r\n");
        }
        drop(state);
        if changed {
            self.shared.changes.send_modify(|_| {});
            // This session's view is already up to date.
            self.changes.borrow_and_update();
        }
        self.out.extend_from_slice(&answer);
        if refused {
            return Err("NO [UNAVAILABLE] the test refused a message of this set".to_owned());
        }
        Ok(())
    }

    /// `STORE` or `UID STORE`: `FLAGS`, `+FLAGS` or `-FLAGS`, each with or without `.SILENT`.
    /// Each message whose flags it touched is answered with a `FETCH` of its flags, unless silent.
    fn store(&mut self, args: &[Token], uid: bool) -> Result<(), String> {
        let (Some(set), Some(item), Some(flags)) = (
            args.first().and_then(Token::raw),
            args.get(1).and_then(Token::raw),
            args.get(2),
        ) else {
            return Err("BAD STORE takes a set, an item and flags".to_owned());
        };
        let item = item.to_ascii_uppercase();
        let (item, silent) = match item.strip_suffix(".SILENT") {
            Some(item) => (item.to_owned(), true),
            None => (item, false),
        };
        let flags: Vec<String> = match flags {
            Token::List(list) => list.iter().filter_map(Token::string).collect(),
            other => other.string().into_iter().collect(),
        };
        let selected = self.selected.as_mut().expect("checked by the caller");
        if selected.read_only {
            return Err("NO [READ-ONLY] the folder was examined, not selected".to_owned());
        }
        let set = Set::parse(set).ok_or("BAD invalid set")?;
        let max = if uid {
            selected.view.last().map_or(0, |m| m.uid)
        } else {
            selected.view.len() as u32
        };
        let mut state = self.shared.state();
        let folder = state.folder(&selected.folder);
        let mut answer = Vec::new();
        for (index, entry) in selected.view.iter_mut().enumerate() {
            let sequence = index as u32 + 1;
            if !set.contains(if uid { entry.uid } else { sequence }, max) {
                continue;
            }
            let Some(message) = folder.messages.iter_mut().find(|m| m.uid == entry.uid) else {
                continue;
            };
            let before = message.flags.clone();
            let has =
                |flags: &[String], flag: &str| flags.iter().any(|f| f.eq_ignore_ascii_case(flag));
            match item.as_str() {
                "FLAGS" => message.flags = flags.clone(),
                "+FLAGS" => {
                    for flag in &flags {
                        if !has(&message.flags, flag) {
                            message.flags.push(flag.clone());
                        }
                    }
                }
                "-FLAGS" => message
                    .flags
                    .retain(|f| !flags.iter().any(|g| g.eq_ignore_ascii_case(f))),
                _ => return Err("BAD unknown STORE item".to_owned()),
            }
            if message.flags != before {
                folder.highest_modseq += 1;
                message.modseq = folder.highest_modseq;
            }
            entry.flags = message.flags.clone();
            if !silent {
                let modseq = if self.condstore {
                    format!(" MODSEQ ({})", message.modseq)
                } else {
                    String::new()
                };
                answer.push(format!(
                    "* {sequence} FETCH (UID {} FLAGS ({}){modseq})",
                    message.uid,
                    message.flags.join(" ")
                ));
            }
        }
        drop(state);
        self.shared.changes.send_modify(|_| {});
        // This session's view is already up to date.
        self.changes.borrow_and_update();
        for line in answer {
            self.send(line);
        }
        Ok(())
    }

    /// `UID COPY`, or `UID MOVE` when `moves`: the messages of the set go to the end of the
    /// target under new UIDs, told in `COPYUID` with UIDPLUS (in the tagged answer of a copy, in
    /// an untagged `OK` before the expunges of a move, as RFC 6851 has it).
    fn copy(&mut self, tag: &str, args: &[Token], moves: bool) -> Flow {
        let command = if moves { "MOVE" } else { "COPY" };
        let (Some(set), Some(target)) = (
            args.first().and_then(Token::raw).and_then(Set::parse),
            args.get(1).and_then(Token::string),
        ) else {
            self.send(format!("{tag} BAD {command} takes a set and a folder"));
            return Flow::Continue;
        };
        let selected = self.selected.as_mut().expect("checked by the caller");
        if moves && selected.read_only {
            self.send(format!(
                "{tag} NO [READ-ONLY] the folder was examined, not selected"
            ));
            return Flow::Continue;
        }
        let max = selected.view.last().map_or(0, |m| m.uid);
        let mut state = self.shared.state();
        let Some(target_index) = state.folders.iter().position(|f| f.name == target) else {
            drop(state);
            self.send(format!("{tag} NO [TRYCREATE] no such folder"));
            return Flow::Continue;
        };
        let source_index = state
            .folders
            .iter()
            .position(|f| f.name == selected.folder)
            .expect("the selected folder exists");
        let mut pairs = Vec::new();
        let mut gone = Vec::new();
        for (index, entry) in selected.view.iter().enumerate() {
            if !set.contains(entry.uid, max) {
                continue;
            }
            let Some(message) = state.folders[source_index]
                .messages
                .iter()
                .find(|m| m.uid == entry.uid)
            else {
                continue;
            };
            let message = StoredMessage {
                uid: message.uid,
                raw: message.raw.clone(),
                flags: message.flags.clone(),
                internal_date: message.internal_date.clone(),
                modseq: message.modseq,
            };
            let new = state.folders[target_index].add_copy(&message);
            pairs.push((message.uid, new));
            gone.push(index);
        }
        let uidvalidity = state.folders[target_index].uidvalidity;
        if moves {
            let moved: Vec<u32> = pairs.iter().map(|(old, _)| *old).collect();
            state.folders[source_index]
                .messages
                .retain(|m| !moved.contains(&m.uid));
        }
        drop(state);
        let copyuid = (self.shared.uidplus && !pairs.is_empty()).then(|| {
            let old: Vec<String> = pairs.iter().map(|(old, _)| old.to_string()).collect();
            let new: Vec<String> = pairs.iter().map(|(_, new)| new.to_string()).collect();
            format!(
                "[COPYUID {uidvalidity} {} {}] ",
                old.join(","),
                new.join(",")
            )
        });
        if moves {
            if let Some(copyuid) = &copyuid {
                self.send(format!("* OK {copyuid}moved"));
            }
            let selected = self.selected.as_mut().expect("checked above");
            let mut lines = Vec::new();
            for index in gone.into_iter().rev() {
                lines.push(format!("* {} EXPUNGE", index + 1));
                selected.view.remove(index);
            }
            for line in lines {
                self.send(line);
            }
        }
        if !pairs.is_empty() {
            self.shared.changes.send_modify(|_| {});
            self.changes.borrow_and_update();
        }
        let code = if moves { None } else { copyuid };
        self.send(format!(
            "{tag} OK {}{command} completed",
            code.unwrap_or_default()
        ));
        Flow::Continue
    }

    /// `UID EXPUNGE`: removes the messages of the set marked `\Deleted`, and only those.
    fn expunge(&mut self, args: &[Token]) -> Result<(), String> {
        let Some(set) = args.first().and_then(Token::raw).and_then(Set::parse) else {
            return Err("BAD UID EXPUNGE takes a set".to_owned());
        };
        let selected = self.selected.as_mut().expect("checked by the caller");
        if selected.read_only {
            return Err("NO [READ-ONLY] the folder was examined, not selected".to_owned());
        }
        let max = selected.view.last().map_or(0, |m| m.uid);
        let mut state = self.shared.state();
        let folder = state.folder(&selected.folder);
        let mut lines = Vec::new();
        for index in (0..selected.view.len()).rev() {
            let uid = selected.view[index].uid;
            if !set.contains(uid, max) {
                continue;
            }
            let deleted = folder.messages.iter().any(|m| {
                m.uid == uid && m.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Deleted"))
            });
            if deleted {
                folder.messages.retain(|m| m.uid != uid);
                selected.view.remove(index);
                lines.push(format!("* {} EXPUNGE", index + 1));
            }
        }
        drop(state);
        if !lines.is_empty() {
            self.shared.changes.send_modify(|_| {});
            self.changes.borrow_and_update();
        }
        for line in lines {
            self.send(line);
        }
        Ok(())
    }

    fn search(&mut self, args: &[Token], uid: bool) -> Result<(), String> {
        let selected = self.selected.as_ref().expect("checked by the caller");
        let mut state = self.shared.state();
        let folder = state.folder(&selected.folder);
        let mut criteria: Vec<Criterion> = Vec::new();
        let mut args = args.iter();
        while let Some(token) = args.next() {
            let word = token.raw().ok_or("BAD unsupported SEARCH criterion")?;
            let upper = word.to_ascii_uppercase();
            let flag = |name: &str, present: bool| -> Criterion {
                let name = name.to_owned();
                Box::new(move |_, m| {
                    m.flags.iter().any(|f| f.eq_ignore_ascii_case(&name)) == present
                })
            };
            criteria.push(match upper.as_str() {
                "ALL" => Box::new(|_, _| true),
                "SEEN" => flag("\\Seen", true),
                "UNSEEN" => flag("\\Seen", false),
                "FLAGGED" => flag("\\Flagged", true),
                "UNFLAGGED" => flag("\\Flagged", false),
                "ANSWERED" => flag("\\Answered", true),
                "UNANSWERED" => flag("\\Answered", false),
                "DELETED" => flag("\\Deleted", true),
                "UNDELETED" => flag("\\Deleted", false),
                "DRAFT" => flag("\\Draft", true),
                "UNDRAFT" => flag("\\Draft", false),
                "HEADER" => {
                    let (Some(field), Some(value)) = (
                        args.next().and_then(Token::string),
                        args.next().and_then(Token::string),
                    ) else {
                        return Err("BAD HEADER takes a field and a string".to_owned());
                    };
                    Box::new(move |_, m| header_holds(&m.raw, &field, &value))
                }
                "UID" => {
                    let set = args
                        .next()
                        .and_then(Token::raw)
                        .and_then(Set::parse)
                        .ok_or("BAD UID takes a set")?;
                    let max = selected.view.last().map_or(0, |m| m.uid);
                    Box::new(move |_, m| set.contains(m.uid, max))
                }
                _ => {
                    let set = Set::parse(word).ok_or("BAD unsupported SEARCH criterion")?;
                    let max = selected.view.len() as u32;
                    Box::new(move |sequence, _| set.contains(sequence, max))
                }
            });
        }
        let mut found = Vec::new();
        for (index, entry) in selected.view.iter().enumerate() {
            let sequence = index as u32 + 1;
            let Some(message) = folder.messages.iter().find(|m| m.uid == entry.uid) else {
                continue;
            };
            if criteria.iter().all(|matches| matches(sequence, message)) {
                found.push(if uid { entry.uid } else { sequence }.to_string());
            }
        }
        drop(state);
        if found.is_empty() {
            self.send("* SEARCH");
        } else {
            self.send(format!("* SEARCH {}", found.join(" ")));
        }
        Ok(())
    }

    /// Tells the client what changed in its selected folder since it last heard, as a real server
    /// does at `NOOP` and during `IDLE`. Returns `false` when the connection must close.
    fn sync(&mut self) -> bool {
        self.changes.borrow_and_update();
        let Some(selected) = self.selected.as_mut() else {
            return true;
        };
        let mut state = self.shared.state();
        let folder: &FolderState = state.folder(&selected.folder);
        let mut lines = Vec::new();
        if folder.uidvalidity != selected.uidvalidity {
            drop(state);
            self.send("* BYE the folder was rebuilt (UIDVALIDITY changed)");
            return false;
        }
        // From the end, so that each sequence number is still right when it is told.
        for index in (0..selected.view.len()).rev() {
            if !folder
                .messages
                .iter()
                .any(|m| m.uid == selected.view[index].uid)
            {
                lines.push(format!("* {} EXPUNGE", index + 1));
                selected.view.remove(index);
            }
        }
        for (index, entry) in selected.view.iter_mut().enumerate() {
            let message = folder.messages.iter().find(|m| m.uid == entry.uid).unwrap();
            if message.flags != entry.flags {
                entry.flags = message.flags.clone();
                let modseq = if self.condstore {
                    format!(" MODSEQ ({})", message.modseq)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "* {} FETCH (UID {} FLAGS ({}){modseq})",
                    index + 1,
                    message.uid,
                    message.flags.join(" ")
                ));
            }
        }
        let last = selected.view.last().map_or(0, |m| m.uid);
        let before = selected.view.len();
        selected.view.extend(
            folder
                .messages
                .iter()
                .filter(|m| m.uid > last)
                .map(|m| Seen {
                    uid: m.uid,
                    flags: m.flags.clone(),
                }),
        );
        if selected.view.len() != before {
            lines.push(format!("* {} EXISTS", selected.view.len()));
        }
        drop(state);
        for line in lines {
            self.send(line);
        }
        true
    }

    /// Serves `IDLE` until `DONE`, telling the client each change as it happens. Returns `false`
    /// when the connection is over.
    async fn idle(&mut self, io: &mut BufReader<Box<dyn Io>>, tag: &str) -> bool {
        // Kept across iterations: a read interrupted by a change keeps what it already read.
        let mut line = Vec::new();
        loop {
            if !self.sync() {
                let _ = flush(io, &mut self.out).await;
                return false;
            }
            if flush(io, &mut self.out).await.is_err() {
                return false;
            }
            let keepalive = self.shared.idle_keepalive;
            let read = tokio::select! {
                read = io.read_until(b'\n', &mut line) => Some(read),
                _ = self.changes.changed() => None,
                _ = tokio::time::sleep(keepalive), if !keepalive.is_zero() => {
                    self.send("* OK Still here");
                    None
                }
            };
            match read {
                None => continue,
                Some(Ok(0)) | Some(Err(_)) => return false,
                Some(Ok(_)) => {}
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\r', '\n']);
            self.shared.state().imap_log.push(text.to_owned());
            if text.eq_ignore_ascii_case("DONE") {
                self.send(format!("{tag} OK IDLE terminated"));
            } else {
                self.send(format!("{tag} BAD expected DONE"));
            }
            return true;
        }
    }
}

/// Whether a header field of the message holds `value`, in any case, as `SEARCH HEADER` asks.
fn header_holds(raw: &[u8], field: &str, value: &str) -> bool {
    let header = String::from_utf8_lossy(&raw[..header_end(raw)]);
    let value = value.to_ascii_lowercase();
    let prefix = format!("{}:", field.to_ascii_lowercase());
    header.split("\r\n").any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with(&prefix) && line[prefix.len()..].contains(&value)
    })
}

/// A `SEARCH` criterion, given a message and its sequence number.
type Criterion = Box<dyn Fn(u32, &StoredMessage) -> bool>;

fn seen(message: &StoredMessage) -> bool {
    message
        .flags
        .iter()
        .any(|f| f.eq_ignore_ascii_case("\\Seen"))
}

/// A `FETCH` data item.
#[derive(Clone, PartialEq, Eq)]
enum Item {
    Uid,
    Flags,
    InternalDate,
    Size,
    ModSeq,
    /// `BODY[section]`, `BODY.PEEK[section]`, `RFC822`, `RFC822.HEADER`, `RFC822.TEXT`.
    Body {
        name: &'static str,
        section: Section,
        peek: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Full,
    Header,
    Text,
}

impl Item {
    fn parse(item: &str) -> Option<Item> {
        let upper = item.to_ascii_uppercase();
        let body = |name, section, peek| {
            Some(Item::Body {
                name,
                section,
                peek,
            })
        };
        match upper.as_str() {
            "UID" => Some(Item::Uid),
            "FLAGS" => Some(Item::Flags),
            "INTERNALDATE" => Some(Item::InternalDate),
            "RFC822.SIZE" => Some(Item::Size),
            "MODSEQ" => Some(Item::ModSeq),
            "BODY[]" => body("BODY[]", Section::Full, false),
            "BODY.PEEK[]" => body("BODY[]", Section::Full, true),
            "BODY[HEADER]" => body("BODY[HEADER]", Section::Header, false),
            "BODY.PEEK[HEADER]" => body("BODY[HEADER]", Section::Header, true),
            "BODY[TEXT]" => body("BODY[TEXT]", Section::Text, false),
            "BODY.PEEK[TEXT]" => body("BODY[TEXT]", Section::Text, true),
            "RFC822" => body("RFC822", Section::Full, false),
            "RFC822.HEADER" => body("RFC822.HEADER", Section::Header, true),
            "RFC822.TEXT" => body("RFC822.TEXT", Section::Text, false),
            _ => None,
        }
    }

    /// A single item, or `FAST`. `ALL` and `FULL` need `ENVELOPE`, which is not served.
    fn parse_macro(item: &str) -> Option<Vec<Item>> {
        if item.eq_ignore_ascii_case("FAST") {
            Some(vec![Item::Flags, Item::InternalDate, Item::Size])
        } else {
            Item::parse(item).map(|item| vec![item])
        }
    }

    fn marks_seen(&self) -> bool {
        matches!(self, Item::Body { peek: false, .. })
    }

    fn write(&self, message: &StoredMessage, out: &mut Vec<u8>) {
        match self {
            Item::Uid => out.extend_from_slice(format!("UID {}", message.uid).as_bytes()),
            Item::Flags => {
                out.extend_from_slice(format!("FLAGS ({})", message.flags.join(" ")).as_bytes())
            }
            Item::InternalDate => out.extend_from_slice(
                format!("INTERNALDATE {}", quote(&message.internal_date)).as_bytes(),
            ),
            Item::Size => {
                out.extend_from_slice(format!("RFC822.SIZE {}", message.raw.len()).as_bytes())
            }
            Item::ModSeq => {
                out.extend_from_slice(format!("MODSEQ ({})", message.modseq).as_bytes())
            }
            Item::Body { name, section, .. } => {
                let raw = &message.raw;
                let split = header_end(raw);
                let bytes = match section {
                    Section::Full => &raw[..],
                    Section::Header => &raw[..split],
                    Section::Text => &raw[split..],
                };
                out.extend_from_slice(format!("{name} {{{}}}\r\n", bytes.len()).as_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
}

/// Where the header ends, its blank line included.
fn header_end(raw: &[u8]) -> usize {
    if let Some(position) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        return position + 4;
    }
    if let Some(position) = raw.windows(2).position(|w| w == b"\n\n") {
        return position + 2;
    }
    raw.len()
}

/// A sequence set or a UID set: `1`, `2:4`, `5:*`, `1,3:*`.
struct Set(Vec<(Bound, Bound)>);

#[derive(Clone, Copy)]
enum Bound {
    Number(u32),
    Star,
}

impl Set {
    fn parse(text: &str) -> Option<Set> {
        let bound = |b: &str| match b {
            "*" => Some(Bound::Star),
            _ => b.parse().ok().filter(|&n| n > 0).map(Bound::Number),
        };
        text.split(',')
            .map(|part| match part.split_once(':') {
                Some((low, high)) => Some((bound(low)?, bound(high)?)),
                None => bound(part).map(|b| (b, b)),
            })
            .collect::<Option<Vec<_>>>()
            .map(Set)
    }

    /// `*` stands for `max`, the largest number in use; a range is the same in either order, so
    /// `UID 9:*` with 5 as the largest UID holds UID 5, as RFC 3501 says.
    fn contains(&self, number: u32, max: u32) -> bool {
        let value = |b: Bound| match b {
            Bound::Number(n) => n,
            Bound::Star => max,
        };
        self.0.iter().any(|&(a, b)| {
            let (a, b) = (value(a), value(b));
            (a.min(b)..=a.max(b)).contains(&number)
        })
    }
}

/// A command argument.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    Atom(String),
    Quoted(String),
    List(Vec<Token>),
}

impl Token {
    /// An atom or a quoted string, as a string.
    fn string(&self) -> Option<String> {
        match self {
            Token::Atom(s) | Token::Quoted(s) => Some(s.clone()),
            Token::List(_) => None,
        }
    }

    /// An atom as written.
    fn raw(&self) -> Option<&str> {
        match self {
            Token::Atom(s) => Some(s),
            _ => None,
        }
    }
}

/// Splits arguments into atoms, quoted strings and parenthesised lists. Brackets belong to the
/// atom they are in (`BODY.PEEK[HEADER.FIELDS (FROM)]`). Literals are not supported.
fn tokens(text: &str) -> Option<Vec<Token>> {
    let mut stack: Vec<Vec<Token>> = vec![Vec::new()];
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' => {
                chars.next();
            }
            '(' => {
                chars.next();
                stack.push(Vec::new());
            }
            ')' => {
                chars.next();
                let list = stack.pop()?;
                stack.last_mut()?.push(Token::List(list));
            }
            '"' => {
                chars.next();
                let mut string = String::new();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => string.push(chars.next()?),
                        c => string.push(c),
                    }
                }
                stack.last_mut()?.push(Token::Quoted(string));
            }
            '{' => return None,
            _ => {
                let mut atom = String::new();
                let mut depth = 0;
                while let Some(&c) = chars.peek() {
                    if depth == 0 && matches!(c, ' ' | '(' | ')' | '"') {
                        break;
                    }
                    match c {
                        '[' => depth += 1,
                        ']' => depth -= 1,
                        _ => {}
                    }
                    atom.push(c);
                    chars.next();
                }
                stack.last_mut()?.push(Token::Atom(atom));
            }
        }
    }
    if stack.len() == 1 { stack.pop() } else { None }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Matches a `LIST` pattern: `*` matches anything, `%` anything but the delimiter `/`.
fn matches(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|i| matches(rest, &name[i..])),
        Some((b'%', rest)) => (0..=name.len())
            .take_while(|&i| i == 0 || name[i - 1] != b'/')
            .any(|i| matches(rest, &name[i..])),
        Some((&c, rest)) => name.first() == Some(&c) && matches(rest, &name[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_keep_brackets_in_their_atom() {
        let parsed =
            tokens("1:* (UID BODY.PEEK[HEADER.FIELDS (FROM TO)]) \"a \\\"b\\\"\"").unwrap();
        assert_eq!(
            parsed,
            [
                Token::Atom("1:*".into()),
                Token::List(vec![
                    Token::Atom("UID".into()),
                    Token::Atom("BODY.PEEK[HEADER.FIELDS (FROM TO)]".into()),
                ]),
                Token::Quoted("a \"b\"".into()),
            ]
        );
    }

    #[test]
    fn unbalanced_lists_and_literals_are_refused() {
        assert!(tokens("(UID").is_none());
        assert!(tokens("UID)").is_none());
        assert!(tokens("{5}").is_none());
    }

    #[test]
    fn a_set_reads_ranges_and_star_in_either_order() {
        let set = Set::parse("2,4:*").unwrap();
        assert!(!set.contains(1, 6));
        assert!(set.contains(2, 6));
        assert!(!set.contains(3, 6));
        assert!(set.contains(6, 6));
        // `9:*` with 5 as the largest number is `5:9`.
        assert!(Set::parse("9:*").unwrap().contains(5, 5));
        assert!(Set::parse("0").is_none());
        assert!(Set::parse("a:3").is_none());
    }

    #[test]
    fn list_patterns_stop_percent_at_the_delimiter() {
        assert!(matches(b"*", b"Archive/2026"));
        assert!(matches(b"%", b"Archive"));
        assert!(!matches(b"%", b"Archive/2026"));
        assert!(matches(b"Archive/%", b"Archive/2026"));
        assert!(matches(b"INBOX", b"INBOX"));
        assert!(!matches(b"INBOX", b"INBOX/Sub"));
    }

    #[test]
    fn the_header_ends_after_its_blank_line() {
        assert_eq!(header_end(b"A: b\r\n\r\nbody"), 8);
        assert_eq!(header_end(b"A: b\n\nbody"), 6);
        assert_eq!(header_end(b"A: b"), 4);
    }
}
