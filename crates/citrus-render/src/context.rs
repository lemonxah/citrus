//! Vulkan instance, surface, and device bootstrap.

use std::ffi::{CStr, c_char, c_void};

use anyhow::{Context as _, Result, bail};
use ash::vk::Handle as _;
use ash::{ext, khr, vk};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

/// When a VR headset is present, the renderer's `VkInstance`/`VkDevice` MUST be
/// created via OpenXR (`xrCreateVulkanInstanceKHR` / `xrGetVulkanGraphicsDevice2KHR`
/// / `xrCreateVulkanDeviceKHR`) so the runtime records our `vkGetInstanceProcAddr`
/// and the session can share this device. `None` = normal flat creation.
pub struct XrBootstrap<'a> {
    pub instance: &'a openxr::Instance,
    pub system: openxr::SystemId,
}

/// Owns the core Vulkan objects every other renderer module hangs off of.
pub struct GpuContext {
    /// Never read, but must outlive everything: it owns the loaded libvulkan.
    #[allow(dead_code)]
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    debug: Option<(ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    pub surface_loader: khr::surface::Instance,
    pub surface: vk::SurfaceKHR,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue_family: u32,
    pub queue: vk::Queue,
    /// Background asset-upload queue (same family, index 1) for the loader
    /// thread; None when the family exposes only one queue.
    pub transfer_queue: Option<vk::Queue>,
    /// Acceleration-structure loader. Present only when the device supports
    /// ray query (the lighting bake's ray tracing); `None` disables baking.
    // Consumed by the bake's BLAS/TLAS builders (next phase).
    #[allow(dead_code)]
    pub accel: Option<khr::acceleration_structure::Device>,
}

impl GpuContext {
    /// Whether the device can ray-trace (acceleration structures + ray query),
    /// which the lighting bake requires.
    pub fn ray_tracing(&self) -> bool {
        self.accel.is_some()
    }
}

impl GpuContext {
    pub fn new(
        display: RawDisplayHandle,
        window: RawWindowHandle,
        app_name: &CStr,
        xr: Option<XrBootstrap<'_>>,
    ) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("loading Vulkan library")?;
        // The runtime needs our vkGetInstanceProcAddr (passed into the XR-routed
        // instance/device creation below). ash's PFN is ABI-identical to OpenXR's.
        let get_proc: openxr::sys::platform::VkGetInstanceProcAddr =
            unsafe { std::mem::transmute(entry.static_fn().get_instance_proc_addr) };

        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"citrus")
            .api_version(vk::API_VERSION_1_3);

        let mut extensions: Vec<*const c_char> =
            ash_window::enumerate_required_extensions(display)?.to_vec();

