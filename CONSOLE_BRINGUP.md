# ZestEngine Console Bring-up Plan

> **Status:** groundwork only. No console SDK or devkit is available, so this
> document is the audit + the target-side plan, not an implementation. It does
> not invent SDK code or fake integration. Every code reference below is a real
> file/line in this checkout (audited 2026-11-01).

The engine is a single Vulkan/ash renderer behind a `winit` window, with a
`std`-heavy runtime. A console port is therefore **not** a "new platform"
feature flag; it is a small set of backend seams plus per-console sysroot and
packaging work. The good news from the audit: the asset/component/input *model*
layers are already platform-neutral, and the engine already carries the two
important patterns it will need (Android's `cfg(target_os = "android")` seams and
the runtime GPU-feature probing in `context.rs`). The bad news: windowing,
presentation, audio, gamepad, networking and some file I/O are desktop-shaped,
and the renderer's FSR3 linkage is hard-coded x86_64.

---

## 1. Current platform architecture (what already exists)

| Layer | Where | Platform handling today |
|---|---|---|
| Workspace | `Cargo.toml` | `members = ["crates/*", "plugins/*"]`; shared `[workspace.dependencies]` |
| Engine runtime | `crates/zestengine-engine` | `cfg(target_os = "android")` entry points and log writer |
| Renderer | `crates/zestengine-render` | `raw-window-handle` surface bootstrap + runtime Vulkan feature probes |
| Assets | `crates/zestengine-assets` | `cfg(target_os = "macos")` path helpers, host tool `glslc` |
| XR | `crates/zestengine-xr` | OpenXR loader, `cfg(target_os = "android")` init |
| Editor bin | `crates/zestengine` / `crates/zestengine-hub` | `winit` desktop shells |
| Plugin hot-load | `crates/zestengine-engine/src/plugins.rs` | `libloading` dylib; static-link in shipped games |
| Packaging | `crates/zestengine-engine/src/bundle.rs`, `tools/android/build_apk.sh`, `pkg/*` | Host / Windows / Android / Quest |

The platform split is already partially present and is the template to extend:

- **Android** — `run_game_android` (`crates/zestengine-engine/src/runtime.rs:159`),
  `android_assets_root` (`runtime.rs:179`), `Logcat` log writer
  (`runtime.rs:1487`), `request_android_permissions` (`runtime.rs:1535`),
  JNI deps (`crates/zestengine-engine/Cargo.toml`).
- **Windows** — `BuildTarget::Windows` cross-build (`crates/zestengine-engine/src/bundle.rs:37`),
  `check_xwin` (`bundle.rs:859`), `crt-static` (`bundle.rs:334`).
- **macOS** — path helpers in `crates/zestengine-assets/src/launcher.rs:139`,
  `recent_projects.rs:26`, `content.rs` (`crates/zestengine-engine/src/content.rs:173`).

---

## 2. Proposed cfg layering for console targets

Do **not** fight the Rust target machinery; layer it. Four layers, each with a
single job:

### Layer 0 — target triple (Cargo)

Rust has no built-in console triples, so each console needs a **custom target
specification JSON** (a `--target <spec>.json` file checked into the repo, e.g.
under `platform/<console>/<triple>.json`) plus, for the vendor SDK, the
SDK-provided C/C++ compiler, linker and sysroot. The custom target spec must set
a distinguishable `vendor` / `os` so higher layers can key off it. Suggested
scheme (names are placeholders until the SDK confirms the real triple):

- Xbox Series X|S / One (GDK): build as a Windows-family target with a custom
  `target_vendor` (e.g. `"ms"` stays, but a dedicated spec distinguishes GDK from
  plain desktop Windows).
- PlayStation: custom target spec (e.g. `aarch64`-family), custom `vendor`.
- Switch: custom target spec (ARM64), custom `vendor`.

This is exactly how the Android support is already wired: the standard
`aarch64-linux-android` triple is the trigger (`Cargo.toml` target deps,
`build.rs` `CARGO_CFG_TARGET_OS`).

