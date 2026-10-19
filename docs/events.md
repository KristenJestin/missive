# The event stream

`missive watch` prints it, one JSON object per line; a program that wants it without the command
line sends `subscribe` on the daemon's socket (`docs/protocol.md`) and reads the same lines. What
is written here is a contract: the names of events and fields are only ever added to, never
renamed, retyped or removed without a new protocol version. A reader ignores the fields and the
events it does not know.

Every line carries `protocol` (1) and `event`, the event's name, beside its own fields:

```json
{"protocol": 1, "event": "state", "account": "alice", "state": "ready"}
```

Accounts are named by their identifier, as `missive account list` shows it.

## The present, then what changes

A watcher starts with the present, so that a program that has just begun knows where things stand
without asking: one `state` event for each account, in the order of their identifiers, then one
`unread` event. After these come the events as they happen, in the order they happened.

Nothing before the moment of subscribing is replayed. A `new-message` that arrived while no one
watched is not told; the `unread` event of the present shows its effect.

## Events

### `new-message`

A message arrived.

```json
{"protocol": 1, "event": "new-message", "account": "alice", "folder": "inbox",
 "message": "alice/1/1779/42",
 "sender": {"name": "Carol Sender", "address": "carol@example.com"},
 "subject": "Quarterly plan"}
```

| Field | Type | |
|---|---|---|
| `account` | string | the account's identifier |
| `folder` | string or `null` | the folder's role: `inbox`, `sent`, `drafts`, `trash`, `junk` or `archive`; `null` for a folder of the user's own |
| `message` | string | the message's identifier, stable for as long as the server keeps the message at the same UID (see below) |
| `sender` | object or `null` | the first address of `From`: `name` and `address`, each a string or `null`; `null` when the message has none |
| `subject` | string or `null` | |

The sender and the subject are told here, on the user's own socket, because this stream is how
the user's desktop shows a notification. They are never written to the daemon's log.

The event is told for every folder, so that a program picks what it wants: a notification is for
`"folder": "inbox"`, not for a message another client put in `sent`.

**What is not told.** A message is announced only when it arrives in a folder that the daemon had
already synced, in an account whose first sync had ended. The first sync of an account, the first
sync of a folder that appears later, and the refill of a folder after the server renumbered it
(a new `UIDVALIDITY`) bring mail the user already has: telling it one message at a time would be
a flood of notifications for mail that is not new. The counts of those syncs are in `sync-ended`,
and their effect on the unread counts in `unread`. After a pause (the daemon was stopped, or the
computer asleep), the mail that came meanwhile is announced by the first sync that follows,
within the limit below.

**At most 20 per folder and per sync.** After a long pause thousands of messages can be new at
once, and a notification each would be noise. A sync of a folder tells the first 20 of its new
messages (the oldest, in the order the server numbered them) and no more: the rest are in the
`unread` event that ends the sync of that folder, and in the `new` count of `sync-ended` when the
sync is a pass over the account. The next sync does not tell them later.

**The identifier** is `<account>/<folder>/<uidvalidity>/<uid>`: the account's identifier, the
folder's number in the local copy, the folder's UIDVALIDITY and the message's UID. A program
treats it as an opaque string. It is the `id` that `missive search` gives a message, built by the
same function: a program can match an event to a search hit.

### `unread`

The unread counts changed. A message is unread when it has no `\Seen` flag and is not marked
`\Deleted`. The counts come from the query behind `missive unread`, and `total` means the same
thing in both: every folder but the junk and trash ones.

```json
{"protocol": 1, "event": "unread", "total": 5,
 "accounts": [
   {"id": "alice", "total": 5, "roles": {"inbox": 3, "sent": 0, "other": 2}}
 ]}
```

| Field | Type | |
|---|---|---|
| `total` | number | the unread messages of every account, but those in a `trash` or `junk` folder |
| `accounts` | array | every account of the copy, by identifier; one that has no folder yet is there with a total of 0 and no role, as `missive unread` shows it |
| `accounts[].id` | string | |
| `accounts[].total` | number | as `total`, for this account |
| `accounts[].roles` | object | the unread messages by folder role, for each role the account has a folder of; `other` stands for the folders of the user's own. A count of 0 is there, so that a role that dropped to 0 is told |

The event is told only when the counts are not the ones last told: reading a message elsewhere,
a message arriving, a message removed on the server. Once per folder synced, not once per message.

### `state`

An account's connection state changed.

```json
{"protocol": 1, "event": "state", "account": "alice", "state": "unreachable"}
```

`state` is one of the names of `docs/protocol.md`: `no-login`, `auth-refused`,
`certificate-refused`, `unreachable`, `syncing`, `ready`. Told when it differs from the state last
told for that account. An `unreachable` account is tried again by the daemon by itself, so its
stream shows `syncing` and `unreachable` again at each attempt, a second apart at first, then
longer.

### `sync-started`

A pass over every folder of an account began. It does not include the checks of the inbox that
follow new mail while the daemon idles: those are told by `new-message` and `unread`.

