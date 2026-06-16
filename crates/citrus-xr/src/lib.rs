//! citrus-xr: OpenXR bootstrap for the engine (editor + game).
//!
//! This is the *start* of the VR path: it loads the OpenXR loader at runtime,
//! creates an instance with the Vulkan-enable2 extension (so a session can later
//! share citrus-render's `VkDevice`), and queries the HMD system + its tracking
//! capabilities. It deliberately does NOT yet create the session/swapchains or
//! the frame loop — those plug in once the renderer hands over its Vulkan
//! handles. The point is that both the editor and a shipped game can call
//! [`XrContext::start`] to detect/initialise VR without any of that wired yet.
//!
//! Runtime targets: Monado and SteamVR's OpenXR runtime (SteamVR exposes Vive
//! trackers through OpenXR tracker extensions, which the full-body path will use
//! to feed `citrus_core::TrackerTargets` into the IK solver).

use anyhow::{Context as _, Result};

/// A live OpenXR instance + selected HMD system. Held by the engine for the app
/// lifetime; dropping it tears the instance down.
pub struct XrContext {
    /// The OpenXR entry (loader). Kept alive for the instance's lifetime.
    pub entry: openxr::Entry,
    pub instance: openxr::Instance,
    pub system: openxr::SystemId,
    pub system_name: String,
    pub orientation_tracking: bool,
    pub position_tracking: bool,
    /// `XR_KHR_convert_timespec_time` available → `Instance::now()` is safe.
    time_supported: bool,
    /// `XR_HTCX_vive_tracker_interaction` available → suggest tracker bindings.
    tracker_supported: bool,
}

impl XrContext {
    /// Try to start OpenXR: load the loader, create an instance (Vulkan2), and
    /// select the HMD system. Returns `Ok(None)` when VR is simply unavailable
    /// (no runtime installed, or no headset connected) so the caller can carry
    /// on flat; returns `Err` only on an unexpected runtime fault.
    pub fn start(app_name: &str) -> Result<Option<Self>> {
        // Load the OpenXR loader at runtime (dlopen) rather than link-time, so a
        // build/run without a runtime installed degrades gracefully.
        let entry = match unsafe { openxr::Entry::load() } {
            Ok(e) => e,
            Err(e) => {
                tracing::info!("OpenXR loader not found, VR disabled: {e}");
                return Ok(None);
            }
        };

        let available = entry
            .enumerate_extensions()
            .context("enumerating OpenXR extensions")?;
        if !available.khr_vulkan_enable2 {
            tracing::warn!("OpenXR runtime lacks XR_KHR_vulkan_enable2; VR disabled");
            return Ok(None);
        }

        let mut extensions = openxr::ExtensionSet::default();
        extensions.khr_vulkan_enable2 = true;
        // Instance::now() (used to timestamp head/tracker pose lookups) needs the
        // timespec-conversion extension on Linux runtimes; enabling it avoids the
        // `KHR_convert_timespec_time not loaded` panic. Tracked so the pose readers
        // can fall back gracefully if a runtime lacks it.
        let time_supported = available.khr_convert_timespec_time;
        if time_supported {
            extensions.khr_convert_timespec_time = true;
        }
        // SteamVR/Vive full-body trackers (waist/feet) come through this
        // extension; enable it when the runtime offers it.
        let tracker_supported = available.htcx_vive_tracker_interaction;
        if tracker_supported {
            extensions.htcx_vive_tracker_interaction = true;
        }

        let app_info = openxr::ApplicationInfo {
            application_name: app_name,
            application_version: 1,
            engine_name: "citrus",
            engine_version: 1,
            api_version: openxr::Version::new(1, 0, 0),
        };
        let instance = entry
            .create_instance(&app_info, &extensions, &[])
            .context("creating OpenXR instance")?;

        // Select the head-mounted display. FORM_FACTOR_UNAVAILABLE means no HMD
        // is connected/active — not an error, just "no VR right now".
        let system = match instance.system(openxr::FormFactor::HEAD_MOUNTED_DISPLAY) {
            Ok(s) => s,
            Err(openxr::sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE) => {
                tracing::info!("No OpenXR HMD connected; VR disabled");
                return Ok(None);
            }
            Err(e) => return Err(anyhow::anyhow!("selecting OpenXR system: {e}")),
        };

        let props = instance
            .system_properties(system)
            .context("querying OpenXR system properties")?;

        tracing::info!(
            system = %props.system_name,
            orientation = props.tracking_properties.orientation_tracking,
            position = props.tracking_properties.position_tracking,
            "OpenXR started"
        );

        Ok(Some(Self {
            entry,
            instance,
            system,
            system_name: props.system_name.clone(),
            orientation_tracking: props.tracking_properties.orientation_tracking,
            position_tracking: props.tracking_properties.position_tracking,
            time_supported,
            tracker_supported,
        }))
    }

