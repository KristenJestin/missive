//! How fast `status`, `unread` and `search` answer on a big copy: 50 000 generated messages,
//! written straight through the store (not through IMAP). Too long for the normal suite in a
//! debug build, so it is run by hand, in release, and its numbers go in the pull request:
//!
//! ```text
//! nice -n 19 cargo test --release -p missive --test perf -- --ignored --nocapture
//! ```
//!
//! The data is invented: words drawn from a vocabulary of 5 000 made-up words, a few very common
//! and most rare, 80–250 words to a message.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use missive_core::account::{Account, AccountStore, Security, Server};
use missive_core::copy::{Copy, Search};
use missive_core::store::{NewMessage, Store};
use rusqlite::Connection;

const MESSAGES: u32 = 50_000;
const SEARCH_LIMIT: Duration = Duration::from_millis(30);

/// A small deterministic generator, so that every run builds the same copy.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// An index in `0..n`, small ones far more often than large ones.
    fn skewed(&mut self, n: u64) -> u64 {
        let u = self.below(1_000_000) as f64 / 1_000_000.0;
        ((u * u * u) * n as f64) as u64
    }
}

fn word(index: u64) -> String {
    let consonants = b"bcdfghjklmnprstvwxyz";
    let vowels = b"aeiou";
    let mut n = index + 1;
    let mut word = String::new();
    while n > 0 {
        word.push(consonants[(n % 20) as usize] as char);
        word.push(vowels[((n / 20) % 5) as usize] as char);
        n /= 100;
    }
    word
}

