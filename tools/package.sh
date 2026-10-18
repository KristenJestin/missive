#!/bin/sh
# Packs the built `missive` with the install script and its desktop entry, and writes the SHA-256 checksum beside the
# archive. Prints the archive's file name.
#
#   tools/package.sh <tag> <binaries-dir> <out-dir>      (tag: v0.1.0, v0.1.1-beta.21)
set -eu

[ "$#" -eq 3 ] || { echo "usage: tools/package.sh <tag> <binaries-dir> <out-dir>" >&2; exit 2; }
tag=$1 binaries=$2 out=$3
root=$(cd "$(dirname "$0")/.." && pwd)

name="missive-$tag-x86_64-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/$name"
install -m 755 "$binaries/missive" "$stage/$name/missive"
install -m 755 "$root/dist/install.sh" "$stage/$name/install.sh"
install -m 644 "$root/dist/missive-mailto.desktop" "$stage/$name/missive-mailto.desktop"

mkdir -p "$out"
tar --owner=0 --group=0 -C "$stage" -czf "$out/$name.tar.gz" "$name"
# Relative name, so that `sha256sum --check` works from the folder the two files are in.
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$name.tar.gz"