    /// Human-readable HMD/runtime name (for the editor status line / logs).
    pub fn system_name(&self) -> &str {
        &self.system_name
    }

    /// Create the VR session, sharing citrus-render's Vulkan device
    /// (`XR_KHR_vulkan_enable2`). Pass the renderer's raw handles
    /// (`Renderer::vulkan_raw_handles`). The session must be polled each frame
    /// ([`XrSession::poll`]) to drive its lifecycle, after which head/controller
    /// poses can be read ([`XrSession::head_pose`]).
    pub fn create_session(
        &self,
        vk_instance: u64,
        vk_physical_device: u64,
        vk_device: u64,
        queue_family_index: u32,
    ) -> Result<XrSession> {
        // Spec requires querying graphics requirements before session creation.
        let _ = self
            .instance
            .graphics_requirements::<openxr::Vulkan>(self.system)
            .context("OpenXR Vulkan graphics requirements")?;
        // With XR_KHR_vulkan_enable2 the runtime MANDATES a call to
        // xrGetVulkanGraphicsDevice2KHR before xrCreateSession (it validates both
        // that the call happened AND that the session uses the device it returns —
        // the HMD's GPU). Skipping it is the `XR_ERROR_VALIDATION_FAILURE: Has not
        // called xrGetVulkanGraphicsDevice2KHR` seen on WiVRn/Monado/SteamVR.
        let vk_instance_ptr = vk_instance as usize as *const std::ffi::c_void;
        let xr_physical_device = unsafe {
            self.instance
                .vulkan_graphics_device(self.system, vk_instance_ptr)
        }
        .context("xrGetVulkanGraphicsDevice2KHR")?;
        // On a single-GPU machine this equals the renderer's pick. If it differs
        // (multi-GPU laptop, HMD on the dGPU), the renderer's VkDevice was built on
        // the wrong GPU and the session will still fail — the real fix is to create
        // the renderer on this device. Warn so that's diagnosable.
        let our_physical_device = vk_physical_device as usize as *const std::ffi::c_void;
        if xr_physical_device != our_physical_device {
            tracing::warn!(
                "OpenXR requires a different Vulkan physical device than the renderer \
                 selected (multi-GPU?). The renderer must be created on the HMD's GPU; \
                 the session create below will likely fail until then."
            );
        }
        let info = openxr::vulkan::SessionCreateInfo {
            instance: vk_instance_ptr,
            physical_device: xr_physical_device,
            device: vk_device as usize as *const std::ffi::c_void,
            queue_family_index,
            queue_index: 0,
        };
        let (session, frame_waiter, frame_stream) = unsafe {
            self.instance
                .create_session::<openxr::Vulkan>(self.system, &info)
                .context("creating OpenXR session")?
        };
        let identity = openxr::Posef {
            orientation: openxr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            position: openxr::Vector3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        };
        let stage = session.create_reference_space(openxr::ReferenceSpaceType::STAGE, identity)?;
        let view = session.create_reference_space(openxr::ReferenceSpaceType::VIEW, identity)?;

        // Full-body tracker action set: pose actions for hands (controllers) +
        // waist/feet (SteamVR/Vive trackers), each with an action space located
        // against the stage every frame.
        let action_set = self
            .instance
            .create_action_set("fullbody", "Full Body Tracking", 0)
            .context("creating OpenXR action set")?;
        let mk = |name: &str, label: &str| -> Result<openxr::Action<openxr::Posef>> {
            action_set
                .create_action::<openxr::Posef>(name, label, &[])
                .map_err(|e| anyhow::anyhow!("creating action {name}: {e}"))
        };
        let a_left_hand = mk("left_hand", "Left Hand")?;
        let a_right_hand = mk("right_hand", "Right Hand")?;
        let a_hips = mk("hips", "Hips")?;
        let a_left_foot = mk("left_foot", "Left Foot")?;
        let a_right_foot = mk("right_foot", "Right Foot")?;

        // Locomotion / interaction inputs (analog sticks + grips, digital trigger +
        // menu button). Bound to the Oculus Touch profile below (the common case);
        // a runtime with a different controller just ignores the suggestions.
        let mkf = |name: &str, label: &str| -> Result<openxr::Action<f32>> {
            action_set
                .create_action::<f32>(name, label, &[])
                .map_err(|e| anyhow::anyhow!("creating action {name}: {e}"))
        };
        let mkb = |name: &str, label: &str| -> Result<openxr::Action<bool>> {
            action_set
                .create_action::<bool>(name, label, &[])
                .map_err(|e| anyhow::anyhow!("creating action {name}: {e}"))
        };
        let a_lstick_x = mkf("left_stick_x", "Left Stick X")?;
        let a_lstick_y = mkf("left_stick_y", "Left Stick Y")?;
        let a_rstick_x = mkf("right_stick_x", "Right Stick X")?;
        let a_rstick_y = mkf("right_stick_y", "Right Stick Y")?;
        let a_lgrip = mkf("left_grip", "Left Grip")?;
        let a_rgrip = mkf("right_grip", "Right Grip")?;
        let a_rtrigger = mkb("right_trigger", "Right Trigger")?;
        let a_menu = mkb("menu", "Menu")?;

        // Suggested bindings. Hands map to the cross-vendor simple controller;
        // waist/feet to Vive tracker role paths (best-effort — ignored if the
        // runtime lacks the tracker extension). Failures here are non-fatal.
        let path = |s: &str| self.instance.string_to_path(s);
        if let (Ok(lh), Ok(rh)) = (
            path("/user/hand/left/input/aim/pose"),
            path("/user/hand/right/input/aim/pose"),
        ) {
            let _ = self.instance.suggest_interaction_profile_bindings(
                path("/interaction_profiles/khr/simple_controller").unwrap_or(openxr::Path::NULL),
                &[
                    openxr::Binding::new(&a_left_hand, lh),
                    openxr::Binding::new(&a_right_hand, rh),
                ],
            );
        }
        if let (true, Ok(w), Ok(lf), Ok(rf)) = (
            self.tracker_supported,
            path("/user/vive_tracker_htcx/role/waist/input/grip/pose"),
            path("/user/vive_tracker_htcx/role/left_foot/input/grip/pose"),
            path("/user/vive_tracker_htcx/role/right_foot/input/grip/pose"),
        ) {
            let _ = self.instance.suggest_interaction_profile_bindings(
                path("/interaction_profiles/htc/vive_tracker_htcx").unwrap_or(openxr::Path::NULL),
                &[
                    openxr::Binding::new(&a_hips, w),
                    openxr::Binding::new(&a_left_foot, lf),
                    openxr::Binding::new(&a_right_foot, rf),
                ],
            );
        }
        // Locomotion/interaction bindings on the Oculus Touch profile (Quest, Rift;
        // the most common runtime). Pose hands also bind here so controllers track.
        if let (Ok(lsx), Ok(lsy), Ok(rsx), Ok(rsy), Ok(lg), Ok(rg), Ok(rt), Ok(mn), Ok(lhp), Ok(rhp)) = (
            path("/user/hand/left/input/thumbstick/x"),
            path("/user/hand/left/input/thumbstick/y"),
            path("/user/hand/right/input/thumbstick/x"),
            path("/user/hand/right/input/thumbstick/y"),
            path("/user/hand/left/input/squeeze/value"),
            path("/user/hand/right/input/squeeze/value"),
            path("/user/hand/right/input/trigger/value"),
            path("/user/hand/left/input/y/click"),
            path("/user/hand/left/input/aim/pose"),
            path("/user/hand/right/input/aim/pose"),
        ) {
            let _ = self.instance.suggest_interaction_profile_bindings(
                path("/interaction_profiles/oculus/touch_controller").unwrap_or(openxr::Path::NULL),
                &[
                    openxr::Binding::new(&a_left_hand, lhp),
                    openxr::Binding::new(&a_right_hand, rhp),
                    openxr::Binding::new(&a_lstick_x, lsx),
                    openxr::Binding::new(&a_lstick_y, lsy),
                    openxr::Binding::new(&a_rstick_x, rsx),
                    openxr::Binding::new(&a_rstick_y, rsy),
                    openxr::Binding::new(&a_lgrip, lg),
                    openxr::Binding::new(&a_rgrip, rg),
                    openxr::Binding::new(&a_rtrigger, rt),
                    openxr::Binding::new(&a_menu, mn),
                ],
            );
        }
        session
            .attach_action_sets(&[&action_set])
            .context("attaching OpenXR action set")?;

        let space = |a: &openxr::Action<openxr::Posef>| a.create_space(&session, openxr::Path::NULL, identity);
        let left_hand = space(&a_left_hand)?;
        let right_hand = space(&a_right_hand)?;
        let hips = space(&a_hips)?;
        let left_foot = space(&a_left_foot)?;
        let right_foot = space(&a_right_foot)?;

        Ok(XrSession {
            instance: self.instance.clone(),
            system: self.system,
            session,
            frame_waiter,
            frame_stream,
            stage,
            view,
            action_set,
            left_hand,
            right_hand,
            hips,
            left_foot,
            right_foot,
            a_lstick_x,
            a_lstick_y,
            a_rstick_x,
            a_rstick_y,
            a_lgrip,
            a_rgrip,
            a_rtrigger,
            a_menu,
            running: false,
            swapchains: Vec::new(),
            swap_images: Vec::new(),
            view_extent: (0, 0),
            blend_mode: openxr::EnvironmentBlendMode::OPAQUE,
            time_supported: self.time_supported,
        })
    }
}

