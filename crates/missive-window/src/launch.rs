//! What `missive-window` is started for: the reading window, or, from `missive compose` and
//! `missive mailto`, one compose window on a draft they saved first.

use missive_core::copy::Copy;

use crate::compose::Compose;
use crate::model::Sources;

#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    Reader,
    /// `missive-window compose --account <id> --draft <id>`.
    Compose {
        account: String,
        draft: String,
    },
}

/// Reads the arguments, the program's name left out.
pub fn parse(args: &[String]) -> Result<Launch, String> {
    let usage = "usage: missive-window [compose --account <ACCOUNT> --draft <DRAFT ID>]";
    match args {
        [] => Ok(Launch::Reader),
        [compose, rest @ ..] if compose == "compose" => {
            let (mut account, mut draft) = (None, None);
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                let slot = match flag.as_str() {
                    "--account" => &mut account,
                    "--draft" => &mut draft,
                    _ => return Err(usage.to_owned()),
                };
                *slot = Some(rest.next().ok_or(usage)?.clone());
            }
            match (account, draft) {
                (Some(account), Some(draft)) => Ok(Launch::Compose { account, draft }),
                _ => Err(usage.to_owned()),
            }
        }
        _ => Err(usage.to_owned()),
    }
}

/// The compose window on the draft `draft` of `account`, read from the copy. A draft the copy
/// does not hold leaves the window empty, and says so: the draft itself is safe in the copy or
/// on the server.
pub fn compose(sources: &Sources, account: String, draft: String) -> Compose {
    let accounts: Vec<String> = sources
        .accounts
        .list()
        .map(|listing| listing.accounts.into_iter().map(|(id, _)| id).collect())
        .unwrap_or_else(|_| vec![account.clone()]);
    let raw = match Copy::open(&sources.copy) {
        Ok(Some(copy)) => copy.draft(&account, &draft).map_err(|e| e.to_string()),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    };
    let found = match raw {
        Ok(Some(raw)) => Compose::draft(accounts.clone(), account.clone(), draft.clone(), &raw),
        Ok(None) => Err("it is not in the local copy".to_owned()),
        Err(e) => Err(e),
    };
    found.unwrap_or_else(|e| {
        let mut compose = Compose::new(accounts, account);
        compose.notice = Some(format!("The draft {draft} cannot be shown: {e}."));
        compose
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn the_window_starts_as_a_reader_or_on_a_draft() {
        assert_eq!(parse(&[]), Ok(Launch::Reader));
        assert_eq!(
            parse(&args(&[
                "compose",
                "--draft",
                "d@example.org",
                "--account",
                "alice"
            ])),
            Ok(Launch::Compose {
                account: "alice".to_owned(),
                draft: "d@example.org".to_owned()
            })
        );
        for wrong in [
            &["compose"][..],
            &["compose", "--account", "alice"],
            &["compose", "--account"],
            &["compose", "--draft", "d", "--account", "a", "--to", "x"],
            &["read"],
        ] {
            assert!(parse(&args(wrong)).is_err(), "{wrong:?}");
        }
    }
}