### Layer 1 — Cargo features for *capability*, not platform

Features must describe what code can do, so a console crate and a desktop crate
can still share the runtime. Keep the existing `editor` / `steam-audio`
features and add:

- `platform-console` — pulls the console backend module set, keeps the editor out.
- `fsr3` (optional, default on x86_64) — isolates the FSR linkage so non-x64
  consoles/Android don't have to link `_x64` static libs (see §4.3).
- `platform-audio-sdk` / `platform-audio-cpal` — audio sink selection.
- `platform-net-sdk` / `platform-net-std` — socket transport selection.

Never key *runtime behaviour* off a raw `cfg(target_os = "...")` when a feature
expresses the same intent; keep the `target_os` checks for the places that
genuinely differ per OS (paths, process spawning, loader names).

### Layer 2 — `build.rs` emitting a dedicated cfg

Rust's `--cfg` flag plus `cargo:rustc-cfg=` in `build.rs` is the correct way to
introduce console markers without inventing fake `target_os` values in `std`.
Emit one stable cfg per console:

```
cargo:rustc-cfg=zestengine_console="xbox"     # or "playstation" / "switch"
```

Consoles then use `#[cfg(zestengine_console = "xbox")]` in source, and the
build script derives it from `CARGO_CFG_TARGET_VENDOR`/`CARGO_CFG_TARGET_ARCH`
plus the SDK's environment. This mirrors how `crates/zestengine-render/build.rs:43`
already reads `CARGO_CFG_TARGET_OS` to pick the SPIR-V target env, and how
`crates/zestengine-assets/build.rs:10-29` reads `CARGO_CFG_TARGET_OS` /
`CARGO_CFG_TARGET_FAMILY` / `CARGO_CFG_TARGET_ARCH` to pick astcenc flags.

### Layer 3 — runtime feature detection (keep, extend)

The renderer already probes the GPU instead of assuming: instance/device API
version, dynamic rendering, synchronization2, multiview, ASTC-LDR/HDR, BC,
RGB9E5, descriptor indexing, ray query, device fault — all in
`crates/zestengine-render/src/context.rs:118-460`. Consoles expose a fixed,
known GPU, so the same probes still work and should be extended with a
console-specific "known device" table rather than removed. `RenderTier::for_platform`
(`crates/zestengine-render/src/tier.rs:19-29`) is the runtime quality gate and
needs a console arm.

### Backend seam (the actual refactor)

The cfg layering only helps if the platform-dependent code sits behind small
traits. The audit shows the seams are mostly already in the right place, with
two exceptions (window/present and the FSR link):

1. **Window + present surface** — today `Renderer` is constructed from an
   `Arc<winit::window::Window>` and derives a `RawWindowHandle` /
   `RawDisplayHandle` (`crates/zestengine-render/src/lib.rs:181,3580-3593`,
   `crates/zestengine-render/src/context.rs:123-206`). A console has no WSI
   window or `VK_KHR_swapchain`; it presents to the system compositor/display.
   Introduce a `SurfaceProvider`/`PresentTarget` trait so the renderer can get
   (a) a surface handle and (b) an acquire/present entry point from either
   winit or the console SDK.
2. **Audio sink** — `rodio::OutputStream` (`crates/zestengine-engine/src/audio.rs:21`)
   is the only output path. Add an audio-backend trait so the mixer's
   `AudioCue` stream (`audio.rs:29-53`) can feed either cpal or the SDK mixer.
3. **Input** — already half-done: `zestengine-core/src/input.rs` is explicitly
   winit/gilrs-free and the `InputManager` merely translates winit/gilrs into
   `RawInput` (`crates/zestengine-engine/src/input_engine.rs:14-38`). Add a
   `PlatformInput` source trait; the console pad API feeds the same `RawInput`.
