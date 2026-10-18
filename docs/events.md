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
computer asleep), the mail that came meanwhile is announced by the first sync that follows.

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

The pass ended, and the copy is up to date with the server: `state` `ready` came just before it.

```json
{"protocol": 1, "event": "sync-ended", "account": "alice",
 "folders": 3, "new": 6, "removed": 0, "flags": 1}
```

| Field | Type | |
|---|---|---|
| `folders` | number | the folders synced |
| `new` | number | messages added to the copy (including the ones `new-message` does not tell) |
| `removed` | number | messages that left the copy because the server expunged them |
| `flags` | number | messages whose flags changed |

A pass that fails (the connection is lost, the copy cannot be written) has a `sync-started` and no
`sync-ended`: the `state` event that follows says what became of the account.

### `lagged`

The watcher read too slowly, and events were dropped for it alone.

```json
{"protocol": 1, "event": "lagged", "missed": 12}
```

`missed` is how many. The daemon keeps 256 events behind each watcher (on top of the socket's own
buffer) and never waits for a watcher, so that one that stops reading slows neither the daemon
nor the others. The connection goes on; the events after this line are in order. A program that
cannot afford to miss one (it keeps a count of its own, say) takes `lagged` as the sign to read
the present again by subscribing anew.

### `gone`

There is no daemon: it was not running when `missive watch` started, or it stopped. Told by
`missive watch`, never by the daemon.

```json
{"protocol": 1, "event": "gone"}
```

Told once for as long as the daemon stays away. When one answers again, the stream resumes with
the present, as at the start.
