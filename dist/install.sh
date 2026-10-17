#!/bin/sh
# Installs Missive from the folder of its release archive: `missive` goes into
# ${PREFIX:-$HOME/.local}/bin. It never asks for root, and never touches your configuration, your
# local copy of the mail or your keyring.
#
#   ./install.sh
#   PREFIX=/some/where ./install.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-${HOME:?HOME is not set: say where to install with PREFIX}/.local}
bin=$prefix/bin

[ -x "$here/missive" ] || { echo "install.sh: $here/missive is missing" >&2; exit 1; }

mkdir -p "$bin"
# Copied beside, then renamed: a launcher starting the program meanwhile never finds a half
# written file, and a program still running is not hurt.
install -m 755 "$here/missive" "$bin/.missive.new"
mv -f "$bin/.missive.new" "$bin/missive"
echo "Installed $bin/missive"

case ":${PATH:-}:" in
  *":$bin:"*) ;;
  *) echo "Note: $bin is not in your PATH." ;;
esac
started=yes
"$bin/missive" --version || { echo "Note: $bin/missive does not start." >&2; started=no; }
echo "Done. The configuration, the local copy and the keyring were left as they were."
[ "$started" = yes ]
