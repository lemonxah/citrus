//! The hub's window: winit events, egui input and the egui-only Vulkan
//! window. Redraws only on input, egui's repaint requests and background
//! results, so an idle hub costs nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::hub::{Hub, HubRequests};
use crate::settings::MIN_SIZE;

pub fn run() -> Result<()> {
    let event_loop = EventLoop::<HubEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        proxy: event_loop.create_proxy(),
        #[cfg(target_os = "linux")]
        tray: None,
        gpu: None,
        egui_state: None,
        window: None,
        egui_ctx: egui::Context::default(),
        hub: None,
        repaint_at: None,
        minimized: false,
        error: None,
    };
    event_loop.run_app(&mut app)?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

struct App {
    proxy: winit::event_loop::EventLoopProxy<HubEvent>,
    #[cfg(target_os = "linux")]
    tray: Option<ksni::blocking::Handle<crate::tray::HubTray>>,
    // Before `window`: the surface must go before the window it was made for.
    gpu: Option<citrus_render::EguiWindow>,
    egui_state: Option<egui_winit::State>,
    window: Option<Arc<Window>>,
    egui_ctx: egui::Context,
    hub: Option<Hub>,
    /// When egui asked for its next repaint (animations, timed messages).
    repaint_at: Option<Instant>,
    minimized: bool,
    error: Option<anyhow::Error>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum HubEvent { Show, Quit }

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        citrus_editor::install_icon_font(&self.egui_ctx);
        citrus_editor::theme::apply(&self.egui_ctx);
        if let Err(e) = citrus_editor::icons::install_app_icon() {
            tracing::debug!("installing app icon: {e}");
        }
        let hub = Hub::new(&self.egui_ctx);
        let [w, h] = hub.settings.window_size_clamped();
        let (icon_w, icon_h, icon_rgba) = citrus_editor::icons::app_icon_rgba();
        let attrs = Window::default_attributes()
            .with_title("Citrus Project Hub")
            .with_inner_size(LogicalSize::new(w, h))
            .with_min_inner_size(LogicalSize::new(MIN_SIZE[0], MIN_SIZE[1]))
            .with_window_icon(winit::window::Icon::from_rgba(icon_rgba, icon_w, icon_h).ok());
        // Same app id as the editor so Wayland finds the installed citrus icon.
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::{wayland, x11};
            let attrs = wayland::WindowAttributesExtWayland::with_name(attrs, "citrus", "citrus");
            x11::WindowAttributesExtX11::with_name(attrs, "citrus", "citrus")
        };
        let window = Arc::new(event_loop.create_window(attrs).context("creating the hub window")?);
        #[cfg(target_os = "linux")]
        { self.tray = crate::tray::start(self.proxy.clone()); }
        let gpu = citrus_render::EguiWindow::new(&window, c"citrus hub", true).context("starting Vulkan for the hub window")?;
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        ));
        window.request_redraw();
        self.gpu = Some(gpu);
        self.window = Some(window);
        self.hub = Some(hub);
        Ok(())
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(state), Some(hub), Some(gpu)) =
            (self.window.as_ref(), self.egui_state.as_mut(), self.hub.as_mut(), self.gpu.as_mut())
        else {
            return;
        };
        if self.minimized {
            return;
        }
        let raw = state.take_egui_input(window);
        let mut requests = HubRequests::default();
        let output = self.egui_ctx.run(raw, |ctx| requests = hub.ui(ctx));
        state.handle_platform_output(window, output.platform_output);
        let primitives = self.egui_ctx.tessellate(output.shapes, output.pixels_per_point);
        let draw = citrus_render::EguiDraw {
            pixels_per_point: output.pixels_per_point,
            primitives,
            textures_delta: output.textures_delta,
        };
        if let Err(e) = gpu.render(&draw) {
            tracing::error!("hub frame: {e:#}");
        }
        let delay = output.viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay);
        if delay.is_zero() {
            window.request_redraw();
        } else if delay < Duration::from_secs(3600) {
            self.repaint_at = Some(Instant::now() + delay);
        }
        if requests.close {
            hub.save_settings();
            event_loop.exit();
        } else if requests.minimize {
            self.minimize_to_tray();
        }
    }
}

impl App {
    fn minimize_to_tray(&mut self) {
        let Some(window) = &self.window else { return };
        #[cfg(target_os = "linux")]
        if self.tray.as_ref().is_some_and(|tray| !tray.is_closed()) {
            window.set_visible(false);
            self.minimized = true;
            return;
        }
        window.set_minimized(true);
    }
}

impl ApplicationHandler<HubEvent> for App {
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: HubEvent) {
        match event {
            HubEvent::Show => {
                if let Some(window) = &self.window {
                    window.set_visible(true);
                    window.set_minimized(false);
                    window.focus_window();
                    self.minimized = false;
                    window.request_redraw();
                }
            }
            HubEvent::Quit => event_loop.exit(),
        }
    }
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.init(event_loop) {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(window), Some(state)) = (self.window.clone(), self.egui_state.as_mut()) else {
            return;
        };
        if state.on_window_event(&window, &event).repaint {
            window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                if let Some(hub) = &self.hub {
                    hub.save_settings();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.minimized = size.width == 0 || size.height == 0;
                if !self.minimized {
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.resize(size.width, size.height);
                    }
                    if let Some(hub) = self.hub.as_mut() {
                        let logical = size.to_logical::<f32>(window.scale_factor());
                        hub.settings.window_size = [logical.width, logical.height];
                    }
                }
                window.request_redraw();
            }
            WindowEvent::Focused(true) => {
                // The editor may have recorded a project, or been built, meanwhile.
                if let Some(hub) = self.hub.as_mut() {
                    hub.refresh();
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.frame(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.as_ref().is_some_and(|w| w.is_visible() == Some(true) && w.is_minimized() == Some(true)) {
            self.minimize_to_tray();
        }
        match self.repaint_at {
            Some(at) if Instant::now() >= at => {
                self.repaint_at = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(hub) = &self.hub {
            hub.save_settings();
        }
        // GPU objects before the window they render to.
        self.gpu = None;
    }
}
