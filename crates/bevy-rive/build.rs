//! Build script for `bevy-rive` — the M-PKG.1 fail-fast version guard (zero-copy tier).
//!
//! The version-LOCKED `zero_copy` tier reaches into Bevy's wgpu via `as_hal::<Vulkan>`,
//! whose handle types are defined by the *exact* `wgpu-hal` version. The hard guard is
//! the exact Cargo pins in `Cargo.toml` (`wgpu = =27.0.1`, `wgpu-hal = =27.0.4`,
//! `wgpu-types` 27.0.1 via Bevy, `ash = 0.38`): if a consumer's Bevy resolves a
//! *different* wgpu, Cargo fails **at resolution time** with an error NAMING the
//! conflicting versions — never a baffling `as_hal` type error or silent corruption.
//!
//! This script's only job is to make that lock LOUD: when `zero_copy` is enabled it
//! emits the required triple as a build warning, so the ABI contract is visible in every
//! build and on every Bevy bump (each of which is a deliberate interop re-validation).
//! The default `floor` tier is loosely coupled (caret `bevy`/`wgpu-types`) and triggers
//! nothing here.

/// The host triple the `zero_copy` `as_hal` FFI is ABI-locked to. Kept here as the
/// single human-readable statement of the lock the Cargo pins enforce.
const ZERO_COPY_LOCK: &str = "Bevy 0.18.1 / wgpu 27.0.1 / wgpu-hal 27.0.4 / ash 0.38";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Cargo sets CARGO_FEATURE_<NAME> (uppercased, `-`/`.`→`_`) for each active feature.
    if std::env::var_os("CARGO_FEATURE_ZERO_COPY").is_some() {
        println!(
            "cargo:warning=bevy-rive `zero_copy` tier is ABI-locked to {ZERO_COPY_LOCK} \
             (enforced by exact Cargo pins). A host Bevy on a different wgpu will fail to \
             resolve, naming the versions — re-validate the Vulkan interop on every Bevy \
             bump and do NOT loosen the pins. The default `floor` tier has no such lock."
        );
    }
}
