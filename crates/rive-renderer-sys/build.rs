//! Builds the native rive-runtime PLS (Vulkan) static libraries via premake5 +
//! make, compiles the C++ FFI shim (plus rive's `rive_vk_bootstrap` sources)
//! with clang, and emits the link directives.
//!
//! Prerequisites (detected below, with actionable errors): clang/clang++,
//! premake5 (vendored under `tools/` or on PATH), GNU make, python3,
//! glslangValidator, spirv-opt, git, and the Vulkan loader dev symlink.
//!
//! See ../../BUILD.md for the full toolchain notes and the rationale behind the
//! premake invocation (CWD, `--out`, `config=default`, debug-vs-release/LTO).

use std::path::{Path, PathBuf};
use std::process::Command;

/// rive-runtime static-lib project names, in single-pass link order
/// (consumers before providers). `librive_shim.a` is prepended by `cc`.
const RIVE_LIBS: &[&str] = &[
    "rive_pls_renderer",
    "rive",
    "rive_decoders",
    "libpng", // -> liblibpng.a
    "zlib",   // -> libzlib.a
    "libjpeg", // -> liblibjpeg.a
    "libwebp", // -> liblibwebp.a
    "rive_harfbuzz",
    "rive_sheenbidi",
    "rive_yoga",
];

/// rive_vk_bootstrap sources compiled into the shim (rive does not build these
/// into any static lib). The swapchain/present path is intentionally excluded —
/// M0 is headless/offscreen only.
const BOOTSTRAP_SOURCES: &[&str] = &[
    "vulkan_instance.cpp",
    "vulkan_device.cpp",
    "vulkan_library.cpp",
    "vulkan_debug_callbacks.cpp",
    "vulkan_frame_synchronizer.cpp",
    "vulkan_headless_frame_synchronizer.cpp",
];

/// Pinned dependency tags (must match renderer/premake5_pls_renderer.lua).
const VULKAN_HEADERS_DIR: &str = "KhronosGroup_Vulkan-Headers_vulkan-sdk-1.4.321";
const VMA_DIR: &str = "GPUOpen-LibrariesAndSDKs_VulkanMemoryAllocator_v3.3.0";

