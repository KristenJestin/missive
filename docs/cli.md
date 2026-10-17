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

### `missive account remove <id>`

Removes the account's keyring entry, then its settings and certificate. The keyring goes first:
if it is unavailable, the account stays whole and the command can be run again.
