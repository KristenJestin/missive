# The daemon's socket

`missive daemon` answers on a Unix socket. `missive sync` speaks to it, and so can any program of
the user: what is written here is a contract. Fields are only ever added, never renamed, retyped
or removed without a new protocol version.

## Where

| File | |
|---|---|
| `$XDG_RUNTIME_DIR/missive/daemon.sock` | the socket |
| `$XDG_RUNTIME_DIR/missive/daemon.pid` | the running daemon's pid; empty once it stopped cleanly |

There is one daemon per user. It holds `daemon.pid` locked for as long as it runs, so a second
daemon refuses to start (`a daemon is already running (pid …)`), and a daemon that was killed
leaves a socket the next one removes. The folder is private to the user (`0700`); without
`XDG_RUNTIME_DIR` the daemon does not start.

The daemon stops cleanly on SIGTERM or SIGINT: each account logs out, the socket is removed and
the pid file emptied. `kill $(cat "$XDG_RUNTIME_DIR/missive/daemon.pid")` stops it.

## Lines

A client writes requests and reads answers, each one JSON object on one line ending with `\n`.
One connection may carry several requests; they are answered in order. Every request names the
protocol version it was written for:

```json
{"protocol": 1, "request": "sync", "account": "alice"}
```

Every answer carries `protocol`, the request it answers in `answer` (`null` when the request could
not be read), and `ok`:

```json
{"protocol": 1, "answer": "sync", "ok": true, "accounts": [{"id": "alice", "state": "ready"}]}
```

A line the daemon writes with an `answer` field is an answer. A line with an `event` field is an
event, written on a connection that sent `subscribe`: see below, and `docs/events.md`.

## Errors

An answer with `"ok": false` says why in `error`, for programs, and in `message`, for people:

```json
{"protocol": 1, "answer": "sync", "ok": false, "error": "no-such-account", "message": "no account nobody (missive account list shows them)"}
```

| `error` | |
|---|---|
| `bad-request` | the line is not JSON, has no `protocol`, or a field has the wrong type |
| `unsupported-protocol` | the daemon speaks another protocol version (`message` says which) |
| `unknown-request` | the daemon knows no such request |
| `no-such-account` | the account named is not in the configuration |
| `no-such-message` | an identifier names no message in the local copy (`message` says which, and why), or a `resend` or `discard` names no message of the outbox |
| `no-such-folder` | the folder a `move` names is not one of the account's |
| `no-trash` | a `delete` without `permanently`, in an account that has no trash |
| `in-trash` | a `delete` without `permanently` of a message already in the trash |
| `not-in-trash` | a `delete` with `permanently` of a message outside the trash, in an account that has one |
| `busy` | a message whose move waits for the server: it takes no other action until then; or a `draft` naming a draft whose message waits in the outbox |
| `uncertain` | a `resend` without `yes` of an `uncertain` message: the server may have taken it already |
| `not-failed` | a `resend` or a `discard` of a message that is neither `failed` nor `uncertain` (`message` says its state) |
| `bad-request` (`send`, `draft`) | also: the message cannot be sent as written (`message` says why: no recipient, an address that is not one…) |
| `failed` | the daemon could not do it: it is stopping, an account's sync could not write the local copy (`message` names the account), the local copy could not be written, or the connection is already subscribed |

## Requests

### `sync`

Syncs every account now, or the one named by `account` (an identifier, as `missive account list`
shows it). The answer comes when that sync has ended, one way or another: `accounts` gives each
account's connection state at that point, in the order of their identifiers.

| `state` | |
|---|---|
| `ready` | the copy is up to date with the server, but for the folders the pass skipped (`skipped` in `status` and `sync-ended`) |
| `syncing` | a sync is running |
| `no-login` | the keyring holds no password for the account (or the keyring could not be read) |
| `auth-refused` | the server refused the user name or password; asking for a sync again is what tries again |
| `certificate-refused` | the server's certificate is trusted neither by the system nor by the certificate imported for the account |
| `unreachable` | the server cannot be reached now; the daemon tries again by itself, waiting longer each time (from 1 second up to 5 minutes) |