/// Controller inputs read this frame (analog sticks/grips + digital trigger/menu).
/// Bound to the Oculus Touch profile; zero/false when unbound or not tracking.
#[derive(Clone, Copy, Debug, Default)]
pub struct VrInput {
    pub left_stick: (f32, f32),
    pub right_stick: (f32, f32),
    pub left_grip: f32,
    pub right_grip: f32,
    pub right_trigger: bool,
    pub menu: bool,
}

/// Tracked full-body poses (stage space) read from the VR session this frame.
/// Each is `None` until its device is present + tracking. Maps directly to
/// `citrus_core::TrackerTargets`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BodyPoses {
    pub head: Option<(glam::Vec3, glam::Quat)>,
    pub left_hand: Option<(glam::Vec3, glam::Quat)>,
    pub right_hand: Option<(glam::Vec3, glam::Quat)>,
    pub hips: Option<(glam::Vec3, glam::Quat)>,
    pub left_foot: Option<(glam::Vec3, glam::Quat)>,
    pub right_foot: Option<(glam::Vec3, glam::Quat)>,
}

/// A live OpenXR session sharing the renderer's Vulkan device. Drives its own
/// lifecycle via [`poll`] and exposes tracked poses (head now; controllers +
/// SteamVR/Vive trackers via action sets are the next step). Per-eye swapchains
/// + stereo rendering are wired separately once the renderer exposes them.
pub struct XrSession {
    instance: openxr::Instance,
    system: openxr::SystemId,
    pub session: openxr::Session<openxr::Vulkan>,
    frame_waiter: openxr::FrameWaiter,
    frame_stream: openxr::FrameStream<openxr::Vulkan>,
    stage: openxr::Space,
    view: openxr::Space,
    action_set: openxr::ActionSet,
    left_hand: openxr::Space,
    right_hand: openxr::Space,
    hips: openxr::Space,
    left_foot: openxr::Space,
    right_foot: openxr::Space,
    a_lstick_x: openxr::Action<f32>,
    a_lstick_y: openxr::Action<f32>,
    a_rstick_x: openxr::Action<f32>,
    a_rstick_y: openxr::Action<f32>,
    a_lgrip: openxr::Action<f32>,
    a_rgrip: openxr::Action<f32>,
    a_rtrigger: openxr::Action<bool>,
    a_menu: openxr::Action<bool>,
    pub running: bool,
    // Stereo render: per-eye swapchains + their VkImage handles, render extent.
    swapchains: Vec<openxr::Swapchain<openxr::Vulkan>>,
    swap_images: Vec<Vec<u64>>,
    view_extent: (u32, u32),
    blend_mode: openxr::EnvironmentBlendMode,
    /// `Instance::now()` is only safe when `XR_KHR_convert_timespec_time` loaded.
    time_supported: bool,
}

