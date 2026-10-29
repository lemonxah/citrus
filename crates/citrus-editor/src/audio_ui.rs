//! Project Settings > Spatial Audio: Steam Audio quality, reflections, HRTF,
//! output layout, ray tracer and baking, on the editor widget kit.

use citrus_core::{OutputLayout, RayTracerKind, ReflectionMode, SpatialAudioSettings};
use egui::{DragValue, Ui};

use crate::icons::Icon;
use crate::widgets;

/// What the panel asked for this frame.
#[derive(Default)]
pub struct AudioSettingsResponse {
    pub changed: bool,
    /// "Bake Audio" clicked.
    pub bake: bool,
}

fn combo<T: Copy + PartialEq>(ui: &mut Ui, salt: &str, value: &mut T, all: &[T], label: fn(T) -> &'static str) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(salt)
        .width(ui.available_width())
        .selected_text(label(*value))
        .show_ui(ui, |ui| {
            for &v in all {
                changed |= ui.selectable_value(value, v, label(v)).changed();
            }
        });
    changed
}

pub fn audio_settings_ui(ui: &mut Ui, s: &mut SpatialAudioSettings) -> AudioSettingsResponse {
    let mut r = AudioSettingsResponse::default();
    let mut c = false;

    widgets::eyebrow(ui, "Output");
    widgets::property_row(ui, "Layout", |ui| c |= combo(ui, "citrus-sa-output", &mut s.output, &OutputLayout::ALL, OutputLayout::label));
    widgets::property_row(ui, "HRTF (.sofa)", |ui| {
        c |= ui
            .add(egui::TextEdit::singleline(&mut s.hrtf_sofa).hint_text("Steam Audio default").desired_width(ui.available_width()))
            .changed();
    });

    widgets::eyebrow(ui, "Reflections");
    widgets::property_row(ui, "Type", |ui| {
        c |= combo(ui, "citrus-sa-reflections", &mut s.reflections, &ReflectionMode::ALL, ReflectionMode::label);
    });
    ui.add_enabled_ui(s.reflections != ReflectionMode::Off, |ui| {
        widgets::property_row(ui, "Listener reverb", |ui| {
            c |= ui
                .checkbox(&mut s.listener_reverb, "")
                .on_hover_text("Reverb around the listener for sources without their own reflections")
                .changed();
        });
        widgets::property_row(ui, "Rays", |ui| c |= ui.add(DragValue::new(&mut s.rays).speed(16).range(64..=65536)).changed());
        widgets::property_row(ui, "Bounces", |ui| c |= ui.add(DragValue::new(&mut s.bounces).range(1..=64)).changed());
        widgets::property_row(ui, "Duration", |ui| {
            c |= ui.add(DragValue::new(&mut s.duration).speed(0.05).range(0.1..=10.0).suffix(" s")).changed();
        });
        widgets::property_row(ui, "Ambisonic order", |ui| c |= ui.add(egui::Slider::new(&mut s.order, 1..=3)).changed());
        widgets::property_row(ui, "Diffuse samples", |ui| {
            c |= ui.add(DragValue::new(&mut s.diffuse_samples).speed(4).range(1..=1024)).changed();
        });
        widgets::property_row(ui, "Update rate", |ui| {
            c |= ui.add(DragValue::new(&mut s.update_rate).speed(0.5).range(1.0..=60.0).suffix(" Hz")).changed();
        });
        widgets::property_row(ui, "Max sources", |ui| c |= ui.add(DragValue::new(&mut s.max_sources).range(1..=256)).changed());
    });

    widgets::eyebrow(ui, "Simulation");
    widgets::property_row(ui, "Ray tracer", |ui| {
        c |= combo(ui, "citrus-sa-tracer", &mut s.ray_tracer, &RayTracerKind::ALL, RayTracerKind::label);
    });

    widgets::eyebrow(ui, "Baking");
    widgets::property_row(ui, "Rays", |ui| c |= ui.add(DragValue::new(&mut s.bake_rays).speed(64).range(256..=262144)).changed());
    widgets::property_row(ui, "Bounces", |ui| c |= ui.add(DragValue::new(&mut s.bake_bounces).range(1..=256)).changed());
    widgets::property_row(ui, "Duration", |ui| {
        c |= ui.add(DragValue::new(&mut s.bake_duration).speed(0.05).range(0.1..=10.0).suffix(" s")).changed();
    });
    ui.add_space(4.0);
    if widgets::primary_button(ui, Some(Icon::Audio), "Bake Audio").on_hover_text("Bake reverb and pathing for the scene's Audio Probe Volumes").clicked() {
        r.bake = true;
    }
    ui.add_space(4.0);
    widgets::info_note(
        ui,
        "Steam Audio settings apply the next time Play starts. Embree and Radeon Rays fall back to the default tracer when unavailable.",
    );
    r.changed = c;
    r
}
