// intel_tex_2 vendors a prebuilt C++ ASTC encoder (tex_cache.rs calls it for
// mobile textures) that references the C++ runtime (e.g. __gxx_personality_v0)
// but doesn't emit a link directive for it on unix, so pull the runtime in.
// Android's system libstdc++.so lacks the NDK libc++ symbols that archive
// needs (std::__ndk1, __cxa_throw), so the shared library would fail to load
// on device: link the NDK's static libc++ instead (nothing extra to ship).
// `-bundle` defers the lookup to the final link, where the NDK clang finds
// the libraries in its sysroot.
fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS");
    if os.as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=static:-bundle=c++_static");
        println!("cargo:rustc-link-lib=static:-bundle=c++abi");
    } else if os.as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("unix") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}
