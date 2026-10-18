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

A line the daemon writes with an `answer` field is an answer. Lines with an `event` field are
reserved for the event stream that a later request will start on the same connection.

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
| `failed` | the daemon could not do it (it is stopping, for one) |

## Requests

### `sync`

Syncs every account now, or the one named by `account` (an identifier, as `missive account list`
shows it). The answer comes when that sync has ended, one way or another: `accounts` gives each
account's connection state at that point, in the order of their identifiers.

| `state` | |
|---|---|
| `ready` | the copy is up to date with the server |
| `syncing` | a sync is running |
| `no-login` | the keyring holds no password for the account (or the keyring could not be read) |
| `auth-refused` | the server refused the user name or password; asking for a sync again is what tries again |
| `certificate-refused` | the server's certificate is trusted neither by the system nor by the certificate imported for the account |
| `unreachable` | the server cannot be reached now; the daemon tries again by itself, waiting longer each time (from 1 second up to 5 minutes) |

The daemon reads the configuration again on each `sync`: an account added since it started is
synced, a removed one is let go.
