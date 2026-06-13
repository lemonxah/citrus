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
