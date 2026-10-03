#!/bin/sh
# ChocoFactory installer. Installs `choco` and `chocofactoryd` side by side.
#   CHOCO_VERSION          version to install (default: latest)
#   CHOCO_INSTALL_DIR      install directory (default: $HOME/.local/bin)
#   CHOCO_RELEASES_URL     releases base URL
#   CHOCO_INSTALL_ARCHIVE  local archive; skips downloading and checksums
set -eu

die() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

[ -n "${HOME:-}" ] || die 'HOME is not set'
BASE=${CHOCO_RELEASES_URL:-https://github.com/itsypkin/ChocoFactory/releases}
DIR=${CHOCO_INSTALL_DIR:-$HOME/.local/bin}
VERSION=${CHOCO_VERSION:-}

os=$(uname -s)
arch=$(uname -m)
case "$os $arch" in
    "Darwin arm64") target=aarch64-apple-darwin ;;
    "Darwin x86_64") target=x86_64-apple-darwin ;;
    "Linux x86_64") target=x86_64-unknown-linux-musl ;;
    "Linux aarch64" | "Linux arm64") target=aarch64-unknown-linux-musl ;;
    *) die "unsupported platform: $os $arch" ;;
esac
asset="chocofactory-$target.tar.gz"

tmp=$(mktemp -d) || die 'cannot create a temporary directory'
trap 'rm -rf "$tmp"' EXIT

if [ -n "${CHOCO_INSTALL_ARCHIVE:-}" ]; then
    [ -f "$CHOCO_INSTALL_ARCHIVE" ] || die "archive not found: $CHOCO_INSTALL_ARCHIVE"
    archive=$CHOCO_INSTALL_ARCHIVE
else
    if [ -z "$VERSION" ]; then
        url_base="$BASE/latest/download"
    else
        url_base="$BASE/download/v$VERSION"
    fi
    archive="$tmp/$asset"
    curl -fsSL -o "$archive" "$url_base/$asset" || die "could not download $url_base/$asset"
    curl -fsSL -o "$tmp/SHA256SUMS" "$url_base/SHA256SUMS" || die "could not download $url_base/SHA256SUMS"
    if command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$archive" | cut -d ' ' -f 1) || die 'shasum failed'
    elif command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$archive" | cut -d ' ' -f 1) || die 'sha256sum failed'
    else
        die 'need shasum or sha256sum to verify the download'
    fi
    # `sha256sum` lines are "<hash>  <name>" (or " *<name>" in binary mode).
    expected=$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1; exit }' "$tmp/SHA256SUMS") ||
        die "could not read $url_base/SHA256SUMS"
    [ -n "$expected" ] || die "no checksum for $asset in $url_base/SHA256SUMS"
    [ "$expected" = "$actual" ] || die "checksum mismatch for $url_base/$asset (expected $expected, got $actual)"
fi

mkdir "$tmp/x" || die 'cannot create a temporary directory'
tar -xzf "$archive" -C "$tmp/x" || die "could not extract $archive"
src="$tmp/x/chocofactory-$target"
for b in choco chocofactoryd; do
    [ -f "$src/$b" ] || die "the archive has no $b (looked for $src/$b)"
done

nl='
'
tab=$(printf '\t')
# The marker is plain JSON without escaping: refuse characters it cannot hold,
# before anything is created.
case "$DIR" in
    *[\"\\]*|*"$nl"*|*"$tab"*) die "install directory $DIR contains a quote, backslash, tab or newline, which the marker file cannot hold" ;;
esac
mkdir -p "$DIR" || die "cannot create $DIR"
DIR=$(cd "$DIR" && pwd -P) || die "cannot resolve $DIR"
# Never overwrite a binary in place: macOS caches code signatures per file.
# Copy under a temporary name, then rename over the old one. Daemon first.
for b in chocofactoryd choco; do
    tmpbin="$DIR/.choco-install.$$.$b"
    cp "$src/$b" "$tmpbin" || { rm -f "$tmpbin"; die "cannot copy $b to $tmpbin"; }
    chmod 755 "$tmpbin" || { rm -f "$tmpbin"; die "cannot chmod $tmpbin"; }
    mv -f "$tmpbin" "$DIR/$b" || { rm -f "$tmpbin"; die "cannot move $tmpbin to $DIR/$b"; }
done

vline=$("$DIR/chocofactoryd" --version) || die "$DIR/chocofactoryd --version failed"
version=$(printf '%s\n' "$vline" | awk '{ print $2 }')
[ -n "$version" ] || die "cannot read a version from: $vline"

conf="$HOME/.config/chocofactory"
mkdir -p "$conf" || die "cannot create $conf"
printf '{"channel":"github-release","dir":"%s","version":"%s","target":"%s"}\n' \
    "$DIR" "$version" "$target" >"$conf/install.json.$$" || die "cannot write $conf/install.json.$$"
mv -f "$conf/install.json.$$" "$conf/install.json" || die "cannot write $conf/install.json"

printf 'installed chocofactory %s to %s\n' "$version" "$DIR"
case ":${PATH:-}:" in
    *":$DIR:"*) ;;
    *) printf '%s is not on your PATH; add it with:\n  export PATH="%s:$PATH"\n' "$DIR" "$DIR" ;;
esac
# A nonzero status here just means no daemon is running (the designed case).
if "$DIR/choco" server status >/dev/null 2>&1; then
    printf 'restart the running daemon to use the new version: choco server restart\n'
fi
