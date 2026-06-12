//! Vulkan instance, surface, and device bootstrap.

use std::ffi::{CStr, c_char, c_void};

use anyhow::{Context as _, Result, bail};
use ash::{ext, khr, vk};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

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
}

impl GpuContext {
    pub fn new(
        display: RawDisplayHandle,
        window: RawWindowHandle,
        app_name: &CStr,
    ) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("loading Vulkan library")?;

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
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .context("creating Vulkan instance")?;

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

        let (physical_device, queue_family) = pick_device(&instance, &surface_loader, surface)?;
        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        let gpu_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) };
        tracing::info!(gpu = ?gpu_name, "selected physical device");

        let queue_priorities = [1.0f32];
        let queue_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&queue_priorities)];
        let device_extensions = [khr::swapchain::NAME.as_ptr()];
        let mut vk13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&device_extensions)
            .push_next(&mut vk13);
        let device = unsafe { instance.create_device(physical_device, &device_info, None) }
            .context("creating logical device")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

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

fn has_validation_layer(entry: &ash::Entry) -> bool {
    let Ok(layers) = (unsafe { entry.enumerate_instance_layer_properties() }) else {
        return false;
    };
    layers.iter().any(|l| {
        let name = unsafe { CStr::from_ptr(l.layer_name.as_ptr()) };
        name == VALIDATION_LAYER
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
