//! The editor's Network window: host / join (direct or from the LAN list),
//! then the session status, player list, traffic and owned objects.

use citrus_editor::{icons::Icon, theme, widgets};
use egui::RichText;

use crate::net::{ConnState, LanBrowser, NetSession};
use crate::scene::LoadedScene;

/// Window state kept between frames.
pub struct NetWindowState {
    pub player_name: String,
    browser: Option<LanBrowser>,
    last_refresh: f64,
}

impl Default for NetWindowState {
    fn default() -> Self {
        Self {
            player_name: std::env::var("CITRUS_NAME").unwrap_or_else(|_| "Editor".to_string()),
            browser: None,
            last_refresh: -10.0,
        }
    }
}

enum Act {
    Host(u16),
    Join(String),
    Disconnect,
}

/// Draw the window. Returns an error to show in the status bar, if any.
pub fn network_window(
    ctx: &egui::Context,
    open: &mut bool,
    net: &mut Option<NetSession>,
    addr: &mut String,
    st: &mut NetWindowState,
    scene: &LoadedScene,
) -> Option<String> {
    let mut act = None;
    let now = ctx.input(|i| i.time);
    egui::Window::new("Network")
        .open(open)
        .resizable(false)
        .default_width(320.0)
        .show(ctx, |ui| match net.as_ref() {
            Some(s) => connected_ui(ui, s, scene, &mut act),
            None => {
                let (cfg, port) = crate::net_sync::config_for(scene);
                widgets::eyebrow(ui, "Session");
                widgets::property_row(ui, "Name", |ui| ui.text_edit_singleline(&mut st.player_name));
                widgets::property_row(ui, "Address", |ui| ui.text_edit_singleline(addr));
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, Some(Icon::Play), "Host").clicked() {
                        let p = addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(port);
                        act = Some(Act::Host(p));
                    }
                    if widgets::secondary_button(ui, None, "Join").clicked() {
                        act = Some(Act::Join(addr.clone()));
                    }
                });
                widgets::info_note(
                    ui,
                    &format!(
                        "{} · up to {} players. Host binds the port in Address; Join connects to it. Networked objects replicate while playing.",
                        cfg.session_name, cfg.max_players
                    ),
                );
                widgets::section_gap(ui);
                ui.horizontal(|ui| {
                    widgets::eyebrow(ui, "LAN games");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button(ui, Icon::Reset, "Search the local network", false).clicked() {
                            st.last_refresh = -10.0;
                            if st.browser.is_none() {
                                st.browser = LanBrowser::new().ok();
                            }
                        }
                    });
                });
                if let Some(b) = st.browser.as_mut() {
                    if now - st.last_refresh > 2.0 {
                        st.last_refresh = now;
                        b.refresh();
                    }
                    let sessions = b.poll().to_vec();
                    if sessions.is_empty() {
                        ui.label(RichText::new("Searching...").weak());
                    }
                    for s in sessions {
                        ui.horizontal(|ui| {
                            ui.label(&s.name);
                            ui.label(RichText::new(format!("{}/{}", s.players, s.max_players)).font(theme::mono(11.0)));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if widgets::secondary_button(ui, None, "Join").clicked() {
                                    *addr = s.addr.to_string();
                                    act = Some(Act::Join(s.addr.to_string()));
                                }
                                ui.label(RichText::new(s.addr.to_string()).font(theme::mono(11.0)).weak());
                            });
                        });
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(500));
                }
            }
        });
    let (mut cfg, _) = crate::net_sync::config_for(scene);
    cfg.player_name = st.player_name.clone();
    match act? {
        Act::Disconnect => {
            if let Some(mut s) = net.take() {
                s.disconnect();
            }
            None
        }
        Act::Host(port) => match NetSession::host_with(port, cfg) {
            Ok(s) => {
                *net = Some(s);
                None
            }
            Err(e) => Some(format!("Host failed: {e}")),
        },
        Act::Join(a) => match NetSession::join_with(&a, cfg) {
            Ok(s) => {
                *net = Some(s);
                None
            }
            Err(e) => Some(format!("Join failed: {e}")),
        },
    }
}

fn connected_ui(ui: &mut egui::Ui, s: &NetSession, scene: &LoadedScene, act: &mut Option<Act>) {
    let role = if s.is_host() { "Hosting" } else { "Client" };
    ui.horizontal(|ui| {
        match s.state() {
            ConnState::Connected => {
                widgets::chip(ui, &format!("{role} · peer {}", s.local_peer()));
            }
            ConnState::Connecting => {
                widgets::badge(ui, "Connecting...");
            }
            ConnState::Disconnected(_) => {
                widgets::warn_badge(ui, "Disconnected");
            }
        }
        if let Some(a) = s.local_addr() {
            ui.label(RichText::new(a.to_string()).font(theme::mono(11.0)).weak());
        }
    });
    if let ConnState::Disconnected(reason) = s.state() {
        widgets::info_note(ui, reason);
    }
    widgets::section_gap(ui);
    widgets::eyebrow(ui, &format!("Players ({}/{})", s.peer_count(), s.cfg.max_players));
    for p in s.peer_infos() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("#{}", p.id)).font(theme::mono(11.0)).weak());
            ui.label(&p.name);
            if p.is_host {
                widgets::badge(ui, "host");
            }
            if p.id == s.local_peer() {
                widgets::badge(ui, "you");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if p.id != s.local_peer() && p.rtt_ms > 0.0 {
                    ui.label(RichText::new(format!("{:.0} ms", p.rtt_ms)).font(theme::mono(11.0)));
                }
            });
        });
    }
    widgets::section_gap(ui);
    widgets::eyebrow(ui, "Traffic");
    let stats = s.stats;
    let mono = |ui: &mut egui::Ui, label: &str, v: String| {
        widgets::property_row(ui, label, |ui| ui.label(RichText::new(v).font(theme::mono(11.0))));
    };
    mono(ui, "In / Out", format!("{:.1} / {:.1} kB/s", stats.rate_in / 1024.0, stats.rate_out / 1024.0));
    mono(ui, "Packets", format!("{} in · {} out · {} resent", stats.packets_in, stats.packets_out, stats.resends));
    if !s.is_host() {
        mono(ui, "Ping", format!("{:.0} ms", s.rtt_ms()));
    }
    let owned = s.world.owners.values().filter(|(o, _)| *o == s.local_peer() && *o != 0).count();
    mono(ui, "Objects", format!("{} owned · {} spawned", owned, s.world.spawns.len()));
    // Networked objects in this scene and who simulates them.
    let rows: Vec<(String, u64, bool)> = scene
        .objects
        .iter()
        .filter_map(|o| {
            let n = o.components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::NetworkObject>())?;
            n.state.connected.then(|| (o.name.clone(), n.state.owner, n.state.authority))
        })
        .collect();
    if !rows.is_empty() {
        egui::CollapsingHeader::new(format!("Networked objects ({})", rows.len())).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                for (name, owner, authority) in rows {
                    ui.horizontal(|ui| {
                        ui.label(if name.is_empty() { "(unnamed)".to_string() } else { name });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let who = if owner == 0 { "host".to_string() } else { format!("peer {owner}") };
                            let text = RichText::new(who).font(theme::mono(11.0));
                            ui.label(if authority { text.color(theme::accent()) } else { text.weak() });
                        });
                    });
                }
            });
        });
    }
    widgets::section_gap(ui);
    if widgets::secondary_button(ui, Some(Icon::Stop), "Disconnect").clicked() {
        *act = Some(Act::Disconnect);
    }
}
