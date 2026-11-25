#!/bin/sh
# Install relief.
#
#   From a checkout:   ./install.sh          (builds with cargo)
#   From anywhere:     curl -fsSL https://raw.githubusercontent.com/tatlilimon/relief/main/install.sh | sh
#
# Environment:
#   RELIEF_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
#   RELIEF_REPO         GitHub repository to download from (default: tatlilimon/relief)
#   RELIEF_VERSION      release tag to download (default: the latest release)
set -eu

repo="${RELIEF_REPO:-tatlilimon/relief}"
dest="${RELIEF_INSTALL_DIR:-$HOME/.local/bin}"
here="$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo .)"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

install_binary() {
    mkdir -p "$dest"
    # Replace atomically so a running relief is not cut off mid-file.
    cp "$1" "$dest/relief.new"
    chmod 755 "$dest/relief.new"
    mv "$dest/relief.new" "$dest/relief"
    say "Installed $("$dest/relief" --version) to $dest/relief"
    case ":$PATH:" in
        *":$dest:"*) ;;
        *) say "Note: $dest is not on your PATH. Add it, for example:"
           say "  echo 'export PATH=\"$dest:\$PATH\"' >> ~/.profile" ;;
    esac
    say "Try: relief scan"
}

# 1. Inside a checkout, build from source.
if [ -f "$here/Cargo.toml" ] && grep -q '^name = "relief"' "$here/Cargo.toml" 2>/dev/null; then
    command -v cargo >/dev/null 2>&1 || die "this is a source checkout but cargo is not installed (https://rustup.rs)"
    say "Building relief from source..."
    (cd "$here" && cargo build --release --locked)
    install_binary "$here/target/release/relief"
    exit 0
fi

# 2. Otherwise download a prebuilt, statically linked binary.
[ "$(uname -s)" = "Linux" ] || die "prebuilt binaries exist for Linux only so far; build from source with cargo"
case "$(uname -m)" in
    x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
    aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
    *) die "no prebuilt binary for $(uname -m); build from source with cargo" ;;
esac
if [ -n "${RELIEF_VERSION:-}" ]; then
    base="https://github.com/$repo/releases/download/$RELIEF_VERSION"
else
    base="https://github.com/$repo/releases/latest/download"
fi
archive="relief-$target.tar.gz"

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
else
    die "need curl or wget to download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
say "Downloading $base/$archive"
fetch "$base/$archive" "$tmp/$archive" || die "download failed; is there a release at https://github.com/$repo/releases ?"
fetch "$base/$archive.sha256" "$tmp/$archive.sha256" || die "could not download the checksum"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "$archive.sha256" >/dev/null) || die "checksum mismatch: refusing to install"
else
    die "sha256sum is needed to verify the download"
fi
tar -xzf "$tmp/$archive" -C "$tmp"
install_binary "$tmp/relief"
