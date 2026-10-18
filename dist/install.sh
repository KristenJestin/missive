#!/bin/sh
# Installs Missive from the folder of its release archive: `missive` (the command line and the
# daemon) and `missive-window` (the reading window) go into ${PREFIX:-$HOME/.local}/bin, and the
# desktop entries into ${XDG_DATA_HOME:-$HOME/.local/share}/applications: one for the window, in
# the menus, and one that lets Missive open mailto: links. It never asks for root, never touches
# your configuration, your local copy of the mail or your keyring, never starts the window, and
# never makes itself the default handler of mailto: links: it tells you the command for that.
#
#   ./install.sh
#   PREFIX=/some/where ./install.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-${HOME:?HOME is not set: say where to install with PREFIX}/.local}
bin=$prefix/bin

data=${XDG_DATA_HOME:-${HOME:?HOME is not set: say where to install with PREFIX and XDG_DATA_HOME}/.local/share}
mailto=missive-mailto.desktop
menu=missive.desktop

# Everything is there before anything is installed.
for file in missive missive-window "$mailto" "$menu"; do
  [ -f "$here/$file" ] || { echo "install.sh: $here/$file is missing" >&2; exit 1; }
done

mkdir -p "$bin"
# Copied beside, then renamed: a launcher starting a program meanwhile never finds a half written
# file, and a program still running is not hurt.
for program in missive missive-window; do
  install -m 755 "$here/$program" "$bin/.$program.new"
  mv -f "$bin/.$program.new" "$bin/$program"
  echo "Installed $bin/$program"
done

# A program is named by its full path: a launcher does not always have $bin in its PATH.
mkdir -p "$data/applications"
for entry in "$mailto" "$menu"; do
  while IFS= read -r line; do
    case $line in
      "Exec=missive-window") printf 'Exec="%s/missive-window"\n' "$bin" ;;
      "Exec=missive "*) printf 'Exec="%s/missive" %s\n' "$bin" "${line#Exec=missive }" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done < "$here/$entry" > "$data/applications/.$entry.new"
  chmod 644 "$data/applications/.$entry.new"
  mv -f "$data/applications/.$entry.new" "$data/applications/$entry"
  echo "Installed $data/applications/$entry"
done

case ":${PATH:-}:" in
  *":$bin:"*) ;;
  *) echo "Note: $bin is not in your PATH." ;;
esac
# Only the command line program is started: the window would open a window.
started=yes
"$bin/missive" --version || { echo "Note: $bin/missive does not start." >&2; started=no; }
echo "To open mailto: links with Missive, run:"
echo "  xdg-mime default $mailto x-scheme-handler/mailto"
echo "Done. The configuration, the local copy and the keyring were left as they were."
[ "$started" = yes ]
