# The command line

`missive …` drives Missive. What it prints for people may change; what it prints with `--json`
is a contract read by other programs: fields are only ever added, never renamed, retyped or
removed without a new major version.

Errors go to standard error, prefixed `missive: `, and the exit status is 1 (2 for a usage
error).

## Accounts

An account is named by its **identifier**: a short name made of letters, digits and
`. _ @ + -`, chosen with `--id` when it is added and the address otherwise. Commands take it to
say which account they mean.

### `missive account add`

Adds an IMAP/SMTP account after logging in to both servers with it. Nothing is saved when a login
fails.

| Option | Value | Default |
|---|---|---|
| `--id` | the identifier | the address |
| `--name` | the display name, as in `Alice Example <alice@example.org>` | asked |
| `--address` | the address | asked |
| `--imap-host`, `--smtp-host` | the server | asked |
| `--imap-security`, `--smtp-security` | `tls` or `starttls` | `tls` |
| `--imap-port` | the port | 993 with `tls`, 143 with `starttls` |
| `--smtp-port` | the port | 465 with `tls`, 587 with `starttls` |
| `--user` | the login on both servers | the address |
| `--certificate <file.pem>` | trust exactly this server certificate for this account | the system's authorities |
| `--password-stdin` | read the password from standard input, one line | asked without echo |

On a terminal, what no option gives is asked for. When standard input is not a terminal (or with
`--password-stdin`), nothing is asked: a missing value without a default is an error, and the
password comes only from `--password-stdin`. It is never an option, since every process can read
the command line.

The password goes only to the keyring (the Secret Service: an item of the default collection with
the attributes `application` = `missive` and `account` = the identifier). The settings go to
`$XDG_CONFIG_HOME/missive/accounts/<id>/account.toml` (or `~/.config/…`), and the imported
certificate beside them, as `certificate.pem`.

With `--certificate`, the server must present exactly that certificate; its names and dates are
not checked, so a local bridge's self-signed certificate, even one marked as a CA, is accepted.
Without it, the server's certificate must be signed by an authority of the system's store.

When a login fails, the message says which server and why, by its connection state:

| State | Meaning |
|---|---|
| `unreachable` | the server could not be reached, or did not answer as expected; the password is not in question |
| `auth-refused` | the server refused the user name or the password |
| `certificate-refused` | the server's certificate is trusted neither by the system nor by the imported certificate: import it with `--certificate <file.pem>` if the server is yours |

### `missive account list [--json]`

Lists the accounts, by identifier. It reads the settings only: neither the network nor the
keyring. With `--json`, standard output is one JSON array, one object per account:

```json
[
  {
    "id": "alice@example.org",
    "name": "Alice Example",
    "address": "alice@example.org",
    "user": "alice@example.org",
    "imap": { "host": "127.0.0.1", "port": 1143, "security": "starttls" },
    "smtp": { "host": "127.0.0.1", "port": 1025, "security": "starttls" },
    "certificate": true
  }
]
```

| Field | Type | |
|---|---|---|
| `id` | string | the identifier |
| `name` | string | the display name |
| `address` | string | |
| `user` | string | the login on both servers |
| `imap`, `smtp` | object | `host` (string), `port` (number), `security` (`"tls"` or `"starttls"`) |
| `certificate` | boolean | whether a certificate is pinned for the account |

Without accounts, it prints `[]`. The array is printed on one line; the layout above is only for
reading.

An account whose `account.toml` cannot be read or is invalid does not stop the others: it is
left out, and one line on standard error names its folder and the kind of error (never the
content of the file). Standard output stays valid, and the exit status is still success.

### `missive account remove <id>`

Removes the account's keyring entry, then its settings and certificate. The keyring goes first:
if it is unavailable, the account stays whole and the command can be run again.

## The event stream

### `missive watch`

Prints what the daemon does, one JSON object per line on standard output, until it is stopped:
new messages, unread counts, connection states, syncs. The lines and their fields are those of
`docs/events.md`; each line is flushed as it is written, so a program reading the pipe sees it at
once. Diagnostics go to standard error.

`watch` never starts the daemon. Without one it prints `{"protocol":1,"event":"gone"}` and waits,
looking for the daemon's socket once a second; when a daemon appears it subscribes, and the
stream goes on (starting, as every subscription does, with the present state). When the daemon
stops, `watch` says `gone` again and goes back to waiting. Several `watch` may run at once.

SIGTERM or SIGINT (Ctrl-C) stops it, and so does the closing of standard output (a program that
stops reading, as `missive watch | head -1` does): neither is an error, and the exit status is
success. The status is 1 when the daemon refuses the subscription (it speaks another protocol
version, say) or the socket cannot be reached for another reason than the daemon's absence.

## The local copy