```json
{"protocol": 1, "event": "sync-started", "account": "alice"}
```

### `sync-ended`

The pass ended: `state` `ready` came just before it, and the copy is up to date with the server
except for the folders in `skipped`.

```json
{"protocol": 1, "event": "sync-ended", "account": "alice",
 "folders": 3, "new": 6, "removed": 0, "flags": 1, "skipped": []}
```

| Field | Type | |
|---|---|---|
| `folders` | number | the folders synced (not those in `skipped`) |
| `new` | number | messages added to the copy (including the ones `new-message` does not tell) |
| `removed` | number | messages that left the copy because the server expunged them |
| `flags` | number | messages whose flags changed |
| `skipped` | array | the folders the pass left as they were, `[]` when none |
| `skipped[].folder` | string | the folder's role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`), else its name |
| `skipped[].kind` | string | `refused` (the server answered `NO`) or `unusable` (it answered something no sync can use) |

A folder the server refuses does not stop the others, and the account is `ready` all the same:
`skipped` is how a program tells `ready` from `ready, and complete`. The same list is in
`missive status --json`. The next pass that syncs the folder leaves it out. It never holds what the
server said, only the kind of error.

A pass that fails (the connection is lost, the copy cannot be written) has a `sync-started` and no
`sync-ended`: the `state` event that follows says what became of the account.

### `action`

What became of an action (`missive mark`, `move`, `delete`, or their requests on the socket) once
the daemon tried it on the server.

```json
{"protocol": 1, "event": "action", "account": "alice", "action": "read",
 "outcome": "applied", "messages": ["alice/1/1745112/4"], "id": 12}
```

| Field | Type | |
|---|---|---|
| `account` | string | the account's identifier |
| `action` | string | `read`, `unread`, `flagged`, `unflagged`, `answered` (set once a reply is sent), `forwarded` (set once a forward is sent; never told when the server keeps no keywords), `move`, `delete` (a move to the trash) or `delete-permanently` |
| `outcome` | string | `applied`, `retried` or `refused` |
| `messages` | array of strings | the identifiers of the messages, as they were when the action was asked |
| `id` | number | the action's number, as the answer to its request gave it (`actions`, `docs/protocol.md`): no other action ever has it |
| `reason` | string | with `retried` and `refused` only: why, for programs (below) |
| `attempts` | number | with `retried` only: how many times it failed so far |
| `message` | string | with `refused` only: why, for people |
| `copied` | `true` | with a refused `move` or `delete` only, and only when the server had copied the messages to the target before the move was refused: they are in both folders (below) |

An action on messages of several folders is one action per folder, each with its event.

- `applied`: the server did it, and the copy shows what the server answered. A message the server
  no longer had is not an error: nothing was done for it, and the sync removes it from the copy.
- `retried`: the action stays in the queue, and the actions asked after it on the same account
  wait for it, so that they are applied in order. `reason` is `refused` (the server answered
  `NO`) or `connection` (the connection was lost before the server answered, the answer could not
  be used, or none came within 30 seconds of a command). It is tried again at the next pass, in a
  way that neither loses nor duplicates a message, and refused after 5 failures of either kind.
  After a `connection` failure, the next pass syncs the folders before it tries the queue again:
  an action that always fails never keeps the account from its mail.
- `refused`: the action was dropped and its effect taken back from the copy. `reason` is
  `renumbered` (the server renumbered the folder: its UIDs name other messages now),
  `no-such-folder` (the target of a move does not exist on the server), `unsupported` (the server
  can neither `MOVE` nor `UID EXPUNGE`, so moving or deleting for good would also remove the
  messages another client marked deleted), `refused` (the server refused it 5 times, or as
  malformed) or `connection` (it failed 5 times that way). A move or a permanent delete whose
  `UID EXPUNGE` the server refused takes its `\Deleted` flag back on the server, so that another
  client's `EXPUNGE` does not remove the message later. An action asked while a move of its messages waited is refused
  with `moved` when the server moved them without saying their new UIDs (no `COPYUID`, or a copy
  found again after a lost answer rather than told): nothing is done to them in their new folder.
  A permanent delete asked while the move of its messages to the trash waited is refused with
  `not-moved` when that move is refused: the messages never reached the trash.

A move on a server without `MOVE` is a `UID COPY`, then the removal of the messages from their
folder. When the removal is refused for good (or the folder renumbered) once the copy was made,
the messages are in both folders: the event has `copied` `true`, its `message` says so, the copy
shows both (the copy in the target too, when the server told its UID), and `missive status` lists
each message in `half_moved` until the user deletes or moves one of the two. Nothing is removed to
fix it without the user.

### `sent`

The server took a message of the outbox (`missive compose --send`, or `send` on the socket): it
answered `250` to its end of data.

```json
{"protocol": 1, "event": "sent", "account": "alice", "outbox": 3,
 "message_id": "9c1f…@example.org", "sent_copy": "append"}
```

