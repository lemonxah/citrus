// intel_tex_2 vendors a prebuilt C++ ASTC kernel that references libstdc++
// symbols (e.g. __gxx_personality_v0) but doesn't emit a link directive for it
// on unix, so the final binary fails to link. We don't use ASTC, but the symbol
// lives in the same archive; pull in the C++ runtime to resolve it.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("unix") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}
