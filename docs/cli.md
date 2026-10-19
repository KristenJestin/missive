# The command line

`missive …` drives Missive. What it prints for people may change; what it prints with `--json`
is a contract read by other programs: fields are only ever added, never renamed, retyped or
removed without a new major version.

Errors go to standard error, prefixed `missive: `, and the exit status is 1 (2 for a usage
error, 3 for `missive show` of a message that does not exist).

## Accounts

An account is named by its **identifier**: a short name made of letters, digits and
`. _ @ + -`, chosen with `--id` when it is added and the address otherwise (an address never
holds a `/`, and `account add` refuses an identifier that does: a message identifier is read
back by cutting it at its `/`). Commands take it to say which account they mean.

### `missive account add`

Adds an IMAP/SMTP account after logging in to both servers with it. Nothing is saved when a login
fails.

| Option | Value | Default |
|---|---|---|
| `--id` | the identifier | the address |
| `--name` | the display name, as in `Alice Example <alice@example.org>` | asked |
| `--address` | the address, plain (`name@example.org`, ASCII, no name or angle brackets): what the SMTP server gets in `MAIL FROM`, so it is checked as a recipient's is, before any login | asked |
| `--imap-host`, `--smtp-host` | the server | asked |
| `--imap-security`, `--smtp-security` | `tls` or `starttls` | `tls` |
| `--imap-port` | the port | 993 with `tls`, 143 with `starttls` |
| `--smtp-port` | the port | 465 with `tls`, 587 with `starttls` |
| `--user` | the login on both servers | the address |
| `--certificate <file.pem>` | trust exactly this server certificate for this account | the system's authorities |
| `--password-stdin` | read the password from standard input, one line | asked without echo |
| `--server-saves-sent` | the SMTP server keeps a copy of what it sends in the Sent folder itself (Proton Mail Bridge does): Missive appends none | Missive appends the copy |

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
    "server_saves_sent": false,
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
| `server_saves_sent` | boolean | whether the SMTP server keeps the copy of what it sends (`--server-saves-sent`) |
| `certificate` | boolean | whether a certificate is pinned for the account |

Without accounts, it prints `[]`. The array is printed on one line; the layout above is only for
reading.

An account whose `account.toml` cannot be read or is invalid does not stop the others: it is
left out, and one line on standard error names its folder and the kind of error (never the
content of the file). Standard output stays valid, and the exit status is still success.

### `missive account remove <id> [--force]`

Removes the account's keyring entry, then its settings and certificate. While the account's
outbox holds messages not sent (`queued`, `claimed`, `sending`, `failed` or `uncertain`; see
`missive outbox`), it is refused, saying how many, since they would leave with its rows: resend
or discard them first, or give `--force`. A message whose copy is still to be appended to Sent
was sent, and does not count. The keyring goes first: if it is unavailable, the account stays
whole and the command can be run again. Its mail leaves the local copy the next time the daemon
starts or reloads the configuration (a `sync` does): the copy only, nothing is sent to the
server. Until then the reading commands skip the account.
Removing the last account leaves its rows in the copy until another account is configured: a
configuration folder that is missing or empty is more likely a wrong or unmounted folder than a
choice, and must not wipe the copy.

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
stops reading, as `missive watch | head -1` does), noticed at once and not at the next line it
would write: neither is an error, and the exit status is success. (Standard output sent to a
regular file has no reader to lose: the stream goes on.) The status is 1 when the daemon refuses the subscription (it speaks another protocol
version, say) or the socket cannot be reached for another reason than the daemon's absence.

## The local copy

