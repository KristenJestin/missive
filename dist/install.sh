#!/bin/sh
# Installs Missive from the folder of its release archive: `missive` goes into
# ${PREFIX:-$HOME/.local}/bin, and the desktop entry that lets it open mailto: links into
# ${XDG_DATA_HOME:-$HOME/.local/share}/applications. It never asks for root, never touches your
# configuration, your local copy of the mail or your keyring, and never makes itself the default
# handler of mailto: links: it tells you the command for that.
#
#   ./install.sh
#   PREFIX=/some/where ./install.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-${HOME:?HOME is not set: say where to install with PREFIX}/.local}
bin=$prefix/bin

data=${XDG_DATA_HOME:-${HOME:?HOME is not set: say where to install with PREFIX and XDG_DATA_HOME}/.local/share}
entry=missive-mailto.desktop

[ -x "$here/missive" ] || { echo "install.sh: $here/missive is missing" >&2; exit 1; }

mkdir -p "$bin"
# Copied beside, then renamed: a launcher starting the program meanwhile never finds a half
# written file, and a program still running is not hurt.
install -m 755 "$here/missive" "$bin/.missive.new"
mv -f "$bin/.missive.new" "$bin/missive"
echo "Installed $bin/missive"

# The program is named by its full path: a launcher does not always have $bin in its PATH.
[ -f "$here/$entry" ] || { echo "install.sh: $here/$entry is missing" >&2; exit 1; }
mkdir -p "$data/applications"
while IFS= read -r line; do
  case $line in
    "Exec=missive "*) printf 'Exec="%s/missive" %s\n' "$bin" "${line#Exec=missive }" ;;
    *) printf '%s\n' "$line" ;;
  esac
done < "$here/$entry" > "$data/applications/.$entry.new"
chmod 644 "$data/applications/.$entry.new"
mv -f "$data/applications/.$entry.new" "$data/applications/$entry"
echo "Installed $data/applications/$entry"

case ":${PATH:-}:" in
  *":$bin:"*) ;;
  *) echo "Note: $bin is not in your PATH." ;;
esac
started=yes
"$bin/missive" --version || { echo "Note: $bin/missive does not start." >&2; started=no; }
echo "To open mailto: links with Missive, run:"
echo "  xdg-mime default $entry x-scheme-handler/mailto"
echo "Done. The configuration, the local copy and the keyring were left as they were."
[ "$started" = yes ]