fn main() {
    let manifest_dir = PathBuf::from(env_var("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate is at <workspace>/crates/rive-renderer-sys")
        .to_path_buf();

    let shim_dir = manifest_dir.join("shim");
    let rive_root = workspace_root.join("vendor/rive-runtime");
    let renderer_dir = rive_root.join("renderer");
    let premake_build_dir = rive_root.join("build");
    let deps_cache = workspace_root.join(".rive-deps");
    // RIVE_BUILD_OUT is anchored to premake's working dir (renderer/), so the
    // output must live under it; `--out` is concatenated and must be RELATIVE.
    let out_rel = "out/rive-rust-m0";
    let rive_out = renderer_dir.join(out_rel);

    // Rebuild triggers.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim/rive_shim.h");
    println!("cargo:rerun-if-changed=shim/rive_shim.cpp");
    for var in ["CC", "CXX", "PREMAKE5", "RIVE_RUNTIME_CONFIG"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    ensure_submodule_present(&rive_root);
    let premake5 = find_premake5(&workspace_root);
    require_build_tools();

    std::fs::create_dir_all(&deps_cache).expect("create .rive-deps cache dir");

    // Both premake (shader gen via clang) and make must use clang, not gcc.
    let cc = std::env::var("CC").unwrap_or_else(|_| "clang".into());
    let cxx = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    // Debug config: avoids release LTO, whose LLVM-bitcode archives can confuse
    // a non-LLVM final linker. M0 favors clean linking over runtime speed.
    let rive_config =
        std::env::var("RIVE_RUNTIME_CONFIG").unwrap_or_else(|_| "debug".into());

    // ---- Step 1: premake5 generates gmake2 Makefiles (and runs shader gen). --
    run(
        Command::new(&premake5)
            .current_dir(&renderer_dir)
            .env("PREMAKE_PATH", &premake_build_dir)
            .env("DEPENDENCIES", &deps_cache)
            .env("CC", &cc)
            .env("CXX", &cxx)
            .args([
                "gmake2",
                &format!("--config={rive_config}"),
                &format!("--out={out_rel}"),
                "--with_vulkan",
                "--with_rive_text",
                "--with_rive_layout",
                "--no-download-progress",
            ]),
        "premake5 gmake2 (generate Makefiles + shaders)",
    );

    // ---- Step 2: make builds the explicit static-lib targets (never `all`). --
    let jobs = std::env::var("NUM_JOBS").unwrap_or_else(|_| "4".into());
    run(
        Command::new("make")
            .current_dir(&renderer_dir)
            .env("DEPENDENCIES", &deps_cache)
            .env("CC", &cc)
            .env("CXX", &cxx)
            .arg("-C")
            .arg(&rive_out)
            .arg("config=default") // the ONLY premake configuration is "default"
            .arg(format!("-j{jobs}"))
            .args(RIVE_LIBS),
        "make (build rive-runtime static libs)",
    );

    verify_archives(&rive_out);

    // ---- Step 3: compile the shim + bootstrap with clang. -------------------
    let vk_headers_inc = deps_cache.join(VULKAN_HEADERS_DIR).join("include");
    let vma_inc = deps_cache.join(VMA_DIR).join("include");
    assert!(
        vk_headers_inc.join("vulkan/vulkan.h").exists(),
        "Vulkan headers not found at {} — premake should have cloned them. \
         Check network/git access during the premake step.",
        vk_headers_inc.display()
    );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .compiler(&cxx)
        .std("c++17")
        .flag("-fno-rtti") // match rive's ABI (RTTI off); exceptions left on for the shim boundary
        .define("RIVE_VULKAN", None)
        .define("VK_NO_PROTOTYPES", None)
        .define("VMA_STATIC_VULKAN_FUNCTIONS", "0")
        .define("VMA_DYNAMIC_VULKAN_FUNCTIONS", "1")
        // The pinned Vulkan/VMA headers MUST come first so `<vulkan/vulkan.h>`
        // resolves to the vendored `vulkan-sdk-1.4.321` copy, not the system
        // header (which otherwise wins and lacks symbols rive's sources expect).
        .include(&vk_headers_inc)
        .include(&vma_inc)
        .include(&shim_dir)
        .include(rive_root.join("include"))
        .include(renderer_dir.join("include"))
        // renderer/src so the bootstrap sources can `#include "shaders/constants.glsl"`,
        // and the generated-headers dir for the SPIR-V/shader headers.
        .include(renderer_dir.join("src"))
        .include(rive_out.join("include"))
        .include(rive_root.join("decoders/include"))
        .include(renderer_dir.join("rive_vk_bootstrap/include"))
        .file(shim_dir.join("rive_shim.cpp"));

    let bootstrap_src = renderer_dir.join("rive_vk_bootstrap/src");
    for src in BOOTSTRAP_SOURCES {
        let path = bootstrap_src.join(src);
        assert!(path.exists(), "missing bootstrap source: {}", path.display());
        build.file(&path);
    }
    // cc emits `cargo:rustc-link-lib=static=rive_shim` + its search path FIRST,
    // so the shim precedes the rive libs in the link line.
    build.compile("rive_shim");

    // ---- Step 4: link directives for the rive-runtime libs + system libs. ---
    println!("cargo:rustc-link-search=native={}", rive_out.display());
    for lib in RIVE_LIBS {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    // Vulkan loader (dynamic; rive uses VK_NO_PROTOTYPES + a runtime loader),
    // the C++ runtime, and libc companions. stdc++ must follow the C++ archives.
    println!("cargo:rustc-link-lib=dylib=vulkan");
    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rustc-link-lib=dylib=m");
}

fn env_var(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing env var {key}"))
}

fn ensure_submodule_present(rive_root: &Path) {
    if !rive_root.join("renderer/premake5.lua").exists() {
        panic!(
            "rive-runtime submodule not found at {}.\n\
             Run:  git submodule update --init --recursive",
            rive_root.display()
        );
    }
}

/// Resolves premake5 from `$PREMAKE5`, then the vendored `tools/premake5`, then
/// `PATH`. premake-on-Linux is the least-trodden path, so the binary is vendored.
fn find_premake5(workspace_root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("PREMAKE5") {
        return PathBuf::from(p);
    }
    let vendored = workspace_root.join("tools/premake5");
    if vendored.is_file() {
        return vendored;
    }
    if command_succeeds("premake5", &["--version"]) {
        return PathBuf::from("premake5");
    }
    panic!(
        "premake5 not found. Expected the vendored binary at {} (run \
         tools/fetch_premake.sh or see BUILD.md), or premake5 on PATH, or set \
         $PREMAKE5. rive-runtime requires premake 5.0.0-beta2+.",
        workspace_root.join("tools/premake5").display()
    );
}

/// Fails the build with one actionable message listing every missing tool.
fn require_build_tools() {
    // (binary, version-probe args, apt package, why it is needed)
    let tools: &[(&str, &[&str], &str, &str)] = &[
        ("clang", &["--version"], "clang", "C++ compiler (rive uses clang vector builtins; gcc is unsupported)"),
        ("clang++", &["--version"], "clang", "C++ compiler driver for the shim"),
        ("make", &["--version"], "make", "runs the generated gmake2 Makefiles + the shader build"),
        ("python3", &["--version"], "python3", "rive's offline shader minify/codegen"),
        ("glslangValidator", &["--version"], "glslang-tools", "compiles rive's Vulkan GLSL to SPIR-V"),
        ("spirv-opt", &["--version"], "spirv-tools", "optimizes rive's SPIR-V shaders"),
        ("git", &["--version"], "git", "premake clones Vulkan-Headers/VMA/etc. at configure time"),
    ];
    let missing: Vec<&(&str, &[&str], &str, &str)> = tools
        .iter()
        .filter(|(bin, args, _, _)| !command_succeeds(bin, args))
        .collect();
    if !missing.is_empty() {
        let mut msg = String::from("Missing required build tools:\n");
        let mut pkgs = Vec::new();
        for (bin, _, pkg, why) in &missing {
            msg.push_str(&format!("  - {bin}: {why}\n"));
            pkgs.push(*pkg);
        }
        pkgs.sort_unstable();
        pkgs.dedup();
        msg.push_str(&format!("\nOn Ubuntu: sudo apt-get install -y {}\n", pkgs.join(" ")));
        msg.push_str("(libvulkan-dev provides the Vulkan loader dev symlink for linking.)");
        panic!("{msg}");
    }

    // Vulkan loader dev symlink (libvulkan.so) is needed to link -lvulkan.
    let has_loader = [
        "/usr/lib/x86_64-linux-gnu/libvulkan.so",
        "/usr/lib/libvulkan.so",
        "/lib/x86_64-linux-gnu/libvulkan.so",
    ]
    .iter()
    .any(|p| Path::new(p).exists())
        || std::env::var("VULKAN_SDK").is_ok();
    if !has_loader {
        panic!(
            "Vulkan loader dev symlink (libvulkan.so) not found.\n\
             On Ubuntu: sudo apt-get install -y libvulkan-dev"
        );
    }
}

fn command_succeeds(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run(cmd: &mut Command, label: &str) {
    eprintln!("[rive-renderer-sys] {label}: {cmd:?}");
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{label}`: {e}"));
    if !status.success() {
        panic!("`{label}` failed with status {status}. Command: {cmd:?}");
    }
}

fn verify_archives(rive_out: &Path) {
    // premake prefixes "lib", so libpng/libjpeg/libwebp get a double prefix.
    let files = [
        "librive.a",
        "librive_pls_renderer.a",
        "librive_decoders.a",
        "liblibpng.a",
        "libzlib.a",
        "liblibjpeg.a",
        "liblibwebp.a",
        "librive_harfbuzz.a",
        "librive_sheenbidi.a",
        "librive_yoga.a",
    ];
    let missing: Vec<&str> = files
        .iter()
        .copied()
        .filter(|f| !rive_out.join(f).exists())
        .collect();
    if !missing.is_empty() {
        panic!(
            "rive-runtime build did not produce: {missing:?}\nin {}",
            rive_out.display()
        );
    }
}
