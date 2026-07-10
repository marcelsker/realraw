use std::path::PathBuf;

use eframe::egui;

use crate::app::App;

/// Develop adjustment settings, matching Lightroom's basic panel.
#[derive(Debug, Clone, PartialEq)]
pub struct DevelopSettings {
    // Light
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    // Presence
    pub clarity: f32,
    pub vibrance: f32,
    pub saturation: f32,
    // Color
    pub temp: f32,
    pub tint: f32,
}

impl Default for DevelopSettings {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            clarity: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            temp: 0.0,
            tint: 0.0,
        }
    }
}

fn slider(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>) {
    ui.horizontal(|ui| {
        ui.add_sized([80.0, 0.0], egui::Label::new(label));
        ui.add(egui::Slider::new(value, range).show_value(true));
    });
}

fn section_header(ui: &mut egui::Ui, label: &str) {
    ui.label(egui::RichText::new(label).strong().size(13.0));
    ui.separator();
}

/// Ensure the develop preview is loading the currently selected photo.
fn ensure_preview(app: &mut App) {
    let Some(id) = app.develop_photo_id else {
        if app.develop_preview.photo_id.is_some() {
            app.develop_preview.clear();
        }
        return;
    };

    if app.develop_preview.is_active_for(id) {
        return;
    }

    let Some(cat) = app.catalog.as_ref() else {
        app.develop_preview.clear();
        return;
    };

    match cat.find_photo_by_id(id) {
        Ok(Some(photo)) => {
            app.develop_preview.open(
                id,
                PathBuf::from(&photo.path),
                photo.orientation,
            );
        }
        Ok(None) => {
            app.develop_preview.clear();
            app.develop_photo_id = None;
        }
        Err(e) => {
            app.develop_preview.fail(id, e.to_string());
        }
    }
}

/// Render the Develop page with adjustment sliders and RAW preview.
pub(crate) fn render(app: &mut App, ctx: &egui::Context) {
    ensure_preview(app);
    app.develop_preview.pump(ctx);

    // Right-side adjustment panel (rendered before CentralPanel so it
    // reserves space from the right edge).
    egui::SidePanel::right("develop_adjustments")
        .resizable(false)
        .default_width(260.0)
        .width_range(200.0..=400.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                let s = &mut app.develop;

                section_header(ui, "Light");
                slider(ui, "Exposure", &mut s.exposure, -5.0..=5.0);
                slider(ui, "Contrast", &mut s.contrast, -100.0..=100.0);
                slider(ui, "Highlights", &mut s.highlights, -100.0..=100.0);
                slider(ui, "Shadows", &mut s.shadows, -100.0..=100.0);
                slider(ui, "Whites", &mut s.whites, -100.0..=100.0);
                slider(ui, "Blacks", &mut s.blacks, -100.0..=100.0);

                ui.add_space(12.0);

                section_header(ui, "Presence");
                slider(ui, "Clarity", &mut s.clarity, -100.0..=100.0);
                slider(ui, "Vibrance", &mut s.vibrance, -100.0..=100.0);
                slider(ui, "Saturation", &mut s.saturation, -100.0..=100.0);

                ui.add_space(12.0);

                section_header(ui, "Color");
                slider(ui, "Temp", &mut s.temp, -100.0..=100.0);
                slider(ui, "Tint", &mut s.tint, -100.0..=100.0);
            });
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        if app.develop_photo_id.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label("Double-click a RAW photo in the Library to open Develop.");
            });
            return;
        }

        let loading = app.develop_preview.is_loading();
        let status = app.develop_preview.status.clone();
        let tex_info = app
            .develop_preview
            .texture()
            .map(|t| (t.id(), t.size_vec2()));

        if let Some((tex_id, size)) = tex_info {
            let avail = ui.available_size();
            let scale = (avail.x / size.x).min(avail.y / size.y).min(1.0);
            let display = size * scale;
            ui.centered_and_justified(|ui| {
                ui.image((tex_id, display));
            });
            if loading {
                if let Some(status) = status {
                    let origin = ui.min_rect().left_top() + egui::vec2(8.0, 8.0);
                    ui.allocate_new_ui(
                        egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                            origin,
                            egui::vec2(320.0, 24.0),
                        )),
                        |ui| {
                            ui.label(egui::RichText::new(status).small().weak());
                        },
                    );
                }
            }
        } else if let Some(status) = status {
            ui.centered_and_justified(|ui| {
                if loading {
                    ui.spinner();
                    ui.add_space(8.0);
                    ui.label(status);
                } else {
                    ui.colored_label(egui::Color32::LIGHT_RED, status);
                }
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
                ui.label("Loading…");
            });
        }
    });
}
