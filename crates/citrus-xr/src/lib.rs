//! citrus-xr: OpenXR integration (stub).
//!
//! Planned shape (milestone M4):
//! - `XrContext`: instance + system, negotiates the Vulkan device with
//!   `XR_KHR_vulkan_enable2` so citrus-render and OpenXR share one VkDevice.
//! - `XrSession`: session lifecycle, reference spaces, swapchains
//!   (one per eye or multiview), frame wait/begin/end loop.
//! - `XrInput`: action sets for hands/controllers, mapped into engine input.
//!
//! Runtime targets: Monado and SteamVR's OpenXR runtime. No OpenVR path —
//! native OpenXR only (xrizer is for running OpenVR apps on OpenXR, which
//! we don't need).