/// One XR frame's per-eye render targets + view/projection (from [`begin_frame`]).
pub struct XrFrame {
    pub display_time: openxr::Time,
    pub eyes: Vec<XrEye>,
}

/// A single eye to render this frame.
pub struct XrEye {
    /// Raw `VkImage` (u64) of the acquired swapchain image to render into.
    pub image: u64,
    pub image_index: u32,
    /// View matrix (world → eye) and projection (asymmetric, from the eye FOV).
    pub view: glam::Mat4,
    pub proj: glam::Mat4,
    pub pose: openxr::Posef,
    pub fov: openxr::Fovf,
    pub width: u32,
    pub height: u32,
}

/// Asymmetric perspective projection from an OpenXR field of view (Vulkan depth
/// 0..1). Matches the OpenXR projection convention.
fn fov_to_proj(fov: openxr::Fovf, near: f32, far: f32) -> glam::Mat4 {
    let l = fov.angle_left.tan();
    let r = fov.angle_right.tan();
    let u = fov.angle_up.tan();
    let d = fov.angle_down.tan();
    let w = r - l;
    let h = u - d;
    glam::Mat4::from_cols_array(&[
        2.0 / w, 0.0, 0.0, 0.0,
        0.0, 2.0 / h, 0.0, 0.0,
        (r + l) / w, (u + d) / h, -far / (far - near), -1.0,
        0.0, 0.0, -(far * near) / (far - near), 0.0,
    ])
}