| Field | Type | |
|---|---|---|
| `account` | string | the account's identifier |
| `outbox` | number | its number in the outbox, as `compose --json` printed it |
| `message_id` | string | its `Message-ID`, without angle brackets |
| `sent_copy` | string | `append`: Missive appends its copy to the Sent folder now; `server`: the account says its server keeps the copy itself; `none`: the account has no Sent folder, so there is no copy (Missive never creates one) |

### `send-failed`

The server refused a message for good (a `5xx` answer): it stays in the outbox as `failed`, and
is never sent again unless the user asks for it (`missive outbox resend`).

```json
{"protocol": 1, "event": "send-failed", "account": "alice", "outbox": 3,
 "message_id": "9c1f…@example.org", "code": 550, "step": "RCPT TO",
 "message": "the server refused the message for good (it answered 550 to RCPT TO): it is not sent again"}
```

| Field | Type | |
|---|---|---|
| `account`, `outbox`, `message_id` | | as for `sent` |
| `code` | number | the server's reply code |
| `step` | string | what it answered: `MAIL FROM`, `RCPT TO` (one recipient refused is the whole message refused), `DATA` or `end of data` |
| `message` | string | for people |

A refusal that may not last (`4xx`), a server that cannot be reached, a login or a certificate
refused, are not events: the message stays `queued`, tried again later (`missive status` shows
its tries and its last error).

### `send-uncertain`

The connection was lost after a message's end of data and before the server's answer, or no
answer came within 10 minutes, or the daemon was killed while it was sending: the server may have
taken it. It stays in the outbox as `uncertain`, and is never sent again by itself, since a
message delivered twice is worse than a question: only `missive outbox resend <n> --yes` does.
A daemon that finds a message it was sending when it stopped tells this event when it starts,
before any watcher can be subscribed: `status` shows it.

```json
{"protocol": 1, "event": "send-uncertain", "account": "alice", "outbox": 3,
 "message_id": "9c1f…@example.org",
 "message": "the connection was lost before the server answered (…): the message may have been sent, so it is not sent again"}
```

| Field | Type | |
|---|---|---|
| `account`, `outbox`, `message_id` | | as for `sent` |
| `message` | string | for people |

### `sent-copy-failed`

The copy of a message the server took will never be in the account's Sent folder: appending it
failed five times, looking for it in Sent (to avoid appending it twice) failed five times, the
server says there is no such folder, the folder's name cannot be written in an IMAP command, or
the account turned out to have no Sent folder. The message was
sent all the same; it leaves the outbox.

```json
{"protocol": 1, "event": "sent-copy-failed", "account": "alice", "outbox": 3,
 "message_id": "9c1f…@example.org",
 "message": "the message was sent, but its copy is not in Sent: appending it failed too many times"}
```

| Field | Type | |
|---|---|---|
| `account`, `outbox`, `message_id` | | as for `sent` |
| `message` | string | for people |

### `draft`

A draft was written to the account's Drafts folder, removed from it (its message was sent), or
is tried again later (`missive compose`, or `draft` on the socket).

```json
{"protocol": 1, "event": "draft", "account": "alice", "draft": "4b2e…@example.org",
 "outcome": "saved", "id": "alice/3/1745112/12", "left": 0}
```

| Field | Type | |
|---|---|---|
| `account` | string | the account's identifier |
| `draft` | string | the draft's identifier (its `Message-ID`, without angle brackets), as `compose --json` printed it |
| `outcome` | string | `saved`: the version is in the Drafts folder; `removed`: the draft left it, its message sent; `retried`: it stays in the local copy, tried again at the next pass |
| `id` | string, absent when not known | for `saved`: the identifier of the version in the Drafts folder |
| `left` | number | for `saved` and `removed`: how many older versions stay in the Drafts folder, `0` but on a server without UIDPLUS (no `UID EXPUNGE`), or one that refused to remove them |
| `reason` | string | for `retried`: `refused` (the server answered `NO` or `BAD`) or `connection` (the connection was lost, or no answer came in time) |
| `attempts` | number | for `retried`: how many times it failed so far |
| `message` | string | when `left` is not `0`: why, for people |

A draft is never given up: the version stays in the local copy until the server holds it. A
draft of an account without a Drafts folder is not an event: it stays in the copy, and
`missive status` says so.

### `lagged`

The watcher read too slowly, and events were dropped for it alone.

```json
{"protocol": 1, "event": "lagged", "missed": 12}
```

`missed` is how many. Behind each watcher the daemon keeps what the socket holds, 16 lines in the
connection's own queue, then 256 events (see `docs/protocol.md`); it never waits for a watcher, so
that one that stops reading slows neither the daemon nor the others. The connection goes on; the
events after this line are in order. A program that cannot afford to miss one (it keeps a count of
its own, say) takes `lagged` as the sign to read the present again by subscribing anew.

### `gone`

There is no daemon: it was not running when `missive watch` started, or it stopped. Told by
`missive watch`, never by the daemon.

```json
{"protocol": 1, "event": "gone"}
```

Told once for as long as the daemon stays away. When one answers again, the stream resumes with
the present, as at the start.
