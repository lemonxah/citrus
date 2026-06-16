use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed=shaders");

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
        // Deferred SSR resolve (fullscreen, current-frame screen-space reflections).
        "ssr_resolve.frag",
        // VR overlay quad (left-hand UI panel + pointer markers in the eye images).
        "vr_quad.vert",
        "vr_quad.frag",
        // Ray-traced reflections (1 bounce, ray query against the scene TLAS).
        "rt_reflect.comp",
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
    ] {
        let src = format!("shaders/{name}");
        let dst = Path::new(&out_dir).join(format!("{name}.spv"));
        let output = Command::new("glslc")
            .arg("--target-env=vulkan1.3")
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
}