        let validation = has_validation_layer(&entry);
        let mut layers: Vec<*const c_char> = Vec::new();
        if validation {
            layers.push(VALIDATION_LAYER.as_ptr());
            extensions.push(ext::debug_utils::NAME.as_ptr());
        }

        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&extensions)
            .enabled_layer_names(&layers);
        let instance = match &xr {
            // xrCreateVulkanInstanceKHR merges the runtime's required instance
            // extensions with ours and records the proc-addr the runtime needs.
            Some(b) => {
                let raw = unsafe {
                    b.instance.create_vulkan_instance(
                        b.system,
                        get_proc,
                        &instance_info as *const _ as *const c_void,
                    )
                }
                .context("xrCreateVulkanInstanceKHR")?
                .map_err(|e| anyhow::anyhow!("xrCreateVulkanInstanceKHR: VkResult {e}"))?;
                let handle = vk::Instance::from_raw(raw as usize as u64);
                unsafe { ash::Instance::load(entry.static_fn(), handle) }
            }
            None => unsafe { entry.create_instance(&instance_info, None) }
                .context("creating Vulkan instance")?,
        };

        let debug = if validation {
            let loader = ext::debug_utils::Instance::new(&entry, &instance);
            let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                .message_severity(
                    vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                        | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
                )
                .message_type(
                    vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                        | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                        | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                )
                .pfn_user_callback(Some(debug_callback));
            let messenger = unsafe { loader.create_debug_utils_messenger(&info, None)? };
            tracing::info!("Vulkan validation layer enabled");
            Some((loader, messenger))
        } else {
            tracing::warn!("VK_LAYER_KHRONOS_validation not found; running without validation");
            None
        };

        let surface_loader = khr::surface::Instance::new(&entry, &instance);
        let surface =
            unsafe { ash_window::create_surface(&entry, &instance, display, window, None) }
                .context("creating window surface")?;

        let (physical_device, queue_family) = match &xr {
            // The runtime dictates the GPU (xrGetVulkanGraphicsDevice2KHR); we just
            // find a graphics+present queue family on it.
            Some(b) => {
                let inst_ptr = instance.handle().as_raw() as usize as *const c_void;
                let pd_raw = unsafe { b.instance.vulkan_graphics_device(b.system, inst_ptr) }
                    .context("xrGetVulkanGraphicsDevice2KHR")?;
                let pd = vk::PhysicalDevice::from_raw(pd_raw as usize as u64);
                let family = present_queue_family(&instance, &surface_loader, surface, pd)
                    .context("OpenXR's required GPU has no graphics+present queue family")?;
                (pd, family)
            }
            None => pick_device(&instance, &surface_loader, surface)?,
        };
        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        let gpu_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) };
        tracing::info!(gpu = ?gpu_name, "selected physical device");

        // Request a SECOND queue from the same family (when the family exposes
        // ≥2) to use as a background asset-upload queue: a loader thread can
        // submit transfers on it independently of the render thread (distinct
        // VkQueues need no cross-synchronization; same family → no ownership
        // transfer for the uploaded resources). Falls back to one queue (uploads
        // then stay on the main thread).
        let queue_count = unsafe {
            instance
                .get_physical_device_queue_family_properties(physical_device)
                .get(queue_family as usize)
                .map(|q| q.queue_count)
                .unwrap_or(1)
        };
        let want_transfer = queue_count >= 2;
        let queue_priorities: &[f32] = if want_transfer { &[1.0, 0.5] } else { &[1.0] };
        let queue_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(queue_priorities)];

        // Ray query (for the lighting bake) needs acceleration structures +
        // ray query + deferred host ops + buffer device address. Enable them
        // only when the device advertises all of it; otherwise the engine runs
        // fine without baking.
        let rt_exts = [
            khr::acceleration_structure::NAME,
            khr::ray_query::NAME,
            khr::deferred_host_operations::NAME,
        ];
        let has_rt = device_supports(&instance, physical_device, &rt_exts)?;

        let mut device_extensions = vec![khr::swapchain::NAME.as_ptr()];
        if has_rt {
            for e in rt_exts {
                device_extensions.push(e.as_ptr());
            }
        }

        let mut vk13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let mut vk12 = vk::PhysicalDeviceVulkan12Features::default().buffer_device_address(true);
        let mut as_features = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default()
            .acceleration_structure(true);
        let mut rq_features = vk::PhysicalDeviceRayQueryFeaturesKHR::default().ray_query(true);

        // BC (S3TC/BPTC) compressed textures for the texture import cache. Core
        // on essentially all desktop GPUs; enable only if reported to avoid
        // failing device creation on the rare GPU without it.
        let supported = unsafe { instance.get_physical_device_features(physical_device) };
        let bc = supported.texture_compression_bc == vk::TRUE;
        if !bc {
            tracing::warn!("GPU lacks textureCompressionBC; textures upload uncompressed");
        }
        crate::set_bc_supported(bc);
        let features = vk::PhysicalDeviceFeatures::default().texture_compression_bc(bc);

        let mut device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&device_extensions)
            .enabled_features(&features)
            .push_next(&mut vk13);
        if has_rt {
            device_info = device_info
                .push_next(&mut vk12)
                .push_next(&mut as_features)
                .push_next(&mut rq_features);
        }
        let device = match &xr {
            // xrCreateVulkanDeviceKHR adds the runtime's required device extensions
            // (external memory/fd, timeline semaphores for the compositor hand-off)
            // on top of ours, so the session can present this device's images.
            Some(b) => {
                let raw = unsafe {
                    b.instance.create_vulkan_device(
                        b.system,
                        get_proc,
                        physical_device.as_raw() as usize as *const c_void,
                        &device_info as *const _ as *const c_void,
                    )
                }
                .context("xrCreateVulkanDeviceKHR")?
                .map_err(|e| anyhow::anyhow!("xrCreateVulkanDeviceKHR: VkResult {e}"))?;
                let handle = vk::Device::from_raw(raw as usize as u64);
                unsafe { ash::Device::load(instance.fp_v1_0(), handle) }
            }
            None => unsafe { instance.create_device(physical_device, &device_info, None) }
                .context("creating logical device")?,
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let transfer_queue = if want_transfer {
            tracing::info!("background asset-upload queue enabled");
            Some(unsafe { device.get_device_queue(queue_family, 1) })
        } else {
            tracing::warn!("only one queue available; asset uploads stay on the main thread");
            None
        };

        let accel = if has_rt {
            tracing::info!("ray query supported; lighting bake enabled");
            Some(khr::acceleration_structure::Device::new(&instance, &device))
        } else {
            tracing::warn!("ray query unsupported on this device; lighting bake disabled");
            None
        };

        Ok(Self {
            entry,
            instance,
            debug,
            surface_loader,
            surface,
            physical_device,
            device,
            queue_family,
            queue,
            transfer_queue,
            accel,
        })
    }
}