/// View (world → eye) matrix = inverse of the eye's world pose.
fn view_to_view_matrix(pose: openxr::Posef) -> glam::Mat4 {
    let q = glam::Quat::from_xyzw(
        pose.orientation.x,
        pose.orientation.y,
        pose.orientation.z,
        pose.orientation.w,
    );
    let p = glam::Vec3::new(pose.position.x, pose.position.y, pose.position.z);
    glam::Mat4::from_rotation_translation(q, p).inverse()
}

impl XrSession {
    /// Pump the OpenXR event queue and drive the session lifecycle (begin on
    /// READY, end on STOPPING). Call once per frame.
    pub fn poll(&mut self) {
        let mut buf = openxr::EventDataBuffer::new();
        while let Ok(Some(event)) = self.instance.poll_event(&mut buf) {
            if let openxr::Event::SessionStateChanged(e) = event {
                match e.state() {
                    openxr::SessionState::READY => {
                        if self
                            .session
                            .begin(openxr::ViewConfigurationType::PRIMARY_STEREO)
                            .is_ok()
                        {
                            self.running = true;
                            tracing::info!("OpenXR session running");
                        }
                    }
                    openxr::SessionState::STOPPING => {
                        let _ = self.session.end();
                        self.running = false;
                    }
                    _ => {}
                }
            }
        }
        // Sync the tracker action set so the action spaces have fresh poses.
        if self.running {
            let _ = self
                .session
                .sync_actions(&[openxr::ActiveActionSet::new(&self.action_set)]);
        }
    }