4. **Networking** — already trait-shaped: `NetSession` depends on a `Socket`
   trait (`crates/zestengine-engine/src/net.rs:174-176`) with a
   `std::net::UdpSocket` implementation (`net.rs:215-258`). A console socket
   backend implements the same trait.
5. **File I/O** — keep `std::fs` for the *build machine* (asset cook, pak
   packing) and route all *device* reads through the existing pak/mount path
   (`crates/zestengine-assets/src/game_pak.rs`, `runtime.rs:131-149`) plus a
   console mount root. Never rely on `std::env::current_exe` / `current_dir` on
   device (`runtime.rs:137`, `launcher.rs:56`, `recent_projects.rs:51`).

---

## 3. Per-platform integration points (concrete)

### 3.1 Windowing / event loop

- `crates/zestengine-engine/src/runtime.rs:15-19` — `winit` event types are the
  runtime's entire input/lifecycle vocabulary.
- `crates/zestengine-engine/src/runtime.rs:210-289` — `run_game_on` owns the
  `EventLoop` and `GameApp`.
- `crates/zestengine-engine/src/runtime.rs:437-474` — `GameApp::init` calls
  `event_loop.create_window(attrs)` then builds the renderer from that window.
- `crates/zestengine-engine/src/editor_app.rs:19-27,14262-14621` — the editor is
  a `winit::application::ApplicationHandler`; **editor never ships on console**,
  so this stays desktop-only behind `feature = "editor"`.
- `crates/zestengine-hub/src/app.rs:9-13` — launcher window, also desktop-only.

**Console work:** add a `PlatformApp`/`PlatformLoop` shim that owns the SDK's
message/loop callbacks and drives the same `GameApp` update/render path used by
`run_game_on`. The Android entry (`runtime.rs:159-187`) is the closest existing
shape: SDK-owned app object → config → `run_game_on`-equivalent.

### 3.2 Vulkan instance / device / surface / present

- `crates/zestengine-render/src/context.rs:118-122` — `GpuContext::new(display, window, app_name, xr)`.
- `context.rs:145-206` — instance creation: `ash::Entry::load()`,
  `ash_window::enumerate_required_extensions`, `ash_window::create_surface`.
- `context.rs:209-460` — physical device pick + feature/extension negotiation.
- `crates/zestengine-render/src/swapchain.rs:18-96` — `VK_KHR_swapchain`
  acquire/present, present mode, surface format.
- `crates/zestengine-render/src/surface_lifecycle.rs:11-60` — surface lost /
  recreated (Android lifecycle).

**Console work:**
- Replace `ash::Entry::load()` (which loads the desktop `libvulkan`/loader) with
  the SDK-provided loader or direct driver entry points.
- Replace WSI surface (`ash_window::create_surface`) with the console display
  path; present becomes the SDK frame submission rather than
  `vkQueuePresentKHR`.
- Keep the feature probes; add a fixed "known GPU capabilities" table per
  console so the tier and format decisions are deterministic.
- The two surface entry points to abstract are `context.rs:206` and
  `surface_lifecycle.rs:37` (both call `ash_window::create_surface`), plus the
  swapchain's present call in `swapchain.rs`.

### 3.3 Graphics API reality check (per console)

`zestengine-render` is Vulkan-only (`ash` everywhere). This determines the cost
per platform:

- **Switch** exposes a proprietary low-level API (NVN); Vulkan is not the
  shipping API. This is the largest renderer port: the `context`/`swapchain`/
  `pipeline`/`frame` modules need a non-ash backend. The shader pipeline
  (`glslc → SPIR-V`) also needs an offline NVN shader step.
- **Xbox (GDK)** is Direct3D 12; HLSL/DXIL. The engine already *authors* HLSL
  custom shaders (`crates/zestengine-assets/src/shader_file.rs:952-955`,
  `ShaderLang::Hlsl`) but compiles them to SPIR-V, so the source-language
  plumbing exists and a DXIL compiler backend can reuse it.
- **PlayStation** is the proprietary graphics API + AGC/PSSL shader language;
  again the shader *source* plumbing exists but the compile target changes.

