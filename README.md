# Missive

A mail client for Linux, written in Rust: a background daemon that keeps your accounts in sync,
a command line and an event stream other programs can drive and follow, local search, and soon a
window. IMAP and SMTP accounts, local bridges such as Proton Mail Bridge included.

Work in progress: see the issues. Agents read `AGENTS.md`.

## Install

No build needed: a release is an archive of the Releases page of this repository,
`missive-<tag>-x86_64-linux.tar.gz` with its `.sha256`, holding `missive` and `install.sh`. It is
built on Ubuntu 24.04 for Linux x86_64 and needs glibc 2.39 or newer.

- **A release** (`vX.Y.Z`) is made when `dev` is merged into `main`.
- **The beta** (`vX.Y.Z-beta.N`, marked pre-release) is made at every push to `dev`, and only the
  newest one exists. It is the patch after the last release, so it is newer than it.

In an empty folder, with `<tag>` the release (`v0.1.0`) or the beta (`v0.1.1-beta.21`) shown on the
Releases page:

```
gh release download <tag> --repo KristenJestin/missive --pattern 'missive-*'
sha256sum --check missive-*.sha256
tar -xzf missive-*.tar.gz
cd missive-*-x86_64-linux
./install.sh
```

Without `gh`, download the two files from the Releases page in a browser and run the same commands
from `sha256sum` on. `install.sh` copies `missive` into `~/.local/bin` (or `$PREFIX/bin`), never
asks for root and never touches your configuration, your local copy of the mail or your keyring.
`~/.local/bin` must be in your `PATH`. `missive --version` prints the tag the build was published
under (`missive v0.1.1-beta.21`), or `missive development build` for a build made locally. To
update, do the same with the newer archive; to remove Missive, delete `~/.local/bin/missive`.