    /// Locate one action/reference space in the stage space at `time`.
    fn locate(&self, space: &openxr::Space, time: openxr::Time) -> Option<(glam::Vec3, glam::Quat)> {
        let loc = space.locate(&self.stage, time).ok()?;
        if !loc
            .location_flags
            .contains(openxr::SpaceLocationFlags::POSITION_VALID)
        {
            return None;
        }
        let p = loc.pose.position;
        let o = loc.pose.orientation;
        Some((
            glam::Vec3::new(p.x, p.y, p.z),
            glam::Quat::from_xyzw(o.x, o.y, o.z, o.w),
        ))
    }

    /// All tracked full-body poses this frame (head + hands + waist + feet) in
    /// the stage space. Feed straight into `citrus_core::TrackerTargets`.
    pub fn body_poses(&self) -> BodyPoses {
        if !self.running || !self.time_supported {
            return BodyPoses::default();
        }
        let Ok(time) = self.instance.now() else {
            return BodyPoses::default();
        };
        BodyPoses {
            head: self.locate(&self.view, time),
            left_hand: self.locate(&self.left_hand, time),
            right_hand: self.locate(&self.right_hand, time),
            hips: self.locate(&self.hips, time),
            left_foot: self.locate(&self.left_foot, time),
            right_foot: self.locate(&self.right_foot, time),
        }
    }

    /// Read this frame's controller inputs (call after [`poll`], which syncs the
    /// action set). All zero/false when not running.
    pub fn input(&self) -> VrInput {
        if !self.running {
            return VrInput::default();
        }
        let f = |a: &openxr::Action<f32>| {
            a.state(&self.session, openxr::Path::NULL)
                .map(|s| s.current_state)
                .unwrap_or(0.0)
        };
        let b = |a: &openxr::Action<bool>| {
            a.state(&self.session, openxr::Path::NULL)
                .map(|s| s.current_state)
                .unwrap_or(false)
        };
        VrInput {
            left_stick: (f(&self.a_lstick_x), f(&self.a_lstick_y)),
            right_stick: (f(&self.a_rstick_x), f(&self.a_rstick_y)),
            left_grip: f(&self.a_lgrip),
            right_grip: f(&self.a_rgrip),
            right_trigger: b(&self.a_rtrigger),
            menu: b(&self.a_menu),
        }
    }

    /// Create the per-eye stereo swapchains (color) for `color_format` (a raw
    /// `VkFormat` as u64 the renderer will render into). Call once after the
    /// session is created, before [`begin_frame`]. Returns the per-eye render
    /// extent (both eyes share it).
    pub fn setup_stereo(&mut self, color_format: u32) -> Result<(u32, u32)> {
        let views = self
            .instance
            .enumerate_view_configuration_views(
                self.system,
                openxr::ViewConfigurationType::PRIMARY_STEREO,
            )
            .context("enumerating view configuration views")?;
        // Stereo = 2 views; both eyes use the recommended size.
        let v0 = views.first().context("no stereo views")?;
        let (w, h) = (
            v0.recommended_image_rect_width,
            v0.recommended_image_rect_height,
        );
        let mut swapchains = Vec::new();
        let mut images = Vec::new();
        for _ in 0..views.len().max(2) {
            let sc = self
                .session
                .create_swapchain(&openxr::SwapchainCreateInfo {
                    create_flags: openxr::SwapchainCreateFlags::EMPTY,
                    usage_flags: openxr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | openxr::SwapchainUsageFlags::SAMPLED,
                    format: color_format,
                    sample_count: 1,
                    width: w,
                    height: h,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })
                .context("creating XR swapchain")?;
            let imgs = sc.enumerate_images().context("enumerating XR swapchain images")?;
            swapchains.push(sc);
            images.push(imgs);
        }
        self.swapchains = swapchains;
        self.swap_images = images;
        self.view_extent = (w, h);
        Ok((w, h))
    }