Missive keeps a copy of each account in `$XDG_DATA_HOME/missive/copy.sqlite` (or
`~/.local/share/missive/…`): folders, messages, flags and each account's connection state. Only
the daemon talks to the servers and writes the copy, and it never changes anything on a server:
it only reads.

### `missive sync [--account <id>]`

Asks the daemon to sync every account now, or one, waits until that sync has ended, and prints
each account's connection state, one line per account:

```
alice: ready
bob: auth-refused
```

When no daemon answers, `sync` starts one in the background first; its log goes to
`$XDG_STATE_HOME/missive/daemon.log` (or `~/.local/state/missive/…`). The exit status is success
only when every account is `ready`; for the others, standard error says why. The states are those
of `docs/protocol.md`; `unreachable` never asks to log in again, the daemon tries again by itself.

### `missive daemon`

Runs the daemon in the foreground, logging to standard error. One daemon runs per user: a second
one refuses to start. SIGTERM or SIGINT (Ctrl-C) stops it cleanly.

| Option | Value | Default |
|---|---|---|
| `--check-interval` | how often, in seconds, the folders other than the inbox are checked | 300 |
| `--batch-size` | how many new messages are fetched, and written to the copy, at a time, from 1 to 100 (and never more than 32 MiB of them, but for a single larger message) | 50 |

New mail in the inbox comes at once, through IDLE, which is issued again at each check (and at
least every 25 minutes). A first sync goes inbox first, newest first: the copy is usable from its
first batch on.

The log says what happened in counts, durations, folder roles and error kinds. It names an
account by its number in the copy and a folder by its id there, never by its name, and never
shows a subject, an address or a message.

## Reading the local copy

`missive status`, `missive unread` and `missive search` answer from the local copy alone: they
open it read only, never create it, and touch neither the network, the keyring nor the daemon's
socket. They work with the daemon stopped and the network down, in a few milliseconds, even
while the daemon is writing.

They show the accounts of the configuration, by identifier: an account removed from the
configuration is not shown even if the copy still holds its mail, and an account that is in the
configuration but has not reached the copy yet counts as never synced, with no message. An
account whose `account.toml` cannot be read is named on standard error, as `account list` does.

When there is no copy yet (no daemon has run), they say so on standard error
(`missive: no local copy yet (missive sync makes it)`) and answer as if it were empty: zeros,
`[]`, status success. A copy written by a later Missive than this one is an error.

### `missive status [--json]`

Each account's connection state, its last complete sync, whether a sync is running, and the
number of messages in the copy; and whether the daemon runs.

```json
{
  "daemon": "running",
  "accounts": [
    { "id": "alice", "state": "ready", "syncing": false, "last_sync": 1792220640, "messages": 8 },
    { "id": "bob", "state": null, "syncing": false, "last_sync": null, "messages": 0 }
  ]
}
```

| Field | Type | |
|---|---|---|
| `daemon` | `"running"` or `"stopped"` | whether a daemon holds `$XDG_RUNTIME_DIR/missive/daemon.pid` locked |
| `accounts[].id` | string | the identifier |
| `accounts[].state` | string or `null` | the connection state the daemon recorded last (`ready`, `syncing`, `no-login`, `auth-refused`, `certificate-refused`, `unreachable`: see `docs/protocol.md`); `null` when the copy knows none to report |
| `accounts[].syncing` | boolean | a sync is running now: the state is `syncing` **and** the daemon runs |
| `accounts[].last_sync` | number or `null` | when the last complete sync ended, in seconds since the epoch; `null` if none has |
| `accounts[].messages` | number | the messages in the copy, in every folder |

`state` is `null` in two cases: the account is not in the copy yet, and the copy says `syncing`
although no daemon runs. The second one is what a daemon killed in the middle of a sync leaves
behind: the state is a record, not a measure, so it is only believed while a daemon is alive to
have written it. With the daemon stopped the other states are what was last known (`ready` is
"was up to date at `last_sync`"). An account that was never synced has `state` `null` and
`last_sync` `null`.

For people, the same on lines:

```
daemon: running
alice	ready	last sync 2026-10-17 07:04 UTC	8 messages
bob	never synced	0 messages
```

A line reads `unknown (the daemon is not running)` in the place of the state when `state` is
`null` for the second reason above.

### `missive unread [--json]`

The unread messages: those without the `\Seen` flag (the flag is matched as a whole word, in any
case), and not marked `\Deleted`, which wait for an expunge and are counted as neither read nor
unread. By account, then by folder: the inbox first, then the other roles (drafts, sent,
archive, junk, trash), then the folders without a role, by name. Every folder of an account is
listed, even with no unread message. The `total` and an account's `unread` add up every folder,
the junk and trash folders included: a program that wants the inbox reads the folder whose `role`
is `inbox`.

