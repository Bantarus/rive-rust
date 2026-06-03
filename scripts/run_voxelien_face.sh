#!/usr/bin/env bash
# Launch the Voxelien face live viewer on the REAL GPU (RTX 4090 via Mesa Dozen),
# not the llvmpipe CPU rasterizer.
#
# Why: on WSL2, wgpu hard-filters the non-conformant Dozen adapter and falls back
# to llvmpipe ("software rendering … very slow"). These three env vars pin the
# Vulkan loader to the Dozen ICD (Dozen -> D3D12 -> host NVIDIA driver) and tell
# wgpu to accept the non-conformant adapter. They route BOTH Bevy's wgpu and
# rive's own Vulkan renderer onto the 4090.
#
# Usage (from anywhere):
#   scripts/run_voxelien_face.sh                 # the signed/published face
#   RIVE_RIV=voxelien_face.riv scripts/run_voxelien_face.sh   # the unsigned backup
#   RIVE_SIZE=768 RIVE_SPEED=1.0 scripts/run_voxelien_face.sh
#
# Expected after this: Bevy logs
#   AdapterInfo { name: "Microsoft Direct3D12 (NVIDIA GeForce RTX 4090)",
#                 device_type: DiscreteGpu, driver: "Dozen", backend: Vulkan }
# Two cosmetic warnings are normal on Dozen (capability gaps):
#   "Missing downlevel flags FULL_DRAW_INDEX_UINT32 | SURFACE_VIEW_FORMATS"
#   "VK_EXT_memory_budget not available"
set -euo pipefail
cd "$(dirname "$0")/.."

DZN_ICD=/usr/share/vulkan/icd.d/dzn_icd.json
if [[ -f "$DZN_ICD" ]]; then
  export VK_DRIVER_FILES="$DZN_ICD"                    # pin loader to Dozen only
  export DZN_DEBUG=experimental                        # Mesa "non-conformant but functional" path
  export WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1  # override wgpu's adapter filter
else
  echo "warn: Dozen ICD not found ($DZN_ICD); falling back to the default Vulkan ICD (likely slow llvmpipe)." >&2
fi

# Optimized rive C++ libs for smooth playback (dev-profile Rust is fine for a viewer).
export RIVE_RUNTIME_CONFIG="${RIVE_RUNTIME_CONFIG:-release}"

# WSLg fallback: if the window shows in the taskbar but never paints under Wayland,
# force the X11/XWayland backend with RIVE_FORCE_X11=1 (winit then uses $DISPLAY).
if [[ "${RIVE_FORCE_X11:-0}" == "1" ]]; then
  unset WAYLAND_DISPLAY
  echo "run_voxelien_face: forcing X11 (XWayland) backend via \$DISPLAY=$DISPLAY" >&2
fi

exec cargo run -p bevy-rive --example voxelien_face --features floor "$@"