The daemon reads the configuration again on each `sync`: an account added since it started is
synced, a removed one is let go.

### `mark`, `move`, `delete`

Change messages on the server, named by the identifiers `missive search` and `list` print
(`<account>/<folder id>/<uidvalidity>/<uid>`), of one account or several:

```json
{"protocol": 1, "request": "mark", "messages": ["alice/1/1745112/4"], "as": "read"}
{"protocol": 1, "request": "move", "messages": ["alice/1/1745112/4"], "to": "archive"}
{"protocol": 1, "request": "delete", "messages": ["alice/1/1745112/4"], "permanently": false}
```

| Field | | |
|---|---|---|
| `messages` | array of strings | at least one |
| `as` (`mark`) | string | `read`, `unread`, `flagged` or `unflagged` |
| `to` (`move`) | string | a role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`), looked for first, or a folder's name as the server gives it |
| `permanently` (`delete`) | boolean, `false` when absent | delete for good, rather than move to the trash: only for messages in the trash, or of an account that has none |

The request is checked whole against the local copy, then recorded in its queue with its effect on
the copy, in one transaction, and answered: nothing waits for the server, which may be
unreachable. The answer gives the identifiers recorded, each once, in the order of the request:

```json
{"protocol": 1, "answer": "mark", "ok": true, "messages": ["alice/1/1745112/4"]}
```

A request that cannot apply to one of its messages records nothing, and is answered with one of
the errors above. The daemon then applies the queue on the server, in order, when the account is
connected; the `action` events (`docs/events.md`) tell what became of each action.

### `send`

Writes a message into the account's outbox, to be sent as soon as its SMTP server can be
reached:

```json
{"protocol": 1, "request": "send", "account": "alice", "to": ["Zoé <zoe@example.com>"],
 "cc": [], "bcc": ["carl@example.net"], "subject": "Les comptes", "body": "Bonjour,\r\n…",
 "attachments": [{"name": "comptes.pdf", "data": "JVBERi0xLjQK…"}]}
```

| Field | | |
|---|---|---|
| `account` | string | the account's identifier |
| `to`, `cc`, `bcc` | arrays of strings, empty when absent | `name@example.org` or `Name <name@example.org>`; at least one recipient in all |
| `subject` | string, absent for none | |
| `body` | string, empty when absent | the text |
| `attachments` | array, empty when absent | each a file's `name` (no folder) and its content, `data`, in base64 |
| `in_reply_to` | string, absent for none | the `Message-ID`, without angle brackets, of the message this one answers, written as its `In-Reply-To` header |
| `references` | array of strings, empty when absent | the thread's `Message-ID`s without angle brackets, written as the `References` header |
| `answers` | string, absent for none | the identifier of the message this one is a reply to: once the server took the reply (never before), that message is marked answered (`\Answered`), through the queue of actions, and the `action` event `answered` tells it. A message gone since is not an error |

The daemon builds the message (see `missive compose` in `docs/cli.md`), writes it to the outbox
in the local copy, and answers: nothing waits for the server.

```json
{"protocol": 1, "answer": "send", "ok": true, "outbox": 3, "message_id": "9c1f…@example.org"}
```

`outbox` is the message's number in the outbox, which the `sent`, `send-failed` and
`send-uncertain` events (`docs/events.md`) name it by; `message_id` is its `Message-ID`, without
angle brackets.

### `resend`, `discard`

Act on a message the outbox keeps because it was not sent: `failed` (the server refused it for
good) or `uncertain` (the server may have taken it). `resend` puts it back in the queue, to send
at once; `discard` removes it from the outbox, for good. Nothing else ever sends a `failed` or
`uncertain` message again.

```json
{"protocol": 1, "request": "resend", "outbox": 3, "yes": true}
{"protocol": 1, "request": "discard", "outbox": 3}
```

| Field | | |
|---|---|---|
| `outbox` | number | the message's number in the outbox, as `send` answered it |
| `yes` (`resend` only) | boolean, `false` when absent | resend an `uncertain` message: its recipients may then get it twice. Without it, an `uncertain` message is answered `uncertain` and stays as it is |

```json
{"protocol": 1, "answer": "resend", "ok": true, "outbox": 3}
```

A message resent is sent as any other, once, under its number and its `Message-ID`; the `sent`,
`send-failed` and `send-uncertain` events tell what became of it.

### `draft`

Saves a draft: a new one, or a new version of the draft `draft` names. With `send`, sends it too.

```json
{"protocol": 1, "request": "draft", "account": "alice", "draft": "4b2e…@example.org",
 "subject": "Les comptes, version 2"}