fn sentence(random: &mut Random, words: u64) -> String {
    (0..words)
        .map(|_| word(random.skewed(5_000)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn build(path: &Path) {
    let mut store = Store::open(path).unwrap();
    let mut random = Random(42);
    let mut folders = Vec::new();
    for name in ["alice", "bob", "carol"] {
        let account = store.account(name).unwrap();
        store.set_state(account, "ready", true).unwrap();
        let mut ids = vec![store.folder(account, "INBOX", Some("inbox")).unwrap().id];
        ids.push(store.folder(account, "Sent", Some("sent")).unwrap().id);
        ids.push(
            store
                .folder(account, "Archive", Some("archive"))
                .unwrap()
                .id,
        );
        for n in 0..5 {
            ids.push(
                store
                    .folder(account, &format!("Folder {n}"), None)
                    .unwrap()
                    .id,
            );
        }
        folders.push(ids);
    }
    let mut uid = vec![0u32; 1_000];
    let mut batch: Vec<(i64, NewMessage)> = Vec::new();
    for n in 0..MESSAGES {
        let account = random.below(3) as usize;
        // Most of the mail is in the inbox and the archive.
        let folder = folders[account][[0, 0, 0, 2, 2, 1, 3, 4, 5, 6, 7][random.below(11) as usize]];
        uid[folder as usize] += 1;
        let words = 80 + random.below(170);
        let mut body = sentence(&mut random, words);
        if n % 10_000 == 7 {
            body.push_str(" quokka");
        }
        let sender = random.below(300);
        let subject_words = 3 + random.below(6);
        let raw = format!(
            "From: {} {} <{}.{}@example.com>\r\nTo: {} <{}@example.org>\r\nSubject: {}\r\n\
             Message-ID: <{n}@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\
             \r\n{body}\r\n",
            word(sender),
            word(sender + 1_000),
            word(sender),
            word(sender + 1_000),
            word(random.below(50)),
            word(random.below(50)),
            sentence(&mut random, subject_words),
        )
        .into_bytes();
        batch.push((
            folder,
            NewMessage {
                uid: uid[folder as usize],
                flags: if random.below(10) < 8 { "\\Seen" } else { "" }.to_owned(),
                internal_date: Some(1_700_000_000 + random.below(100_000_000) as i64),
                size: raw.len() as i64,
                raw,
            },
        ));
        if batch.len() == 200 || n + 1 == MESSAGES {
            // One folder at a time, as the daemon writes.
            batch.sort_by_key(|(folder, _)| *folder);
            while !batch.is_empty() {
                let folder = batch[0].0;
                let split = batch
                    .iter()
                    .position(|(f, _)| *f != folder)
                    .unwrap_or(batch.len());
                let messages: Vec<NewMessage> = batch.drain(..split).map(|(_, m)| m).collect();
                store.insert(folder, &messages).unwrap();
            }
        }
    }
}

/// The median of the times `run` takes, over 21 runs, after one to warm the cache.
fn median(mut run: impl FnMut()) -> Duration {
    run();
    let mut times: Vec<Duration> = (0..21)
        .map(|_| {
            let started = Instant::now();
            run();
            started.elapsed()
        })
        .collect();
    times.sort();
    times[10]
}

fn search(copy: &Copy, terms: &[&str], accounts: &[&str], role: Option<&str>) -> usize {
    copy.search(&Search {
        terms: terms.iter().map(|t| (*t).to_owned()).collect(),
        accounts: accounts.iter().map(|a| (*a).to_owned()).collect(),
        role: role.map(str::to_owned),
        limit: 20,
    })
    .unwrap()
    .len()
}

#[test]
#[ignore = "builds a copy of 50 000 messages: run it by hand, in release"]
fn search_unread_and_status_answer_quickly_on_50_000_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data/missive/copy.sqlite");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let started = Instant::now();
    build(&path);
    eprintln!("built {MESSAGES} messages in {:.1?}", started.elapsed());
    {
        let connection = Connection::open(&path).unwrap();
        let sizes = |tables: &str| -> i64 {
            connection
                .query_row(
                    &format!("SELECT sum(pgsize) FROM dbstat WHERE name IN ({tables})"),
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let raw = sizes("'message_raw'");
        let envelope = sizes("'message'");
        let index = sizes(
            "'message_search_data', 'message_search_idx', 'message_search_docsize', \
             'message_search_config'",
        );
        eprintln!(
            "bytes per message: raw {}, envelope {}, search index {}",
            raw / MESSAGES as i64,
            envelope / MESSAGES as i64,
            index / MESSAGES as i64
        );
    }

    let copy = Copy::open(&path).unwrap().unwrap();
    type Case<'a> = (&'a str, &'a [&'a str], &'a [&'a str], Option<&'a str>);
    let cases: [Case; 6] = [
        ("a rare word", &["quokka"], &["alice", "bob", "carol"], None),
        (
            "the commonest word",
            &[&word(0)],
            &["alice", "bob", "carol"],
            None,
        ),
        (
            "two common words",
            &[&word(0), &word(1)],
            &["alice", "bob", "carol"],
            None,
        ),
        (
            "a sender",
            &[&word(5), &word(1_005)],
            &["alice", "bob", "carol"],
            None,
        ),
        (
            "one account, the inbox",
            &[&word(0)],
            &["alice"],
            Some("inbox"),
        ),
        (
            "a word nobody has",
            &["absentword"],
            &["alice", "bob", "carol"],
            None,
        ),
    ];
    for (what, terms, accounts, role) in cases {
        let mut found = 0;
        let time = median(|| {
            let copy = Copy::open(&path).unwrap().unwrap();
            found = search(&copy, terms, accounts, role);
        });
        eprintln!("search, {what}: {time:.1?} ({found} shown)");
        assert!(time < SEARCH_LIMIT, "search, {what}: {time:?}");
    }
    let time = median(|| {
        let copy = Copy::open(&path).unwrap().unwrap();
        copy.unread().unwrap();
    });
    eprintln!("unread: {time:.1?}");
    let time = median(|| {
        let copy = Copy::open(&path).unwrap().unwrap();
        copy.accounts().unwrap();
    });
    eprintln!("status: {time:.1?}");
    drop(copy);

    // The same through the program, the start of the process included.
    let config = dir.path().join("config");
    let accounts = AccountStore::new(config.join("missive"));
    for id in ["alice", "bob", "carol"] {
        let server = Server {
            host: "127.0.0.1".to_owned(),
            port: 1,
            security: Security::Tls,
        };
        let account = Account {
            name: id.to_owned(),
            address: format!("{id}@example.org"),
            user: id.to_owned(),
            imap: server.clone(),
            smtp: server,
            server_saves_sent: false,
        };
        accounts.save(id, &account, None).unwrap();
    }
    let word0 = word(0);
    for args in [
        vec!["search", "--json", "quokka"],
        vec!["search", "--json", &word0],
        vec!["unread", "--json"],
        vec!["status", "--json"],
    ] {
        let time = median(|| {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_missive"))
                .args(&args)
                .env_clear()
                .env("HOME", dir.path())
                .env("XDG_CONFIG_HOME", &config)
                .env("XDG_DATA_HOME", dir.path().join("data"))
                .env("XDG_RUNTIME_DIR", dir.path().join("runtime"))
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success());
        });
        eprintln!("missive {}: {time:.1?} with the process", args.join(" "));
    }
}
