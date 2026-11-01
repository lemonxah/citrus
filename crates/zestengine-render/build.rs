use std::env;
use std::path::Path;
use std::process::Command;

/// Strips bindgen's `# [doc = "..."]` attributes. The FFX headers are Doxygen
/// commented, and rustdoc tries to compile those doc strings as doctests (the
/// `@param jitterPhaseCount ...` lines are not valid Rust). Removing them keeps
/// `cargo test --doc` green and shrinks the generated source.
fn strip_bindgen_docs(mut src: &str) -> String {
    const MARK: &str = "# [doc = \"";
    let mut out = String::with_capacity(src.len());
    while let Some(pos) = src.find(MARK) {
        out.push_str(&src[..pos]);
        let after = &src[pos + MARK.len()..];
        let bytes = after.as_bytes();
        let mut i = 0;
        // Walk the Rust string literal, honoring backslash escapes.
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => {
                    i += 1;
                    break;
                }
                _ => i += 1,
            }
        }
        // Skip the closing `]` that terminates the attribute.
        if i < bytes.len() && bytes[i] == b']' {
            i += 1;
        }
        src = &after[i..];
    }
    out.push_str(src);
    out
}

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed=shaders");
    // Android (Quest) devices may only offer Vulkan 1.1, which loads SPIR-V
    // up to 1.3; desktop keeps the 1.3 environment.
    let target_env = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        "--target-env=vulkan1.1"
    } else {
        "--target-env=vulkan1.3"
    };

    for name in [
        "standard.vert",
        "standard.frag",
        "error.frag",
        "outline.vert",
        "outline.frag",
        "skybox.vert",
        "skybox.frag",
        "shadow.vert",
        "fullscreen.vert",
        "post.frag",
        // Mip-chain bloom (prefilter, downsample, upsample).
        "bloom.comp",
        // Deferred SSR resolve (fullscreen, current-frame screen-space reflections).
        "ssr_resolve.frag",
        // Deferred projected decals (fullscreen box projectors over opaque geometry).
        "decal.frag",
        // Underwater view fog / caustics over the HDR scene.
        "underwater.frag",
        // VR overlay quad (left-hand UI panel + pointer markers in the eye images).
        "vr_quad.vert",
        "vr_quad.frag",
        // Ray-traced reflections (1 bounce, ray query against the scene TLAS).
        "rt_reflect.comp",
        // Displaced water positions for the RT reflections' water BLAS refit.
        "water_displace.comp",
        // Shallow-water simulation of the interactive water layer.
        "water_sim.comp",
        // FFT line passes of the water simulation's dispersive layer.
        "water_fft.comp",
        // Lighting bake (compiled only used when ray query is available).
        "bake_gbuffer.vert",
        "bake_gbuffer.frag",
        "bake_lightmap.comp",
        "bake_probe.comp",
        // Software (SDF) GI probe march. GPU compute over the Global Distance Field.
        "sw_gi.comp",
        // Screen-space GI final gather. Per-pixel GDF trace (screen-probe final gather).
        "screen_gi.comp",
        // Probe-space GI denoise (à-trous, variance-driven probe-space spatial filter).
        "flux_denoise.comp",
        // Full-res screen-probe integrate (per-pixel-normal SH resolve of the gather).
        "flux_integrate.comp",
        // Hardware ray-query screen-probe gather (RT-core GI backend; selected when
        // the device exposes ray-query, SDF screen_gi.comp is the fallback).
        "screen_gi_rt.comp",
        // GPU skinning for Animator-driven meshes.
        "skin.comp",
        // Built-in unlit material (world-space UI, particles).
        "unlit.frag",
        // GPU-driven culling + its Hi-Z depth pyramid.
        "gpu_cull.comp",
        "hiz_build.comp",
    ] {
        let src = format!("shaders/{name}");
        let dst = Path::new(&out_dir).join(format!("{name}.spv"));
        let output = Command::new("glslc")
            .arg(target_env)
            .arg("-O")
            .arg(&src)
            .arg("-o")
            .arg(&dst)
            .output()
            .expect("failed to run glslc — is shaderc/glslc installed?");
        if !output.status.success() {
            panic!(
                "glslc failed for {src}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // Multiview variants (`<name>.mv.spv`): both XR eyes in one pass, the
    // eye picked by gl_ViewIndex.
    for name in ["standard.vert", "standard.frag", "skybox.vert"] {
        let src = format!("shaders/{name}");
        let dst = Path::new(&out_dir).join(format!("{name}.mv.spv"));
        let output = Command::new("glslc")
            .arg(target_env)
            .arg("-DMULTIVIEW")
            .arg("-O")
            .arg(&src)
            .arg("-o")
            .arg(&dst)
            .output()
            .expect("failed to run glslc — is shaderc/glslc installed?");
        if !output.status.success() {
            panic!("glslc failed for {src} (multiview):\n{}", String::from_utf8_lossy(&output.stderr));
        }
    }

    // FidelityFX FSR 3.0.4: link the prebuilt static libs and generate the FFX
    // FFI bindings from the vendored public headers.
    let ffx = format!("{}/third_party/ffx", env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed={ffx}");
    println!("cargo:rustc-link-search=native={ffx}/lib");
    for lib in [
        "ffx_backend_vk_x64",
        "ffx_fsr3upscaler_x64",
        "ffx_fsr3_x64",
        "ffx_frameinterpolation_x64",
        "ffx_opticalflow_x64",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    // The FSR libs are C++; pull in the C++ runtime (and the math/pthread it needs).
    println!("cargo:rustc-link-lib=dylib=stdc++");
    // The FFX Vulkan backend references loader entry points (vkGetPhysicalDeviceProperties2,
    // vkGetDeviceProcAddr, ...) directly rather than through ash's runtime-loaded pointers,
    // so the final binary must link the Vulkan loader.
    println!("cargo:rustc-link-lib=dylib=vulkan");

    let bindings = bindgen::Builder::default()
        .header(format!("{ffx}/include/FidelityFX/host/ffx_fsr3upscaler.h"))
        .header(format!("{ffx}/include/FidelityFX/host/ffx_fsr3.h"))
        .clang_args(["-x", "c++", "-std=c++17", "-DFFX_GCC"])
        .clang_arg(format!("-I{ffx}/include"))
        .allowlist_type("Ffx.*")
        // `FfxInterface` is a self-referential function-pointer table (its callbacks
        // take `FfxInterface*`), which bindgen cannot model as a struct; it becomes an
        // opaque blob. `.opaque_type` lets clang still report the correct size/alignment
        // (152 bytes, 8-byte aligned), so every context description embedding it by value
        // stays ABI-correct.
        .opaque_type("FfxInterface")
        // These context types are just opaque `uint32_t data[N]` buffers the app
        // allocates; bindgen turns them into sized blobs (clang reports the size).
        .opaque_type("FfxFrameInterpolationContext")
        .opaque_type("FfxOpticalflowContext")
        .allowlist_function("ffx.*")
        .allowlist_var("FFX_.*")
        .generate()
        .expect("bindgen failed on the FFX headers");
    let bindings_src = strip_bindgen_docs(
        &bindings
            .to_string()
            // Rust 2024 requires `unsafe` on extern blocks; bindgen's function-pointer
            // typedefs already carry it, so only touch the block-open form.
            .replace("extern \"C\" {", "unsafe extern \"C\" {"),
    );
    std::fs::write(Path::new(&out_dir).join("ffx_bindings.rs"), bindings_src)
        .expect("write ffx bindings");
}