impl Drop for GpuContext {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            if let Some((loader, messenger)) = self.debug.take() {
                loader.destroy_debug_utils_messenger(messenger, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

/// Whether `pd` advertises every extension in `wanted`.
fn device_supports(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    wanted: &[&CStr],
) -> Result<bool> {
    let exts = unsafe { instance.enumerate_device_extension_properties(pd)? };
    let available: Vec<&CStr> = exts
        .iter()
        .map(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) })
        .collect();
    Ok(wanted.iter().all(|w| available.contains(w)))
}

fn has_validation_layer(entry: &ash::Entry) -> bool {
    let Ok(layers) = (unsafe { entry.enumerate_instance_layer_properties() }) else {
        return false;
    };
    layers.iter().any(|l| {
        let name = unsafe { CStr::from_ptr(l.layer_name.as_ptr()) };
        name == VALIDATION_LAYER
    })
}

/// First queue family on `pd` that supports both graphics and presenting to
/// `surface`. Used when OpenXR already chose the physical device for us.
fn present_queue_family(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pd: vk::PhysicalDevice,
) -> Option<u32> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    families.iter().enumerate().find_map(|(i, f)| {
        let graphics = f.queue_flags.contains(vk::QueueFlags::GRAPHICS);
        let present = unsafe {
            surface_loader
                .get_physical_device_surface_support(pd, i as u32, surface)
                .unwrap_or(false)
        };
        (graphics && present).then_some(i as u32)
    })
}

/// Pick the best physical device that can render to `surface`, preferring
/// discrete GPUs. Returns the device and a graphics+present queue family.
fn pick_device(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    let devices = unsafe { instance.enumerate_physical_devices()? };
    let mut best: Option<(vk::PhysicalDevice, u32, u32)> = None;

    for pd in devices {
        let exts = unsafe { instance.enumerate_device_extension_properties(pd)? };
        let has_swapchain = exts.iter().any(|e| {
            let name = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
            name == khr::swapchain::NAME
        });
        if !has_swapchain {
            continue;
        }

        let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let family = families.iter().enumerate().find_map(|(i, f)| {
            let graphics = f.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let present = unsafe {
                surface_loader
                    .get_physical_device_surface_support(pd, i as u32, surface)
                    .unwrap_or(false)
            };
            (graphics && present).then_some(i as u32)
        });
        let Some(family) = family else { continue };

        let props = unsafe { instance.get_physical_device_properties(pd) };
        let score = match props.device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => 100,
            vk::PhysicalDeviceType::INTEGRATED_GPU => 50,
            _ => 10,
        };
        if best.is_none_or(|(_, _, s)| score > s) {
            best = Some((pd, family, score));
        }
    }

    match best {
        Some((pd, family, _)) => Ok((pd, family)),
        None => bail!("no Vulkan device supports rendering to this surface"),
    }
}

extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> vk::Bool32 {
    let message = unsafe {
        let data = &*data;
        if data.p_message.is_null() {
            "<no message>".into()
        } else {
            CStr::from_ptr(data.p_message).to_string_lossy()
        }
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        tracing::error!(target: "vulkan", "{message}");
    } else {
        tracing::warn!(target: "vulkan", "{message}");
    }
    vk::FALSE
}
