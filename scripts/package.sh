#!/bin/sh
# Construit l'archive de release dans dist/ :
#   dist/mcp-shell-<version>-x86_64-linux.tar.gz (+ .sha256)
# Binaire statique musl : fonctionne tel quel sur Debian/Proxmox et Alpine.
#
# Prérequis : rustup target add x86_64-unknown-linux-musl ; musl-tools (musl-gcc).
set -eu

cd "$(dirname "$0")/.."
TARGET=x86_64-unknown-linux-musl
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
NAME="mcp-shell-$VERSION"
ARCHIVE="$NAME-x86_64-linux.tar.gz"

cargo build --release --locked --target "$TARGET"

rm -rf dist && mkdir -p "dist/$NAME/bin"
install -m 0755 "target/$TARGET/release/mcp-shell" "dist/$NAME/bin/mcp-shell"
(cd "dist/$NAME/bin" && sha256sum mcp-shell > mcp-shell.sha256)
cp -r packaging "dist/$NAME/packaging"
cp README.md CHANGELOG.md LICENSE "dist/$NAME/"
install -m 0755 scripts/install.sh "dist/$NAME/install.sh"

# Archive reproductible : ordre, dates et propriétaires fixés.
EPOCH=$(git log -1 --format=%ct 2>/dev/null || date +%s)
tar --sort=name --mtime="@$EPOCH" --owner=0 --group=0 --numeric-owner \
    -C dist -czf "dist/$ARCHIVE" "$NAME"
(cd dist && sha256sum "$ARCHIVE" > "$ARCHIVE.sha256")
rm -rf "dist/$NAME"

echo "dist/$ARCHIVE"
cat "dist/$ARCHIVE.sha256"
