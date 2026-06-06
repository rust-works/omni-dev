//! Build script for `voxtral-sys`.
//!
//! Compiles the vendored pure-C Voxtral engine (`vendor/voxtral.c/`) with the
//! `cc` crate and generates `bindgen` FFI bindings over its public `voxtral.h`
//! API. ADR-0037 permits `cc` and/or `make`; `cc` is used here so the build
//! integrates with Cargo (`OUT_DIR`, target detection, profile-driven opt
//! level) and so we can control flags directly — most importantly to **drop
//! upstream's `-march=native`**, which bakes in host-CPU instructions that
//! `SIGILL` on a different machine and is unacceptable for a distributable
//! binary.
//!
//! Backend selection mirrors the upstream Makefile:
//! - macOS aarch64: `USE_BLAS` + `USE_METAL` (Accelerate + Metal/MPS fast path).
//! - macOS x86_64:  `USE_BLAS` (Accelerate only).
//! - Linux:         `USE_BLAS` (+ OpenBLAS `cblas.h`).
//! - Windows:       unsupported by design (ADR-0037) — the build fails loudly.

use std::env;
use std::path::{Path, PathBuf};

/// Library translation units (the CLI, mic capture, and inspector are not
/// vendored — see `vendor/README.md`).
const CORE_SRCS: &[&str] = &[
    "voxtral.c",
    "voxtral_kernels.c",
    "voxtral_audio.c",
    "voxtral_encoder.c",
    "voxtral_decoder.c",
    "voxtral_tokenizer.c",
    "voxtral_safetensors.c",
];

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");

    if target_os == "windows" {
        panic!(
            "voxtral-sys is not supported on target_os = \"windows\" \
             (ADR-0037: native Voxtral is excluded on Windows; use the \
             cross-platform `candle` backend instead)"
        );
    }
    if target_os != "macos" && target_os != "linux" {
        panic!("voxtral-sys: unsupported target_os {target_os:?} (supported: macos, linux)");
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vendor = manifest.join("vendor/voxtral.c");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    // Apple Silicon gets the Metal/MPS fast path; everything else uses BLAS.
    let use_metal = target_os == "macos" && target_arch == "aarch64";

    // --- Core C library -----------------------------------------------------
    let mut core = cc::Build::new();
    core.include(&vendor);
    for src in CORE_SRCS {
        core.file(vendor.join(src));
    }
    // No `-march=native` (non-portable). Keep upstream's fast-math numerics.
    core.flag_if_supported("-ffast-math");
    // Third-party C: don't fail our build on its warnings.
    core.warnings(false);
    core.define("USE_BLAS", None);

    if target_os == "macos" {
        // Accelerate provides cblas via `#include <Accelerate/Accelerate.h>`
        // (auto-found by clang on macOS); ACCELERATE_NEW_LAPACK matches upstream.
        core.define("ACCELERATE_NEW_LAPACK", None);
        if use_metal {
            core.define("USE_METAL", None);
        }
    } else {
        // Linux: `#include <cblas.h>` from OpenBLAS. Debian/Ubuntu place it
        // under /usr/include/openblas; add it if present (harmless otherwise).
        core.define("USE_OPENBLAS", None);
        for cand in ["/usr/include/openblas", "/usr/include/x86_64-linux-gnu"] {
            if Path::new(cand).is_dir() {
                core.include(cand);
            }
        }
    }
    core.compile("voxtral_core");

    // --- Metal (Objective-C) TU, macOS Apple-Silicon only -------------------
    if use_metal {
        // voxtral_metal.m `#include`s "voxtral_shaders_source.h", an xxd-style
        // C array of the .metal shader source (compiled at runtime via the
        // Metal API). Generate it into OUT_DIR with the exact symbol names the
        // .m expects (`voxtral_shaders_metal` / `_len`) — `xxd -i` would derive
        // the name from the file path, so we emit it ourselves.
        generate_shader_header(
            &vendor.join("voxtral_shaders.metal"),
            &out_dir.join("voxtral_shaders_source.h"),
        );

        let mut metal = cc::Build::new();
        metal.include(&vendor);
        metal.include(&out_dir); // for the generated voxtral_shaders_source.h
        metal.file(vendor.join("voxtral_metal.m"));
        metal.flag("-fobjc-arc");
        metal.flag_if_supported("-ffast-math");
        metal.warnings(false);
        metal.define("USE_BLAS", None);
        metal.define("USE_METAL", None);
        metal.define("ACCELERATE_NEW_LAPACK", None);
        metal.compile("voxtral_metal");
    }

    // --- Native libraries / frameworks --------------------------------------
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Accelerate");
        if use_metal {
            for fw in [
                "Metal",
                "MetalPerformanceShaders",
                "MetalPerformanceShadersGraph",
                "Foundation",
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
    } else {
        println!("cargo:rustc-link-lib=openblas");
    }

    // --- bindgen over the public API ----------------------------------------
    let bindings = bindgen::Builder::default()
        .header(vendor.join("voxtral.h").to_string_lossy())
        .clang_arg(format!("-I{}", vendor.display()))
        .allowlist_function("vox_.*")
        .allowlist_type("vox_.*")
        .allowlist_var("VOX_.*")
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to generate bindings for voxtral.h");
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");

    // --- Rebuild triggers ----------------------------------------------------
    println!("cargo:rerun-if-changed=build.rs");
    if let Ok(entries) = std::fs::read_dir(&vendor) {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }
}

/// Emit an xxd-`-i`-style C header embedding `metal` as
/// `unsigned char voxtral_shaders_metal[]` + `unsigned int
/// voxtral_shaders_metal_len`, the exact symbols `voxtral_metal.m` references.
fn generate_shader_header(metal: &Path, out: &Path) {
    use std::fmt::Write as _;

    let bytes =
        std::fs::read(metal).unwrap_or_else(|e| panic!("failed to read {}: {e}", metal.display()));
    let mut header = String::with_capacity(bytes.len() * 6 + 128);
    header.push_str("unsigned char voxtral_shaders_metal[] = {\n");
    for (i, b) in bytes.iter().enumerate() {
        if i % 12 == 0 {
            header.push_str("  ");
        }
        let _ = write!(header, "0x{b:02x}, ");
        if i % 12 == 11 {
            header.push('\n');
        }
    }
    header.push_str("\n};\n");
    let _ = writeln!(
        header,
        "unsigned int voxtral_shaders_metal_len = {};",
        bytes.len()
    );
    std::fs::write(out, header)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out.display()));
}
