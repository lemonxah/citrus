//! Linux StatusNotifier tray, with the exact same orange icon as the window.
use ksni::blocking::TrayMethods;
use winit::event_loop::EventLoopProxy;

use crate::app::HubEvent;

pub struct HubTray {
    proxy: EventLoopProxy<HubEvent>,
    icon: Vec<u8>,
}

pub fn start(proxy: EventLoopProxy<HubEvent>) -> Option<ksni::blocking::Handle<HubTray>> {
    let (_, _, rgba) = citrus_editor::icons::app_icon_rgba();
    // StatusNotifier pixmaps are network-order ARGB, rather than winit RGBA.
    let icon = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    match (HubTray { proxy, icon }).spawn() {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::info!("Tray unavailable; keeping normal window minimization: {e}");
            None
        }
    }
}

impl ksni::Tray for HubTray {
    fn id(&self) -> String { "citrus-hub".into() }
    fn title(&self) -> String { "Citrus Project Hub".into() }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon { width: 256, height: 256, data: self.icon.clone() }]
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = self.proxy.send_event(HubEvent::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem { label: "Open Citrus Hub".into(), activate: Box::new(|tray: &mut Self| { let _ = tray.proxy.send_event(HubEvent::Show); }), ..Default::default() }.into(),
            StandardItem { label: "Quit Citrus Hub".into(), activate: Box::new(|tray: &mut Self| { let _ = tray.proxy.send_event(HubEvent::Quit); }), ..Default::default() }.into(),
        ]
    }
}