In short: the engine's abstraction currently stops at "Vulkan", so the first
real renderer step on every console is a `GfxApi` backend, not a per-platform
`#[cfg]` sprinkled over `ash::vk::*` calls.

### 3.4 Shaders / SPIR-V

- `crates/zestengine-render/build.rs:41-46` — engine shaders compiled at build
  time to `.spv` via `glslc`; `--target-env=vulkan1.1` on Android, else 1.3.
- `crates/zestengine-render/build.rs:47-136` — the fixed engine shader list.
- `crates/zestengine-assets/src/shader_file.rs:925-967` — `ShaderTargetEnv`
  (Vulkan 1.3 / 1.1) and `glslc` invocation for custom shaders.
- `crates/zestengine-assets/src/shader_file.rs:977-1008` — `.spv` sidecar + baked
  variant sidecars (so shipped games don't need `glslc`).
- `crates/zestengine-engine/src/bundle.rs:399-440` — `stage_assets` cooks with a
  scoped `ShaderTargetEnv::Vulkan1_1` for Android/Quest, then `precompile_shaders`
  (`bundle.rs:880-894`).

**Console work:**
- Add a `ShaderTargetEnv`-style per-platform compile target (DXIL / console IR)
  alongside the two existing Vulkan envs.
- Keep offline compilation (never ship a runtime compiler to a console).
- `precompile_shaders` and `bake_project_variants` already produce per-variant
  blobs; the console step is a different compiler, same pipeline.

### 3.5 Allocator / GPU memory

- `crates/zestengine-render/src/alloc.rs:6-13` — `gpu_allocator::vulkan::Allocator`
  with `AllocationScheme::GpuAllocatorManaged`; `Buffer::new` (`alloc.rs:12-44`)
  and `upload_buffer` (`alloc.rs:94+`) are the only allocation funnel.
- `context.rs` MarkerBuffer maps host-visible memory directly (`context.rs:66-95`).

**Console work:**
- Consoles require explicit memory budgets and lifetime rules; replace the
  gpu-allocator backend with a per-console allocator implementing the same
  allocate/free surface, and add a device memory budget signal (a
  `set_memory_budget`/`on_low_memory` hook) that the renderer currently lacks.
- There is **no custom global Rust allocator** anywhere (no
  `#[global_allocator]`); a console target typically needs one plus a startup
  memory reservation, which is a new crate-level item.

### 3.6 Audio

- `crates/zestengine-engine/src/audio.rs:21` — `rodio::OutputStream` is the sole
  sink; `AudioCue` is the frame-decoupled source list (`audio.rs:29-53`).
- `crates/zestengine-engine/src/steam_audio.rs:1-40` — Steam Audio via
  `audionimbus`/`libphonon`; `audionimbus`'s `auto-install` fetches **desktop
  builds only** (Cargo.toml note in the workspace file).
- `crates/zestengine-engine/src/voice_codec.rs:44-49` — `libopus` is dlopen'ed by
  name (`libopus.so.0` etc.) with a PCM fallback.

**Console work:**
- Add an audio sink trait behind `OutputStream`; the console SDK mixer consumes
  the same 48 kHz mixed stream (`steam_audio.rs` `SAMPLE_RATE`).
- Steam Audio ships console builds only through the vendor; gate it off or
  replace with the SDK's spatialization until a console `libphonon` is licensed.
- Voice codec: statically link a console-licensed Opus (or the SDK codec)
  instead of dlopen names.

### 3.7 Input

- `crates/zestengine-core/src/input.rs` — abstract `RawInput`/`Key`/`PadButton`/
  `PadAxis` model (deliberately winit/gilrs-free).
- `crates/zestengine-engine/src/input_engine.rs:14-38` — `InputManager` feeds
  from winit `KeyCode`/`MouseButton` and `gilrs::Gilrs`.

**Console work:** a `PlatformInput` source trait; console pad/keyboard/touch go
through the SDK and emit the same `RawInput`. `gilrs` is desktop-only and must
not be a hard dependency on console (it is currently an unconditional
dependency in `crates/zestengine-engine/Cargo.toml`).

### 3.8 File I/O / packaging / asset cook

- `crates/zestengine-engine/src/bundle.rs:27-106` — `BuildTarget` enum
  `{Host, Windows, Android, Quest}`; add console variants.
- `bundle.rs:300-395` — `build_game` runs `cargo` on the host, stages assets,
  packs `assets.pak`, ships the executable + Steam Audio lib.
- `bundle.rs:759-828` — `build_android` generates the `android_main` crate and
  calls `tools/android/build_apk.sh`; the console equivalent is an SDK package
  step (not bash).
- `crates/zestengine-assets/src/game_pak.rs` — `write_game_pak`/`extract_game_pak`;
  the on-device extraction pattern already exists.
- `crates/zestengine-engine/src/runtime.rs:131-149` — `shipped_pak_root` uses
  `std::env::current_exe()`; consoles must use a mount root instead.
- `crates/zestengine-assets/src/launcher.rs:56` and `recent_projects.rs:51` —
  `current_exe`/`current_dir` discovery (desktop/host only).
- `crates/zestengine-assets/build.rs:10-29` — astcenc C++ runtime linkage is
  keyed to `CARGO_CFG_TARGET_OS`/`TARGET_FAMILY`; each console needs its own
  runtime-link arm (static, not `dylib=stdc++`).

**Console work:** asset *cooking* stays on the host (`stage_assets` +
`precompile_shaders`); asset *delivery* is the console package/mount, replacing
`assets.pak`-beside-exe and the `android_assets_root` extraction. Device-side
config/cache dirs (`recent_projects.rs:18-34`, `content.rs:170-189`,
`input_engine.rs:314-335`) need a console profile path.

### 3.9 Networking

- `crates/zestengine-engine/src/net.rs:174-176` — `Socket` trait
  (`send`/`recv`/`local_addr`) is already the transport abstraction.
- `net.rs:215-258` — `std::net::UdpSocket` implementation.
- `crates/zestengine-relay/src/server.rs` — the relay runs on a *server*, so it
  needs no console port (desktop-only deployment).
- `net_identity.rs:86-96` — Unix-only `PermissionsExt` is already cfg-gated.

**Console work:** a console `Socket` backend (SDK sockets / platform network
API) implementing the existing trait; platform network initialization (NAT,
login, title services) wraps `NetSession` but does not change the protocol.
This is the cheapest seam in the whole port.

### 3.10 Threading / tasks

- `std::thread`/`std::thread::scope` used in cooks and workers:
  `crates/zestengine-assets/src/hdr_codec.rs:74-76`,
  `crates/zestengine-assets/src/tex_cache.rs:985-986`,
  `crates/zestengine-engine/src/bundle.rs:473,540-553` (host-side, fine),
  `crates/zestengine-engine/src/content.rs:639-641`,
  `crates/zestengine-engine/src/steam_audio.rs` (worker thread).
- `std::thread::available_parallelism()` at `bundle.rs:473` (host cook).

**Console work:** consoles pin worker threads and set priorities; wrap
`std::thread::Builder` in a `spawn_worker` helper and register audio/network
threads with the SDK. `std::time::{Instant,SystemTime}` and
`UNIX_EPOCH` are used widely and are fine on device, but wall-clock
`SystemTime` should be replaced with the SDK monotonic clock for net/lag logic.

---

## 4. Sequenced plan (what to do first, without an SDK)

### Phase A — make the workspace target-ready (no SDK needed)

1. **Fix the FSR3 x86_64 hard-link.** `crates/zestengine-render/build.rs:137-158`
   links `ffx_*_x64` static libs, `dylib=stdc++` and `dylib=vulkan`
   unconditionally, and `crates/zestengine-render/src/lib.rs:13,38,3284-3287`
   compile the `fsr3`/`ffx` modules unconditionally. Only
   `third_party/ffx/lib/*_x64.a` binaries ship, so any non-x64 target (Android
   aarch64 today, every console) cannot link. Gate FSR3 behind a feature or
   `cfg(target_arch = "x86_64")` in both the build script and `lib.rs`.
2. **Gate the desktop-only deps** (`gilrs`, `winit`'s window path,
   `libloading` plugin hot-load) so a console runtime crate can build
   `default-features = false` the same way `bundle.rs:700-731` already does for
   Android.
3. **Introduce the `zestengine_console` cfg** emitted from `build.rs` (Layer 2
   above), and stub the backend trait seams (surface provider, audio sink,
   input source, socket backend) with desktop implementations first so the
   refactor is a pure no-op on Linux before any SDK lands.

### Phase B — one reference console, SDK in hand

Order of implementation, cheapest to most expensive:

1. Toolchain/target spec + `BuildTarget` variant + packaging (extends
   `bundle.rs` the way `build_android` did).
2. File I/O + mount root (extends the `game_pak`/`android_assets_root` pattern).
3. Networking backend (implements the existing `Socket` trait).
4. Input backend (implements the `RawInput` feed).
5. Audio sink (rodio/cpal replacement).
6. GPU backend + present path (the `ash`/WSI seam; largest for non-Vulkan
   consoles — see §3.3).
7. Shader compiler backend (offline DXIL/console IR).
8. Certification/lifecycle (suspend/resume, achievements, storage quotas,
   entitlements) — net-new subsystem.

### Phase C — performance tier

Add a `RenderTier::Console` (or per-console arms) in
`crates/zestengine-render/src/tier.rs:19-29` once the reference device's
GPU capabilities are measured, reusing the existing Mobile-vs-Desktop knobs.

---

## 5. What is deliberately out of scope here

- No SDK headers, no devkit calls, no per-console code beyond the cfg/feature
  *shape* described above — the exact SDK APIs are unspecified until an SDK is
  present.
- No full-workspace build or cross-compile (no console target is installed).
- The editor/hub (`zestengine`, `zestengine-hub`, `feature = "editor"`) never
  ships on console; they remain desktop tools.
- UGC/WASM (`crates/zestengine-scripting`) and the relay server are
  runtime/server concerns and are only affected by the console's
  entitlements/sandbox policy, not by this port's core seams.

## 6. Key files to touch, at a glance

| Concern | File(s) |
|---|---|
| Event loop / window | `crates/zestengine-engine/src/runtime.rs`, `editor_app.rs` |
| Surface / present | `crates/zestengine-render/src/context.rs`, `swapchain.rs`, `surface_lifecycle.rs` |
| GPU feature probing | `crates/zestengine-render/src/context.rs:118-460` |
| Quality tier | `crates/zestengine-render/src/tier.rs` |
| FSR linkage (blocker) | `crates/zestengine-render/build.rs:137-158`, `lib.rs:13,38` |
| Shader compile | `crates/zestengine-render/build.rs:41-136`, `crates/zestengine-assets/src/shader_file.rs:925-1008` |
| Asset cook / packaging | `crates/zestengine-engine/src/bundle.rs`, `crates/zestengine-assets/src/game_pak.rs`, `tools/android/build_apk.sh` |
| GPU allocator | `crates/zestengine-render/src/alloc.rs` |
| Audio | `crates/zestengine-engine/src/audio.rs`, `steam_audio.rs`, `voice_codec.rs` |
| Input | `crates/zestengine-core/src/input.rs`, `crates/zestengine-engine/src/input_engine.rs` |
| Networking | `crates/zestengine-engine/src/net.rs:174-258` |
| Paths / file I/O | `crates/zestengine-assets/src/launcher.rs`, `recent_projects.rs`, `crates/zestengine-engine/src/content.rs:170-189`, `input_engine.rs:314-335` |
| C++ toolchain linkage | `crates/zestengine-assets/build.rs:10-29`, `crates/zestengine-render/build.rs` |
