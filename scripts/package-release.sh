#!/bin/sh
# Usage: package-release.sh <target> <bin-dir> <out-dir>
# Creates <out-dir>/chocofactory-<target>.tar.gz holding exactly
# chocofactory-<target>/{choco,chocofactoryd,README.md,LICENSE-MIT,LICENSE-APACHE}.
set -eu

if [ "$#" -ne 3 ]; then
    printf 'usage: %s <target> <bin-dir> <out-dir>\n' "$0" >&2
    exit 2
fi
target=$1
bin_dir=$2
out_dir=$3
root=$(cd "$(dirname "$0")/.." && pwd)

for f in "$bin_dir/choco" "$bin_dir/chocofactoryd" "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE"; do
    if [ ! -f "$f" ]; then
        printf 'package-release: missing %s\n' "$f" >&2
        exit 1
    fi
done

name="chocofactory-$target"
mkdir -p "$out_dir"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$bin_dir/choco" "$bin_dir/chocofactoryd" "$stage/$name/"
chmod 755 "$stage/$name/choco" "$stage/$name/chocofactoryd"
cp "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$stage/$name/"
tar -czf "$out_dir/$name.tar.gz" -C "$stage" "$name"
printf 'wrote %s\n' "$out_dir/$name.tar.gz"