Missive keeps a copy of each account in `$XDG_DATA_HOME/missive/copy.sqlite` (or
`~/.local/share/missive/…`): folders, messages, flags and each account's connection state. Only
the daemon talks to the servers and writes the copy. Its sync only reads; it changes something on
a server only when the user asks for it (`mark`, `move`, `delete`: see
[Changing messages](#changing-messages)).

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

`missive status`, `missive unread`, `missive search`, `missive list` and `missive show` answer from
the local copy alone: they
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

Each account's connection state, its last complete sync, whether a sync is running, the
number of messages in the copy, what its outbox holds and the drafts the copy keeps; and whether
the daemon runs.

```json
{
  "daemon": "running",
  "accounts": [
    { "id": "alice", "state": "ready", "syncing": false, "last_sync": 1792220640, "messages": 8,
      "skipped": [{ "folder": "sent", "kind": "refused" }],
      "outbox": [
        { "id": 3, "state": "failed", "message_id": "9c1f…@example.org", "queued_at": 1792220100,
          "attempts": 0, "last_error": "the server answered 550 to RCPT TO" }
      ],
      "drafts": [
        { "id": "4b2e…@example.org", "state": "waiting", "saved_at": 1792220300, "attempts": 0,
          "last_error": null }
      ],
      "half_moved": [
        { "message": "alice/1/1745112/4", "copy": "alice/2/1745113/9", "folder": "inbox",
          "target": "archive", "maybe": false }
      ],
      "deleted_left": [] },
    { "id": "bob", "state": null, "syncing": false, "last_sync": null, "messages": 0,
      "skipped": [], "outbox": [], "drafts": [], "half_moved": [], "deleted_left": [] }
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
| `accounts[].skipped` | array | the folders the last complete pass left as they were, `[]` when none (below) |
| `accounts[].skipped[].folder` | string | the folder's role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`), else its name |
| `accounts[].skipped[].kind` | string | why: `refused` (the server answered `NO` to a command of the sync, for example `EXAMINE`) or `unusable` (it answered `OK` with something no sync can use: no `UIDVALIDITY` or `EXISTS`, a listing shorter than the folder) |
| `accounts[].outbox` | array | the messages of the outbox, oldest first (see [Sending](#sending)) |
| `accounts[].outbox[].id` | number | its number in the outbox, as `compose --json` printed it |
| `accounts[].outbox[].state` | string | `queued`, `claimed`, `sending`, `sent` (its copy in Sent is still to be appended), `failed` or `uncertain` |
| `accounts[].outbox[].message_id` | string | its `Message-ID`, without angle brackets |
| `accounts[].outbox[].queued_at` | number | when it was written, in seconds since the epoch |
| `accounts[].outbox[].attempts` | number | the tries that failed before the end of its data (tried again later) |
| `accounts[].outbox[].last_error` | string or `null` | the last error, for people |
| `accounts[].outbox[].copy_search_failures` | number | for a `sent` message: how many times looking for its copy in Sent failed (the connection was lost, or the server refused the search); the copy is given up at five, with a `sent-copy-failed` event. `0` for any other state |
| `accounts[].outbox[].stuck` | boolean | the message has been `claimed` for more than an hour: the daemon's worker could not write the outbox to put it back in the queue (five tries), and it stays so until the daemon starts again, which queues it again. `false` for any other state, for a claim that is being tried, and for a claim without a time (made by a daemon older than the claim time) |
| `accounts[].drafts` | array | the drafts the copy keeps until the server holds them, oldest first (see [Drafts](#drafts)) |
| `accounts[].drafts[].id` | string | the draft's identifier (its `Message-ID`, without angle brackets), as `compose --json` printed it |
| `accounts[].drafts[].state` | string | `waiting` (to write to the Drafts folder), `removing` (its message was sent: to remove from the Drafts folder) or `no-drafts-folder` (the account has no Drafts folder: the draft stays in the copy) |
| `accounts[].drafts[].saved_at` | number | when its version was saved, in seconds since the epoch |
| `accounts[].drafts[].attempts` | number | the tries that failed (tried again at the next pass) |
| `accounts[].drafts[].last_error` | string or `null` | the last kind of error |
| `accounts[].half_moved` | array | the messages a move left in two folders: the server refused to remove them after it had copied them (a server without `MOVE`; see [Changing messages](#changing-messages)), oldest first, `[]` when none |
| `accounts[].half_moved[].message` | string | the message's identifier, in the folder it was to leave |
| `accounts[].half_moved[].copy` | string or `null` | the identifier of its copy in the target; `null` when the server did not tell its UID (the copy is in the target all the same) |
| `accounts[].half_moved[].folder`, `.target` | string | the folder it was to leave and its target: their roles, else their names |
| `accounts[].half_moved[].maybe` | boolean | `true` when the message may only be in both: the move was refused while the server's answer to its copy was lost, so whether the target has it is not known (`copy` is then `null`); `false` in a copy written before this field existed |
| `accounts[].deleted_left` | array of strings | the messages an action refused for good, or the removal of a draft's older versions, left marked `\Deleted` on the server, by their identifiers: Missive set the flag itself and could not take it back (the connection was lost, or the server refused). It takes it back at the next connection, or lets it go once the folder was renumbered or deleted on the server (no message is left to take it from); `[]` when none |

`ready` does not mean the copy is complete: a folder the server refuses is skipped, the others are
synced, and the account is `ready` all the same. `skipped` is what keeps that from being silent. It
is replaced at the end of every complete pass: a folder that syncs again leaves it, and the folder
that was skipped keeps its mail in the copy as it was (nothing is removed from it). It names the
kind of error only, never what the server said. Like the rest of `status`, it is read from the
local copy: a copy written before this field existed says `[]`.

`state` is `null` in two cases: the account is not in the copy yet, and the copy says `syncing`
although no daemon runs. The second one is what a daemon killed in the middle of a sync leaves
behind: the state is a record, not a measure, so it is only believed while a daemon is alive to
have written it. With the daemon stopped the other states are what was last known (`ready` is
"was up to date at `last_sync`"). An account that was never synced has `state` `null` and
`last_sync` `null`.

For people, the same on lines:

```
daemon: running
alice	ready	last sync 2026-10-17 07:04 UTC	8 messages	skipped: sent (refused)
bob	never synced	0 messages
```

A line reads `unknown (the daemon is not running)` in the place of the state when `state` is
`null` for the second reason above. An account whose outbox holds messages has a line more,
`	outbox: 1 queued, 1 failed`, and one line for each failed or uncertain message, with why, for
each stuck one (`stuck` above) and for each sent one whose copy could not be looked for in Sent
(`copy_search_failures` above). An account whose drafts wait in the copy has a line more too,
`	drafts: 1 to write to the Drafts folder` (or `kept in the local copy: the account has no
Drafts folder`), and one line for each draft whose last try failed. Each message of `half_moved`
has a line: `	half moved: alice/1/1745112/4 is in both inbox and archive, since the server refused to
remove it after copying it; delete or move one of the two`, or, with `maybe`, `	half moved:
alice/1/1745112/4 may be in both inbox and archive, since the server's answer to its copy was lost
and the move was then refused; look in archive, and delete or move one of the two if it is there`.
A message leaves `half_moved` once one
of the two is gone from the copy (the user deleted or moved it, or another client did); a copy
written before this field existed says `[]`. Each message of `deleted_left` has a line too:
`	still marked \Deleted: alice/1/1745112/4, by an action that was then refused or the removal
of a draft's older version; Missive takes the flag back at the next connection`.

### `missive unread [--json]`

The unread messages: those without the `\Seen` flag (the flag is matched as a whole word, in any
case), and not marked `\Deleted`, which wait for an expunge and are counted as neither read nor
unread. By account, then by folder: the inbox first, then the other roles (drafts, sent,
archive, junk, trash), then the folders without a role, by name. Every folder of an account is
listed, even with no unread message. The `total` and an account's `unread` add up every folder
but the junk and trash ones, which are listed with their own count and left out of the sums:
nobody wants a badge for spam. The `unread` event of `missive watch` counts the same way. A
program that wants the inbox alone reads the folder whose `role` is `inbox`.

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
| `total` | number | the unread messages of all the accounts shown, but those in a junk or trash folder |
| `accounts[].id` | string | |
| `accounts[].unread` | number | as `total`, for this account |
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

### `missive list [--json] [--account <id>] [--folder <role or name>] [--unread] [--limit <n>]`

The messages of a folder, **newest first** (the message's `Date`, else the server's internal
date). By default the inbox of each account, merged into one list, newest first.

- `--folder` takes a role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`; capitals do not
  matter) or a folder's name as the server gives it (`unread` shows them, in modified UTF-7):
  the folders of the accounts shown that have that role *or* that name. A name or role that no
  folder of those accounts has is an error; an account that lacks the folder is not, while
  another has it. So `--folder sent` is every account's Sent folder, whatever its name there,
  and `--folder Projects` is that folder.
- `--unread` keeps the messages `unread` counts: without `\Seen`, and not marked `\Deleted`.
- `--account` takes an identifier (an unknown one is an error); `--limit` defaults to 50, over
  all the accounts together.
- A list that finds nothing prints `[]` (with `--json`) or nothing, and succeeds.
- A copy made before the attachment marker existed (by an earlier build) can be read by every
  other command but cannot be listed until the daemon has started once: `list` says so and
  fails.
- A draft saved and not yet on the server (see [Drafts](#drafts)) is listed in its account's
  Drafts folder at once: its `id` is the draft's identifier (its `Message-ID`, which
  `compose --draft` takes), its `uid` is `0` and its `uidvalidity` `null`, and the older
  versions of it the folder holds are not listed.

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
    "unread": true,
    "flagged": false,
    "answered": false,
    "size": 2214,
    "has_attachments": true
  }
]
```

Each message has the fields of a `search` hit, with the same meaning (`id`, `account`, `folder`,
`uidvalidity`, `uid`, `date`, `subject`, `from`, `to`, `unread`), and:

| Field | Type | |
|---|---|---|
| `flagged` | boolean | the `\Flagged` flag |
| `answered` | boolean | the `\Answered` flag |
| `size` | number | the size the server gives, in bytes |
| `has_attachments` | boolean | the message has something a person would call an attachment (rule below) |

The rule of `has_attachments`: a part counts when it is a file, or a message attached as a file
(`Content-Disposition: attachment`, or a file name). It does not count when it is

- an image the HTML shows inside the text: a part with a `Content-ID` that the HTML names as
  `cid:<that id>`, or an image marked `inline` inside a `multipart/related`;
- a message nested without being attached (the copy of the original in a bounce, say);
- one of the text and HTML versions of the message.

`has_attachments` is decided when the daemon writes the message to the copy (and, for the
messages a copy already holds, when it first opens the copy under this version), and kept in
the copy: a list of a big folder does not parse a single message. A message written under an
earlier rule keeps its mark until its folder is rebuilt. The `attachments` of `show` lists every
part that is not a body, inline images included, with the numbers that name them.

For people, one line per message, separated by tabs: the identifier, four flags (`u` unread, `f`
flagged, `a` answered, `@` has attachments; a dot for each that is not set), the date in UTC, the
first sender (its name, else its address) and the subject.

```
alice/1/1745112/4	u..@	2026-10-17 07:04 UTC	Carol Sender	Plan 4
```

### `missive show <message id> [--json | --raw]`

One message, by the identifier `search` and `list` print (`<account>/<folder id>/<uidvalidity>/<uid>`).

A draft the server does not hold yet is read too, named by its `Message-ID` (what `compose` prints
and `list --folder drafts` shows as the `id` of a draft waiting for the server, with `uid` `0` and
`uidvalidity` `null`), with or without angle brackets: the version waiting in the local copy, else
the message waiting in the outbox. It is shown as a message of the account's Drafts folder; `--raw`
gives the version as Missive keeps it (its `X-Missive-Draft` and `Bcc` headers included). Once the
server holds the draft, its identifier is the usual one, and the `Message-ID` finds nothing more.

```json
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
  "unread": true,
  "flagged": false,
  "answered": false,
  "size": 2214,
  "cc": [{ "name": "Erin Copy", "address": "erin.copy@example.org" }],
  "reply_to": [],
  "message_id": "4@example.com",
  "text": "The report is attached.",
  "html": "<p>The report is <b>attached</b>.</p>",
  "attachments": [
    { "name": "report.pdf", "media_type": "application/pdf", "size": 1532, "part": 2 }
  ]
}
```

It has the fields of a `list` message (but `has_attachments`: the list of `attachments` says it, with no distinction of inline images),
and:

| Field | Type | |
|---|---|---|
| `cc`, `reply_to` | array | objects with `name` and `address`, each a string or `null`, as `from` |
| `message_id` | string or `null` | the `Message-ID` header, without its angle brackets |
| `text` | string or `null` | the message's text parts, one after the other, decoded to UTF-8 from the charset they declare; `null` when it has none |
| `html` | string or `null` | its HTML parts **as the sender wrote them** (decoded to UTF-8): tags, styles, scripts and remote addresses included. Nothing interprets them; whoever shows them must make them safe and must not fetch what they point to without being allowed to |
| `attachments` | array | `name` (string or `null`), `media_type`, `size` in bytes once decoded, and `part`: the part's number in the message (0 is the message itself, parts are numbered in the order they appear), which is how a later command will name the attachment |

A message with only HTML has `text` `null`, and one in two versions has both: the JSON does not
make a text out of HTML. A message that cannot be read, or only in part, gives what can be read
and `null`, `[]` or empty arrays for the rest; it is never an error. Its headers are those that
`list` shows.

Without `--json`, the message is written for a terminal: its headers (`From`, `To`, `Cc`,
`Reply-To`, `Date` in UTC, `Subject`, `Message-ID` and `Attachments`, each when the message has
it, but the subject, which reads `(no subject)` when there is none), an empty line, then its
text: the text part, else the HTML reduced to its text (markup, styles and scripts left out; the
paragraphs and line breaks stay). A link keeps its target after its words, `the site
[https://example.org/]`, for `http`, `https` and `mailto` targets only (none is fetched, and a
link that is its own text is written once); other targets are only in `html`. The HTML is never
interpreted. Control characters (the escape sequences that could move a terminal's cursor or
change its title) are left out of the headers and the text, and so are the Unicode format
characters that reorder or hide text (a right-to-left override, the zero-width space, the byte
order mark…); the zero-width joiner and non-joiner stay, as emoji and some scripts need them.

```
From: Carol Sender <carol.sender@example.com>
To: Dave Recipient <dave.recipient@example.net>
Date: 2026-10-17 07:04 UTC
Subject: Plan 4
Message-ID: 4@example.com
Attachments: report.pdf (application/pdf, 1532 bytes)

The report is attached.
```

`--raw` writes the message exactly as the server sent it (every byte as stored) on standard
output, and nothing else; it cannot be combined with `--json`.

**Not found.** An identifier that names no message in the copy is an error with exit status 3
(not 1: a program can tell "that message is not there" from "the copy could not be read"), and
nothing on standard output. It says which of three it is:

- it is not an identifier (not `<account>/<folder>/<uidvalidity>/<uid>`, with each number written
  without sign or leading zeros);
- the account, the folder or the UID is not in the copy: the message has been expunged, or never
  was, or its account was removed from the configuration (such mail is kept in the copy but is
  neither listed nor shown);
- the folder is in the copy under another UIDVALIDITY (*renumbered*): the server gave its UIDs
  anew, and the UID of the identifier may now be another message, which is not shown. `list`
  gives the current identifiers.

With no local copy yet, `show` says so (as the other commands do) and exits with 3.

### `missive open <message id> [--original]`

"View original": the message, cleaned, in the system's default browser. It is the fallback for
any message a window cannot draw well. Until the window exists it is the only view, so
`--original` changes nothing yet; a program that wants the browser passes it already.

The message is read from the local copy (as `show` does: the same identifiers, a draft the server
does not hold yet named by its `Message-ID` included, the same exit status 3 when the identifier
names no message), cleaned, and written to a new file
`$XDG_RUNTIME_DIR/missive/original/<random>.html`, readable by the user only (mode 0600, in a
folder of mode 0700). Without `XDG_RUNTIME_DIR` it refuses: the page is never written to a
shared folder such as `/tmp`. Pages older than an hour are removed at the next `open`. On success
the path of the page is printed on standard output, and nothing of the message is printed.

The page is opened with the system's default handler, never a named browser: the desktop
portal (`org.freedesktop.portal.OpenURI.OpenFile`, given the file as a descriptor, which a
sandboxed browser can read), else `xdg-open`. `open` does not wait for the browser: it returns
once the portal has taken the page, or once `xdg-open` is started. A portal that says it could
not open the page leaves it to `xdg-open`; one that does not answer within two seconds is taken
to be asking the user. `xdg-open` gets exactly the environment `missive` received, with nothing
added.

**What the cleaning keeps.** Everything is kept by allow-list; what is not listed goes.

- *HTML*: the elements of a document's content (text, lists, tables, images, links, `font`,
  `center`…) and their presentation attributes (`style`, `class`, `id`, `align`, `valign`,
  `bgcolor`, `width`, `height`, `dir`, table spacing, `font` colour, face and size). Scripts,
  event handler attributes (`onclick`…), forms, frames, objects, embeds, SVG and MathML, `<meta>`
  (refresh included), `<base>`, `<link>` (external stylesheets and fonts), the `<title>` and HTML
  comments are removed.
- *Links* (`<a href>`) stay clickable for `http`, `https` and `mailto`, and for anchors of the
  page itself (`#…`); every other address (`javascript:`, `data:`, a relative path, which would
  point into the user's files) is removed. Links get `rel="noopener noreferrer"`.
- *Images* keep their source only when it is a part of the message: a `cid:` image (PNG, GIF,
  JPEG or WebP) is inlined as a `data:` URL. A remote image, a tracking pixel among them, never
  loads; its `alt` text stays.
- *Styles* (`<style>` blocks, gathered at the top of the page, and `style` attributes): plain
  rules and `@media` rules, with declarations of these properties and their families: `align-*`,
  `aspect-ratio`, `background`, `border`, `box-shadow`, `box-sizing`, `caption-side`, `clear`,
  `color`, `column-gap`, `direction`, `display`, `empty-cells`, `flex`, `float`, `font`, `gap`,
  `grid`, `height`, `hyphens`, `justify-*`, `letter-spacing`, `line-height`, `list-style`,
  `margin`, `max-height`, `max-width`, `min-height`, `min-width`, `object-fit`, `opacity`,
  `order`, `outline`, `overflow`, `padding`, `row-gap`, `tab-size`, `table-layout`, `text-*`,
  `unicode-bidi`, `vertical-align`, `visibility`, `white-space`, `width`, `word-break`,
  `word-spacing`, `word-wrap`, `-webkit-text-size-adjust`, `-ms-text-size-adjust`. A value may
  call only `rgb`, `rgba`, `hsl`, `hsla`, `hwb`, `calc`, `min`, `max`, `clamp`, the gradients,
  `repeat`, `minmax`, `fit-content`, and `url()` for a `data:` image (PNG, GIF, JPEG, WebP in
  base64). Every other at-rule (`@import`, `@font-face`…), property (`position`, `content`,
  `cursor`, `behavior`, `-moz-binding`…) and function (`expression()`, `image-set()`, `url()` of
  anything else…) is removed. CSS escapes (`\75rl(`) and comments are removed first, so that
  nothing can spell a removed word.
- A Content-Security-Policy comes first in the page, as a second line of defence: `default-src
  'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'`.

A message with only text is opened as a plain-text page: the text escaped in a `<pre>`, never
interpreted. Allowing remote images, for a message or a sender, does not exist yet.

## Changing messages

`missive mark`, `missive move` and `missive delete` change messages on the server. They name
messages by the identifiers `search` and `list` print, as many as wanted, of one account or
several. They ask the daemon (starting one when none answers, as `sync` does), which:

1. records the action in the local copy, in a queue, together with its effect: the message reads
   as seen, shows in its new folder, or is gone, in `list`, `search`, `show` and `unread` at once;
2. answers: the command returns as soon as the action is recorded, whether the server can be
   reached or not (with `--wait`, once the server applied or refused it: see below);
3. applies it on the server when the account is connected, in the order the actions were asked,
   and reconciles the copy with what the server answered. Offline, the queue waits for the
   connection; a daemon stopped or killed in between applies it when it starts again.

`missive watch` tells what became of each action (an `action` event, `docs/events.md`): applied,
tried again later, or refused. A refused action is taken back from the copy: the message shows as
the server has it. A move the server refused after it had copied the messages (a server without
`MOVE`) leaves them in both folders: the event says so (`copied`), and `status` lists them
(`half_moved`) until the user deletes or moves one of the two; nothing is removed without the
user. One refused while the answer to its copy was lost may have: the event says so
(`maybe_copied`), and `status` lists them as a maybe.

While a move waits for the server, the message is shown in its new folder, but its identifier
still names the folder the server has it in; once moved, it has a new identifier there (its UID in
that folder), as it would if another client had moved it. An action asked on a message whose move
waits is applied after the move, to the message in its new folder: the server's `COPYUID` tells
its new UID there. An action that names messages that are moving and messages that are not is
recorded as one action for each, each with its number. When the server moved the message without
telling its new UID, the action is refused (`moved`): a UID found by other means could name
another message. On a server known to have no `UIDPLUS`, which never tells it, such an action is
refused when asked (`busy`).

### `missive mark <message id>… --read | --unread | --flagged | --unflagged [--wait [--timeout <seconds>]] [--json]`

Adds or removes the `\Seen` or the `\Flagged` flag.

### `missive move <message id>… --to <role or folder name> [--wait [--timeout <seconds>]] [--json]`

Moves the messages to another folder of their account: a role (`inbox`, `sent`, `drafts`,
`trash`, `junk`, `archive`) or a folder's name as the server gives it (`unread` shows them). A
role is looked for first, then a name. A message already in that folder is left as it is.

The server must have `MOVE`, or else `UIDPLUS` (the move is then a copy, followed by the removal
of exactly those messages). A server with neither refuses the move: removing a message there
would also remove every message another client marked deleted in the same folder. The daemon
records what the server can do at each connection: once it knows, such a move is refused when it
is asked, and nothing is recorded; before the account's first connection, it is recorded and
refused when it is tried.

### `missive delete <message id>… [--permanently] [--wait [--timeout <seconds>]] [--json]`

Without `--permanently`, moves the messages to their account's trash (the folder with the
`trash` role). A message already in the trash is refused, and so is a message of an account that
has no trash: the message says to use `--permanently`.

With `--permanently`, deletes them for good: only messages in the trash, or in an account that
has no trash. Anywhere else it is refused, before anything is recorded. The server must have
`UIDPLUS`, for the same reason as for a move.

### What they answer

For people, one line: `2 messages marked read; the server is told as soon as the account is
connected.`. With `--json`, one object:

```json
{"messages": ["alice/1/1745112/4", "alice/1/1745112/7"]}
```

`messages` holds the identifiers the action was recorded for, each once, in the order given.

#### `--wait`

With `--wait`, the command answers once the server applied or refused the actions, rather than
once they are recorded. The wait is bounded: it ends when an account of the request is offline
(`no-login`, `auth-refused`, `certificate-refused` or `unreachable`: its queue waits for the
connection, so waiting would only end at the timeout), when the daemon stops, or after
`--timeout` seconds (60 by default). An action left in the queue then is not lost: the daemon
applies it as it would without `--wait`.

For people, the line ends with what became of them: `1 message marked read; applied on the
server.`, `…; refused by the server: <why>.` or `…; still queued: the account is unreachable.`
With `--json`, the object has two fields more:

```json
{"messages": ["alice/1/1745112/4"], "outcome": "applied",
 "actions": [{"id": 12, "outcome": "applied"}]}
```

| Field | Type | |
|---|---|---|
| `outcome` | string | `applied` (every action), `refused` (one at least was refused) or `queued` (one at least is still in the queue, none refused) |
| `actions` | array | one per action recorded (one per folder of the messages), in the order of the answer |
| `actions[].id` | number | the action's number, as the `action` events give it (`docs/events.md`) |
| `actions[].outcome` | string | `applied`, `refused` or `queued` |
| `actions[].reason` | string | with `refused`: the `reason` of the `action` event; with `queued`: the state of the account that is offline (`unreachable`…), `timeout` or `gone` (the daemon stopped) |
| `actions[].message` | string | with `refused`: why, for people |
| `actions[].copied` | `true` | with a refused move that left the messages in two folders (`docs/events.md`) |
| `actions[].maybe_copied` | `true` | with a refused move that may have left the messages in two folders (`docs/events.md`) |

The exit status tells them apart: 0 when every action was applied, 5 when one was refused, 4 when
one is still queued (and none refused). The line or the object is printed in every case.

A request is checked whole before anything is recorded: when one identifier names no message, or
the action cannot apply to one of them, nothing is recorded and the command fails, saying why on
standard error. The exit status is 3 when an identifier names no message (as for `show`), 1 for
every other refusal: a folder that does not exist, `--permanently` outside the trash, a message
whose move waits, an account that is not configured, an action the server cannot apply.

## Sending

### `missive compose [--account <id>] [--draft <draft id> | --reply <message id> | --reply-all <message id> | --forward <message id>] [--to <address>]… [--cc <address>]… [--bcc <address>]… [--no-to] [--no-cc] [--no-bcc] [--subject <text>] [--body-file <path> | --body <text or ->] [--attach <path>]… [--no-attach] [--send] [--no-window] [--json]`

Writes a message: saved as a draft, or with `--send` sent. An address is `name@example.org` or
`Name <name@example.org>` (the name may be in any script; the address is ASCII). `--body -`
reads the text from standard input; `--body-file` and `--body -` read UTF-8 text. Without a body
the message has an empty text. The account is the only one configured, the one `--account`
names, or the one of the message identifier `--draft` names.

Each `--attach` file is read when the command runs, and goes into the message at once: changing
or removing the file afterwards changes nothing. Its media type is guessed from its name
(`application/octet-stream` when the name says nothing), and its name is written as RFC 2231
asks, so that any script survives.

The message is built by the daemon (with `mail-builder`): UTF-8 headers encoded as RFC 2047
asks, a `Message-ID` at the domain of the account's address, a `Date`, the text, then the
attachments. Bcc recipients are given to the server only: the message sent does not name them.

With `--send`, the command asks the daemon (starting one when none answers), which writes the
message to the account's **outbox** in the local copy, and answers: the command returns at once,
whether the server can be reached or not. The daemon then sends it over SMTP, with STARTTLS or
TLS and the account's pinned certificate, as soon as the server can be reached; a daemon stopped
or killed in between sends it when it starts again. For people, one line says the message is in
the outbox. With `--json`, one object:

```json
{"outbox": 3, "message_id": "9c1f…@example.org"}
```

Without `--send`, the message is saved as a **draft** (below), and needs no recipient. For people,
one line says the draft is saved, with its identifier. With `--json`, one object:

```json
{"id": "4b2e…@example.org"}
```

When a display is there (`WAYLAND_DISPLAY` or `DISPLAY` is set), the draft, once saved, is also
opened in the **compose window**: `missive-window compose --account <id> --draft <draft id>`,
the `missive-window` beside `missive`, else the one on the `PATH`, started on its own (the
command returns at once). The output is the same; a window that cannot start is said on standard
error, and the draft stays saved. Without a display, nothing is opened; **`--no-window`** opens
nothing even with one, for a script that only wants the draft saved.

#### Replying and forwarding

`--reply`, `--reply-all` and `--forward` take a message's identifier (as `search` and `list`
print it) and start a new message from that message of the local copy. They combine with the
other options (`--body`, `--attach`, `--send`, or a draft without `--send`), exclude one another
and `--draft`, and exit with 3 when the identifier names no message. The account is the one that
received the message, unless `--account` says otherwise; a reply all then leaves out the address
of the account that sends it, not the one of the account that received the message.

What is worked out from the message is replaced, field by field, by what is given: `--to`
replaces the recipients, `--cc` the copies of a reply all, `--subject` the subject. `--body` is
the text above the quote.

- **Reply** goes to the `Reply-To` of the message, else its `From`. **Reply all** also goes to the
  message's `To`, and its `Cc` becomes the reply's `Cc`, but the account's own address is left
  out (case does not matter) and no address is written twice. An address Missive cannot send to
  (not ASCII, say) in the message stops the reply, and `--to` can name the recipients instead.
- The subject gets one `Re: `: a subject that already starts with `Re:`, in any case, is kept as
  it is.
- The thread: `In-Reply-To` is the message's `Message-ID`; `References` is the message's own
  `References` (else its `In-Reply-To`) followed by that `Message-ID`, each identifier once.
  Beyond 20 identifiers it keeps the first and the latest 19. A message without a `Message-ID`
  gives no `In-Reply-To`, and its `References` go on as they were.
- The text of the message (its HTML reduced to text when it has none) is quoted below the
  `--body`, each line prefixed by `> `, under a line `On 2026-10-17 07:30 +0200, Alice Example
  wrote:` (the date in the offset the message carries, the same in every language; without a
  date or a name the line says what it can).
- After a reply is **sent** (the server took it), never before, the message gets `\Answered`:
  it is shown at once in the local copy, and the flag is applied on the server through the
  queue of actions, as `missive mark` does. A reply that is only saved as a draft marks nothing,
  and neither does one the server refuses. A message that was moved or deleted meanwhile is not
  marked. The flag goes to the account that owns the message, which wakes at once even when
  another account sends the reply. A reply or a forward saved as a draft keeps the link to its
  original (an `X-Missive-Answers` or `X-Missive-Forwards` header of the draft, which the message
  sent never carries): picked up later and sent, from the command line or the window, it marks the
  original all the same. Only the links of drafts Missive saved itself are believed (the copy records
  them): a draft of another client that carries these headers is edited and sent as any other, and
  marks nothing. The message answered must be in the local copy when the reply is saved
  or sent: an identifier of a message that is gone is refused with the error `original-gone` (exit
  status 3; the window drops the link and asks again), and one that is no identifier at all with
  `no-such-message`.
- A message that is odd is answered all the same, and one line on the error output says what was
  changed (counts and kinds, never what the message says): line breaks in its subject (an
  encoded subject can decode to some) become spaces, and the identifiers (`Message-ID`,
  `In-Reply-To`, `References`) that cannot be written back, because they hold a space or are
  longer than a line (900 bytes), are left out of the thread.
- **Forward** has a subject `Fwd: ` (a subject that already starts with `Fwd:` or `Fw:` is kept),
  no `In-Reply-To` and no `References`, and the message goes along as an attachment of type
  `message/rfc822`, named after its subject (`<subject>.eml`), byte for byte as the local copy
  keeps it (it is encoded in base64 on the wire, as every attachment is). `--to` names the
  recipients. After a forward is **sent**, the message gets the keyword `$Forwarded` (RFC 5788) the
  same way, through the queue of actions. A server whose folder keeps no keywords (no `\*` in its
  `PERMANENTFLAGS`) is not asked: the action is dropped and nothing is said, the copy forgets the
  flag it showed. A forward does not mark the message answered.

The exit status is 1 when the message cannot be written as asked: no recipient (with `--send`),
an address that is not one, a file that cannot be read, an account that is not configured. It is
3 when `--draft` names no draft, or `--reply`, `--reply-all` or `--forward` no message.

#### Drafts

A draft is named by its identifier: its `Message-ID`, without angle brackets, which every version
of the draft keeps (so that its versions are found again on the server, and a reply's thread
stays whole); each version gets a new `Date`. `--draft` takes it, or the identifier `list` prints
for a message of the Drafts folder (`missive list --folder drafts`), which is how a draft written
by another client is picked up. The fields given replace those of the draft; the others are
kept, its Bcc recipients and attachments included (`--attach` replaces every attachment).
`--no-to`, `--no-cc`, `--no-bcc` and `--no-attach` take the recipients of that kind, or every
attachment, **off** the draft: they need `--draft`, and cannot go with the option that gives the
field (`--no-cc --cc …` is refused; give `--cc` alone to replace the Cc recipients). A draft
cleared of every recipient is still a draft, and `--send` refuses it. With
`--send` the draft is sent as it is then, and it is removed from the Drafts folder **once the
server took the message**: a message the server refuses (`failed`) or that may not have arrived
(`uncertain`) keeps its draft. A draft whose message waits in the outbox cannot be changed or sent
again until it is sent, and a draft whose message was already sent (the Sent folder holds it, or
the outbox sent it or does not know whether the server took it) cannot be sent again: `--send`
refuses it with an error, so that a server without UIDPLUS, which keeps the sent draft's last
version in the Drafts folder, never makes the same message go twice. The changes given in the
request that `--send` refuses this way are saved first, as a **new draft** with a new identifier,
never as a version of the sent one (a later resend of an `uncertain` message would remove it with
the sent draft): the error says `already sent; your changes are saved as a new draft <id>`, and
nothing typed is lost with the refusal. A request that changes nothing saves nothing.

Missive only ever replaces or removes a draft it wrote itself: its drafts carry an
`X-Missive-Draft: 1` header (the message sent from one does not). A draft **another client
wrote** is never changed or removed: picking it up makes a new draft, Missive's own, with a new
identifier (which `--json` prints), and the original stays in the Drafts folder as it is, with
`--send` or without; one line on standard error says so, and it is yours to delete there. What is
picked up of a draft is its recipients, subject, text, attachments and the thread it answers
(`In-Reply-To` and `References`), and its HTML alternative, kept as it was, byte for byte (a draft
with only HTML gets its text made from it, beside the HTML; with several HTML parts, the first is
kept): a draft picked up and written again, or sent, still carries its HTML. Its other headers
(`Reply-To`…) are not kept, nor is an image the HTML draws from a part of the message (it stays an
attachment). The HTML says what the text says: when `--body` or `--body-file` gives **another
text**, the HTML part is dropped, so that the draft never says two things; it is dropped too when
`--no-attach` or `--attach` takes an attachment off while the HTML draws an image from a part of
the message (`cid:`), which would leave a broken image. One line on standard
error says so; the same text (line ends aside) keeps it.

The draft is saved in the local copy first, and the command returns at once, whether the server
can be reached or not: `list --folder drafts` shows it at once. The daemon then writes it to the
account's Drafts folder (the folder with the `drafts` role), as soon as the server can be
reached:

1. it looks for the versions the folder already holds, by `Message-ID` and Missive's own header
   (`UID SEARCH`), one from a connection cut before the server answered included (a
   `Message-ID` that is not ASCII is not searched for, the server would answer `BAD`: the
   versions the local copy holds stand for them);
2. it appends the new version, flagged `\Draft` and `\Seen`, waiting for the server's answer
   longer the larger the draft is (up to 15 minutes); the command counts as done only with the
   server's `OK`;
3. only then it removes the older versions, by their UIDs only (`UID STORE … +FLAGS.SILENT
   (\Deleted)` then `UID EXPUNGE …`): a message another client marked deleted stays, and when
   the `UID EXPUNGE` is refused only the `\Deleted` Missive set is taken back. A server
   without UIDPLUS has no `UID EXPUNGE`: the older versions are then left in place, and the
   `draft` event says so (`docs/events.md`). Missive never sends a bare `EXPUNGE`.

When the server refuses the new version, the draft stays in the copy, the older version stays
on the server, and it is tried again at the next pass (`status` shows it, the `draft` event
tells it). An account without a Drafts folder keeps its drafts in the copy, and `status` says
so: Missive never creates a folder on the server.

#### The outbox

A message stays in the outbox until its server took it, in one of these states:

| State | |
|---|---|
| `queued` | to send. A try that fails before the end of its data (a `4xx` answer, the server unreachable, the login or the certificate refused) leaves it here: the server took nothing, and it is tried again later, waiting longer each time (1 second, then twice as long, up to 5 minutes). Each message waits for its own time |
| `claimed` | the daemon is sending it, and has not yet written its end of data: the server cannot have taken it. A daemon killed then puts it back in `queued` when it starts again; so does one whose worker could not put it back itself (`status` says `stuck`) |
| `sending` | its end of data is being written |
| `sent` | the server took it (its `250` to the end of data); only its copy in Sent is left to append. Without one to append, the message leaves the outbox at once |
| `failed` | the server refused it for good (`5xx` to `MAIL FROM`, a `RCPT TO`, `DATA` or the end of data). It stays, shown by `status` and `missive outbox`, and is sent again only by `missive outbox resend` |
| `uncertain` | the connection was lost after its end of data and before the server's answer (or the daemon was killed then): the server may have taken it. It stays, shown by `status` and `missive outbox`, and is never sent again by itself: a message delivered twice is worse than a question. Only `missive outbox resend <n> --yes` sends it again |

Messages are sent one at a time, in the order they were written, each when its time has come: a
message that waits for a new try holds back none of the others. After a login the server
refused, or with no password in the keyring, the daemon waits longer each time (1 second, then
twice as long, up to 5 minutes) and a new message does not cut the wait short; a password fixed
meanwhile does, once `missive sync` tells the daemon to look again: the password is read again,
and if it is not the one that was refused, the messages are tried at once. `missive watch` tells
when a message is `sent`, `send-failed` or `send-uncertain` (`docs/events.md`).

#### `missive outbox [--json]`, `missive outbox resend <n> [--yes]`, `missive outbox discard <n>`

`missive outbox` lists every message of the outbox, of every account, oldest first, one per line:
its number, its account, its state, when it was written and its last error. It reads the local
copy only. With `--json`, standard output is one JSON array, on one line:

```json
[{"account": "alice", "outbox": 3, "state": "failed", "message_id": "9c1f…@example.org",
  "queued_at": 1790000000, "attempts": 0,
  "last_error": "the server answered 550 to RCPT TO"}]
```

| Field | Type | |
|---|---|---|
| `account` | string | the account's identifier |
| `outbox` | number | the message's number in the outbox, as `compose --json` printed it |
| `state` | string | as in the table above |
| `message_id` | string | its `Message-ID`, without angle brackets |
| `queued_at` | number | when it was written, in seconds since the epoch |
| `attempts` | number | the tries that failed before the end of its data |
| `last_error` | string or `null` | for people |

`missive outbox resend <n>` puts a `failed` message back in the queue, to send at once (it is sent
once, as any other). An `uncertain` message may have arrived already: resending it needs `--yes`,
and its recipients may then get it twice. `missive outbox discard <n>` removes a `failed` or
`uncertain` message for good. Both go through the daemon (starting it when none runs), are
refused for a message in another state, and exit with 3 when no message has that number.

#### The copy in Sent

Once the server took a message, Missive appends a copy to the account's Sent folder (the folder
with the `sent` role) over IMAP, flagged `\Seen`, unless the account was added with
`--server-saves-sent` (Proton Mail Bridge saves what it sends itself). The copy holds the Bcc
recipients, in a `Bcc:` header: it is the user's own record, as in every mail client; what the
recipients got never names them. A server that saves sent mail itself keeps what it was sent,
without them. An account without a Sent folder gets no copy (the `sent` event says so): Missive
never creates a folder. Appending is tried again when it fails, up to five times, then given up
with a `sent-copy-failed` event; every try after the first (an append whose answer was lost, or
one a killed daemon left) first looks for the message in Sent by its `Message-ID`. Two counted
tries (an append started, or a look in Sent that failed) are at least a minute apart: the syncs
in between count none, so that a server that refuses for a minute does not use the five up.

### `missive mailto <uri> [--account <id>] [--no-window] [--json]`

Composes from a `mailto:` link (RFC 6068), as the system's handler for them: the desktop entry
`missive-mailto.desktop` runs `missive mailto %u`. The link is saved as a **draft** (see
`missive compose`) and, when a display is there, the draft is then opened in the compose window,
as `compose` does; without a display, or with `--no-window`, nothing is opened. For people, one line gives
the draft's identifier and says where to find it (`missive list --folder drafts`); with `--json`,
one object, as `compose` prints:

```json
{"id": "4b2e…@example.org"}
```

The account is the only one configured, or the one `--account` names.

What is read from the link: the addresses of its path (several, separated by commas) and its
`to`, `cc` and `bcc` fields (the addresses of a field are added to the others), `subject`, `body`
and `in-reply-to` (the draft's `In-Reply-To`). Names of fields ignore case; percent-encoding is
decoded as UTF-8, and a `+` stays a plus sign. The first of a `subject`, `body` or `in-reply-to`
given twice is kept; the fragment (`#…`) is dropped.

A link comes from anywhere, so it is never trusted to do more than write a draft:

- every other field is **ignored**: `from` (the sender is the account's), `attach` and
  `attachment` (a link never adds a file), `reply-to`, `references`, any header at all;
- a `to`, `cc`, `bcc`, `subject` or `in-reply-to` that holds a line break, or another control
  character, once decoded (`%0D%0ABcc:…`) is **refused**, not repaired: it would start a header
  the link may not set. A line break in the `body` is only text;
- a link is refused when it is longer than 32 KiB, is not a `mailto:` link, has an invalid
  percent-encoding (or one that does not decode to UTF-8), an address Missive cannot send to
  (ASCII only, as for `compose`), or an `in-reply-to` that is not one message identifier.

Nothing is saved from a refused link: the exit status is 1, with the reason on standard error.
The recipients are not required (`mailto:?subject=Hello` is a draft without one).

#### Being the default handler

The release archive holds `missive-mailto.desktop` (`MimeType=x-scheme-handler/mailto;`,
`NoDisplay=true`: it is not in the menus). `install.sh` copies it into
`${XDG_DATA_HOME:-~/.local/share}/applications`, with `Exec` naming the installed program by its
full path, and prints the command that makes it the default, which it never runs itself:

```
xdg-mime default missive-mailto.desktop x-scheme-handler/mailto
```