    /// Begin an XR frame: wait for the predicted display time, start the frame,
    /// and locate the per-eye views. Returns `None` when not running / should
    /// skip rendering this frame. The caller renders the scene into each eye's
    /// image (`XrEye::image`), then calls [`end_frame`].
    pub fn begin_frame(&mut self) -> Result<Option<XrFrame>> {
        if !self.running || self.swapchains.is_empty() {
            return Ok(None);
        }
        let state = self.frame_waiter.wait().context("xrWaitFrame")?;
        self.frame_stream.begin().context("xrBeginFrame")?;
        if !state.should_render {
            // Still must end the frame with no layers.
            self.frame_stream
                .end(state.predicted_display_time, self.blend_mode, &[])
                .ok();
            return Ok(None);
        }
        let (_flags, views) = self
            .session
            .locate_views(
                openxr::ViewConfigurationType::PRIMARY_STEREO,
                state.predicted_display_time,
                &self.stage,
            )
            .context("xrLocateViews")?;
        let (w, h) = self.view_extent;
        let mut eyes = Vec::with_capacity(self.swapchains.len());
        for (i, view) in views.iter().enumerate().take(self.swapchains.len()) {
            let idx = self.swapchains[i].acquire_image().context("acquire XR image")?;
            self.swapchains[i]
                .wait_image(openxr::Duration::INFINITE)
                .context("wait XR image")?;
            eyes.push(XrEye {
                image: self.swap_images[i][idx as usize],
                image_index: idx,
                view: view_to_view_matrix(view.pose),
                proj: fov_to_proj(view.fov, 0.05, 2000.0),
                pose: view.pose,
                fov: view.fov,
                width: w,
                height: h,
            });
        }
        Ok(Some(XrFrame {
            display_time: state.predicted_display_time,
            eyes,
        }))
    }

    /// Finish an XR frame: release each eye image and submit the projection
    /// composition layer to the compositor. Call after rendering all eyes.
    pub fn end_frame(&mut self, frame: XrFrame) -> Result<()> {
        for sc in &mut self.swapchains {
            sc.release_image().ok();
        }
        let (w, h) = self.view_extent;
        let rect = openxr::Rect2Di {
            offset: openxr::Offset2Di { x: 0, y: 0 },
            extent: openxr::Extent2Di {
                width: w as i32,
                height: h as i32,
            },
        };
        let proj_views: Vec<openxr::CompositionLayerProjectionView<openxr::Vulkan>> = frame
            .eyes
            .iter()
            .enumerate()
            .map(|(i, eye)| {
                openxr::CompositionLayerProjectionView::new()
                    .pose(eye.pose)
                    .fov(eye.fov)
                    .sub_image(
                        openxr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[i])
                            .image_array_index(0)
                            .image_rect(rect),
                    )
            })
            .collect();
        let layer = openxr::CompositionLayerProjection::new()
            .space(&self.stage)
            .views(&proj_views);
        self.frame_stream
            .end(frame.display_time, self.blend_mode, &[&layer])
            .context("xrEndFrame")?;
        Ok(())
    }

    /// The headset pose (position + orientation) in the stage reference space, or
    /// `None` if not yet tracking. Feed into `citrus_core::TrackerTargets::head`.
    pub fn head_pose(&self) -> Option<(glam::Vec3, glam::Quat)> {
        if !self.running || !self.time_supported {
            return None;
        }
        let time = self.instance.now().ok()?;
        let loc = self.view.locate(&self.stage, time).ok()?;
        if !loc
            .location_flags
            .contains(openxr::SpaceLocationFlags::POSITION_VALID)
        {
            return None;
        }
        let p = loc.pose.position;
        let o = loc.pose.orientation;
        Some((
            glam::Vec3::new(p.x, p.y, p.z),
            glam::Quat::from_xyzw(o.x, o.y, o.z, o.w),
        ))
    }
}
