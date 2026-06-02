#!/usr/bin/env bash
# Archive the rive static libs + shim into a directory for RIVE_PREBUILT_LIBS, so a
# consumer can link bevy-rive WITHOUT the C++ toolchain (M-PKG.1; see BUILD.md §8).
#
# Usage:
#   tools/archive_prebuilt.sh [out_dir] [--release]
#
# Linux/dev by default. Produces <out_dir> containing the 10 rive `.a` libs + the
# `librive_shim.a` archive. (Windows uses the `.lib` equivalents — archive those by hand
# or extend this script; the relay's out dir is the same layout with `.lib`.)
set -euo pipefail
cd "$(dirname "$0")/.."

release=0
out=""
for arg in "$@"; do
  case "$arg" in
    --release) release=1 ;;
    *) out="$arg" ;;
  esac
done

if [[ "$release" == 1 ]]; then
  profile_dir="release"; rive_out="out/rive-rust-m0-release"
  : "${out:=prebuilt/linux-release}"
  echo ">> building rive libs + shim from source (release)"
  cargo build --release -p bevy-rive
else
  profile_dir="debug"; rive_out="out/rive-rust-m0"
  : "${out:=prebuilt/linux-dev}"
  echo ">> building rive libs + shim from source (dev)"
  cargo build -p bevy-rive
fi

echo ">> collecting archives into $out"
mkdir -p "$out"
cp vendor/rive-runtime/renderer/"$rive_out"/*.a "$out"/
shim="$(ls -t target/"$profile_dir"/build/rive-renderer-sys-*/out/librive_shim.a | head -1)"
cp "$shim" "$out"/

count="$(ls "$out" | wc -l)"
abs_out="$(cd "$out" && pwd)"
echo ">> done: $count archives in $abs_out"
ls -1 "$out" | sed 's/^/   /'
echo ">> link a consumer with:  RIVE_PREBUILT_LIBS=$abs_out cargo build"
