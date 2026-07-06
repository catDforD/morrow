#!/usr/bin/env sh
set -eu

REPO="${MORROW_REPO:-catDforD/morrow}"
VERSION="${MORROW_VERSION:-latest}"
INSTALL_DIR="${MORROW_INSTALL_DIR:-$HOME/.local/bin}"

need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        exit 1
    fi
}

target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux) os_part="unknown-linux-gnu" ;;
        Darwin) os_part="apple-darwin" ;;
        *)
            echo "error: unsupported OS: $os" >&2
            echo "Windows users should download morrow-x86_64-pc-windows-msvc.zip from GitHub Releases." >&2
            exit 1
            ;;
    esac

    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        aarch64 | arm64) arch_part="aarch64" ;;
        *)
            echo "error: unsupported CPU architecture: $arch" >&2
            exit 1
            ;;
    esac

    echo "$arch_part-$os_part"
}

download_url() {
    file="$1"
    if [ "$VERSION" = "latest" ]; then
        echo "https://github.com/$REPO/releases/latest/download/$file"
    else
        echo "https://github.com/$REPO/releases/download/$VERSION/$file"
    fi
}

sha256_file() {
    file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    else
        shasum -a 256 "$file" | awk '{print $1}'
    fi
}

need curl
need tar
need awk
need grep

TARGET="$(target)"
ARCHIVE="morrow-$TARGET.tar.gz"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT INT TERM

echo "Downloading $ARCHIVE from $REPO..."
curl -fsSL "$(download_url "$ARCHIVE")" -o "$WORK_DIR/$ARCHIVE"
curl -fsSL "$(download_url "SHA256SUMS")" -o "$WORK_DIR/SHA256SUMS"

EXPECTED="$(grep "[[:space:]]$ARCHIVE\$" "$WORK_DIR/SHA256SUMS" | awk '{print $1}')"
if [ -z "$EXPECTED" ]; then
    echo "error: checksum for $ARCHIVE was not found in SHA256SUMS" >&2
    exit 1
fi

ACTUAL="$(sha256_file "$WORK_DIR/$ARCHIVE")"
if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "error: checksum mismatch for $ARCHIVE" >&2
    exit 1
fi

tar -xzf "$WORK_DIR/$ARCHIVE" -C "$WORK_DIR"
mkdir -p "$INSTALL_DIR"
install -m 755 "$WORK_DIR/morrow" "$INSTALL_DIR/morrow"
if [ -f "$WORK_DIR/morrow-rg" ]; then
    install -m 755 "$WORK_DIR/morrow-rg" "$INSTALL_DIR/morrow-rg"
fi

echo "Installed morrow to $INSTALL_DIR/morrow"
if [ -f "$INSTALL_DIR/morrow-rg" ]; then
    echo "Installed ripgrep sidecar to $INSTALL_DIR/morrow-rg"
fi
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "Add $INSTALL_DIR to PATH if morrow is not found:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
echo "Next: morrow init"