```json
{
  "total": 6,
  "accounts": [
    {
      "id": "alice",
      "unread": 6,
      "folders": [
        { "id": 1, "name": "INBOX", "role": "inbox", "unread": 4 },
        { "id": 2, "name": "Sent", "role": "sent", "unread": 0 },
        { "id": 3, "name": "Projects", "role": null, "unread": 2 }
      ]
    }
  ]
}
```

| Field | Type | |
|---|---|---|
| `total` | number | the unread messages of all the accounts shown |
| `accounts[].id`, `unread` | string, number | |
| `folders[].id` | number | the folder's number in the copy, which a message's identifier names it by |
| `folders[].name` | string | the folder's name on the server (as IMAP spells it: modified UTF-7) |
| `folders[].role` | string or `null` | `inbox`, `sent`, `drafts`, `trash`, `junk` or `archive`; `null` for a folder of the user's own |
| `folders[].unread` | number | |

For people, each account with its count, the inbox and the folders that have unread messages
(by role, else by name), then the total:

```
alice: 6 unread
  inbox: 4
  Projects: 2
total: 6
```

### `missive search <text…> [--json] [--account <id>] [--folder <role>] [--limit <n>]`

The messages that hold **every** word given, in their sender, recipients (To and Cc), subject or
text, **newest first**. Capital letters and accents do not matter (`cafe` finds `Café`, `ELODIE`
finds `Élodie`).

- A word is what the index calls a word: a run of letters and digits. Anything else cuts words,
  so an address is its parts: `carol.sender@example.com` finds the same messages as
  `carol sender example com`, and `plan*` is the word `plan` (there is no search by prefix, and
  no search by phrase: the order of the words and their distance do not count).
- What is typed is words, never search syntax: `AND`, `OR`, `NOT`, `NEAR`, quotes, `*`, `col:`
  and `-` find messages that hold them or find nothing. A word that starts with a dash is given
  after `--`: `missive search -- -draft`.
- The text of a message is its text parts; of a message with only HTML, that HTML reduced to
  its text: tags, styles, scripts and the addresses links go to are not words.
- `--account` takes an identifier (an unknown one is an error); `--folder` a role, among
  `inbox`, `sent`, `drafts`, `trash`, `junk` and `archive`, since a folder of the user's own has
  none; `--limit` defaults to 20.
- A search that finds nothing prints `[]` (with `--json`) or nothing, and succeeds.
- A copy made before the search index existed (by an earlier build) is counted by `status` and
  `unread` but cannot be searched until the daemon has started once: `search` says so and fails.

```json
[
  {
    "id": "alice/1/1745112/4",
    "account": "alice",
    "folder": { "id": 1, "name": "INBOX", "role": "inbox" },
    "uidvalidity": 1745112,
    "uid": 4,
    "date": 1792220640,
    "subject": "Plan 4",
    "from": [{ "name": "Carol Sender", "address": "carol.sender@example.com" }],
    "to": [{ "name": "Dave Recipient", "address": "dave.recipient@example.net" }],
    "unread": true
  }
]
```

| Field | Type | |
|---|---|---|
| `id` | string | the message's identifier, below |
| `account` | string | the identifier of its account |
| `folder` | object | `id`, `name` and `role`, as in `unread` |
| `uidvalidity`, `uid` | number | the IMAP UID, and the UIDVALIDITY it belongs to (`null` before the folder has been examined, written `0` in `id`) |
| `date` | number or `null` | the message's `Date`, else the server's internal date, in seconds since the epoch |
| `subject` | string or `null` | |
| `from`, `to` | array | objects with `name` and `address`, each a string or `null` |
| `unread` | boolean | no `\Seen` flag |

The **identifier** is `<account>/<folder id>/<uidvalidity>/<uid>`, e.g. `alice/1/1745112/4`: an
account's identifier has no `/`, so it is read back unambiguously. The copy's own row number of a
message would be shorter, but it is lost when a folder is rebuilt after the server renumbered it
(UIDVALIDITY changed). With this identifier, whatever opens the message later can tell "this
message is gone" (the folder's UIDVALIDITY is the same and the UID is no longer there) from
"these UIDs now name other messages" (the folder's UIDVALIDITY is no longer the one in the
identifier), and can refuse to show the wrong message in the second case. The folder's number is
the copy's: it follows a folder through renumberings but not through an account being removed
and added again.

For people, one line per message, separated by tabs: the identifier, the date in UTC, the first
sender (its name, else its address) and the subject.

```
alice/1/1745112/4	2026-10-17 07:04 UTC	Carol Sender	Plan 4
```

#### The index

The index is an SQLite FTS5 table, `message_search`, added by the copy's schema version 2 and
filled by the daemon in the same transaction that writes a message (it leaves with the message,
too). It is *contentless*: it keeps the words and where they are, not the text, which is read
from the raw message when it is written, so no text is stored twice. A copy of version 1 is
indexed in one pass when the daemon first opens it, from the raw messages it holds. The
tokenizer is `unicode61 remove_diacritics 2`.
