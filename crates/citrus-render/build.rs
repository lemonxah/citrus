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
        // Lighting bake (compiled only used when ray query is available).
        "bake_gbuffer.vert",
        "bake_gbuffer.frag",
        "bake_lightmap.comp",
        "bake_probe.comp",
        // Software (SDF) GI probe march — GPU compute over the Global Distance Field.
        "sw_gi.comp",
        // Screen-space GI final gather — per-pixel GDF trace (Lumen-style).
        "screen_gi.comp",
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