```

| Field | | |
|---|---|---|
| `account` | string | the account's identifier |
| `draft` | string, absent for a new draft | the draft to pick up again: its identifier (its `Message-ID`), or the identifier of a message of the account's Drafts folder, as `list` prints it |
| `to`, `cc`, `bcc`, `subject`, `body`, `attachments` | as for `send`, each absent to keep the draft's own | the fields that replace the draft's; for a new draft, those absent are empty. No recipient is needed |
| `in_reply_to` | string, absent to keep the draft's own | the `Message-ID`, without angle brackets, of the message the draft answers |
| `references` | array of strings, absent to keep the draft's own | the draft's `References`: `Message-ID`s without angle brackets. A draft marks nothing answered unless `answers` names it |
| `send` | boolean, `false` when absent | send the draft, as it is once changed (at least one recipient then); it is removed from the Drafts folder once the server took the message |
| `answers` | string, optional | with `send`: the identifier of the message the draft is a reply to, as for `send`; marked answered once the server took the reply |

The daemon saves the draft in the local copy (and, with `send`, puts the message in the outbox),
and answers: nothing waits for the server. It then writes the draft to the account's Drafts
folder (see `missive compose` in `docs/cli.md`); the `draft` events (`docs/events.md`) tell how
it went.

```json
{"protocol": 1, "answer": "draft", "ok": true, "message_id": "4b2e…@example.org"}
```

`message_id` is the draft's identifier, its `Message-ID` without angle brackets, the same for
every version of the draft. With `send`, the answer also has `outbox`, as for `send`. A `draft`
that names no draft of the account is answered `no-such-message`. A `draft` naming a draft whose
message still waits in the outbox is answered `busy`: a client that lost the answer to its `send`
and asks again sends nothing twice.

A `draft` naming a draft another client wrote (one without Missive's `X-Missive-Draft` header)
does not replace it: the daemon makes a new draft from it, under a new `message_id`, and the
answer also has `kept`, the `draft` given, which stays on the server as it is (and is not removed
once the message is sent).

### `subscribe`

Starts the event stream on this connection:

```json
{"protocol": 1, "request": "subscribe"}
```

```json
{"protocol": 1, "answer": "subscribe", "ok": true}
```

After that answer the daemon writes `event` lines on the connection, each one a JSON object with
`protocol` and `event` (the name), and the other fields `docs/events.md` gives that event. The
first ones are the present: a `state` event for each account, then an `unread` event.

The connection stays a connection of requests: it may send `sync` (or any other request) after
subscribing, and the answer comes on the same connection, in the order of the requests, among the
events. A request that takes time (a `sync` waits for the end of the sync) delays the answers
after it, not the events, and never another connection. Asking to subscribe on a connection that
already has, is answered `ok: false`, `error: "failed"`, and changes nothing.

A subscription ends with the connection, when the client closes it or a write to it fails. A
client that only closes its writing side after the request (`echo '{…subscribe…}' | socat -
UNIX-CONNECT:…` does) is still listening, and keeps receiving events. Events are not kept for a
client that is not there: one that connects later starts from the present.

A watcher that reads more slowly than the daemon writes is never waited for. Behind a watcher the
daemon keeps what the socket holds, then 16 lines in the connection's own queue (answers and
events alike, written by a task of that connection), then at most 256 events in the channel all
the watchers share; past that the oldest are dropped for that watcher alone, and the next line it
reads is `{"protocol": 1, "event": "lagged", "missed": <how many>}`. Its connection is not
closed: it may read on, or send `subscribe` on a new connection to start again from the present.
