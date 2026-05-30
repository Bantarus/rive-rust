#!/usr/bin/env bash
# Fetches the premake5 binary used to build rive-runtime into tools/premake5.
# Pinned to 5.0.0-beta2, matching rive-runtime's own Dockerfile.
set -euo pipefail

VERSION="5.0.0-beta2"
URL="https://github.com/premake/premake-core/releases/download/v${VERSION}/premake-${VERSION}-linux.tar.gz"
DEST_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"

echo "Downloading premake5 ${VERSION} ..."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$URL" -o "$tmp/premake.tar.gz" || wget -q "$URL" -O "$tmp/premake.tar.gz"
tar -xf "$tmp/premake.tar.gz" -C "$tmp"
mv "$tmp/premake5" "$DEST_DIR/premake5"
chmod +x "$DEST_DIR/premake5"
echo "Installed $DEST_DIR/premake5"
"$DEST_DIR/premake5" --version
