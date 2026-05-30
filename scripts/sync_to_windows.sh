#!/bin/bash
# sync_to_windows.sh — mirror the working tree to E:\DEV\rive-rust for a native
# Windows build/test, run from WSL2.
#
# The canonical repo lives on the WSL2 ext4 filesystem (fast iteration + the
# Linux build); it is copied to the 9p-mounted Windows drive only when testing
# the Windows build. Excludes Linux build objects and target/ (Windows builds
# its own); KEEPS vendor/ sources, the .rive-deps header cache, and the prebuilt
# SPIR-V/generated shader headers under out/ (reused verbatim on Windows so
# glslangValidator/spirv-opt are never needed there). No --delete, so Windows
# build outputs already on E: (.lib, .sln, generated d3d headers) are preserved.
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/"
DST="${1:-/mnt/e/DEV/rive-rust/}"

mkdir -p "$DST"
rsync -rlt --modify-window=2 \
    --exclude='/target/' \
    --exclude='.git/' \
    --exclude='*.o' --exclude='*.a' --exclude='*.d' \
    "$SRC" "$DST"
echo "synced $SRC -> $DST"
