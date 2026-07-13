use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_phosphor::variants::regular;

use crate::app::App;
use crate::develop::pipeline::{auto_wb, eyedropper_wb};
use crate::develop::settings::WhiteBalancePreset;
use crate::develop::DevelopSettings;
use crate::import::xmp::update_sidecar_develop;

/// Quiet period after the last develop change before regenerating the
/// library thumbnail (low priority, coalesced).
pub const THUMB_REFRESH_DEBOUNCE: Duration = Duration::from_millis(700);

/// Mark the open photo for a debounced library thumbnail refresh.
pub(crate) fn schedule_thumb_refresh(app: &mut App) {
    let Some(id) = app.develop_photo_id.or(app.develop_loaded_id) else {
        return;
    };
    app.develop_thumb_due = Some((id, Instant::now()));
}

/// Fire any due developed-thumbnail refresh (debounce + not dragging).
pub(crate) fn pump_thumb_refresh(app: &mut App, ctx: &egui::Context) {
    let Some((id, at)) = app.develop_thumb_due else {
        return;
    };
    if app.develop_dragging {
        ctx.request_repaint_after(Duration::from_millis(50));
        return;
    }
    let wait = THUMB_REFRESH_DEBOUNCE.saturating_sub(at.elapsed());
    if !wait.is_zero() {
        ctx.request_repaint_after(wait);
        return;
    }
    app.develop_thumb_due = None;
    fire_thumb_refresh(app, id);
}

/// Immediately run a pending thumb refresh (e.g. leaving the photo / page).
pub(crate) fn flush_thumb_refresh(app: &mut App) {
    let Some((id, _)) = app.develop_thumb_due.take() else {
        return;
    };
    if app.develop_dragging {
        // Still schedule so pump can fire after release.
        app.develop_thumb_due = Some((id, Instant::now()));
        return;
    }
    fire_thumb_refresh(app, id);
}

fn fire_thumb_refresh(app: &mut App, photo_id: i64) {
    let Some(cat) = app.catalog.clone() else {
        return;
    };
    let Ok(Some(photo)) = cat.find_photo_by_id(photo_id) else {
        return;
    };
    let tone = if app.develop_loaded_id == Some(photo_id) {
        app.develop.tone()
    } else {
        cat.get_develop(photo_id)
            .map(|s| s.tone())
            .unwrap_or_default()
    };
    app.library.schedule_developed_thumb_refresh(
        cat.dir().to_path_buf(),
        photo_id,
        PathBuf::from(&photo.path),
        photo.orientation,
        tone,
    );
}

/// Result of interacting with a develop slider this frame.
struct SliderHit {
    changed: bool,
    dragging: bool,
}

fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> SliderHit {
    let mut hit = SliderHit {
        changed: false,
        dragging: false,
    };
    ui.horizontal(|ui| {
        ui.add_sized([80.0, 0.0], egui::Label::new(label));
        let r = ui.add(egui::Slider::new(value, range).show_value(true));
        if r.changed() || r.drag_stopped() {
            hit.changed = true;
        }
        if (r.dragged() || r.is_pointer_button_down_on()) && !r.drag_stopped() {
            hit.dragging = true;
        }
    });
    hit
}

fn section_header(ui: &mut egui::Ui, label: &str) {
    ui.label(egui::RichText::new(label).strong().size(13.0));
    ui.separator();
}

/// Determine the displayed preset name, accounting for auto-WB tracking.
fn current_preset_display(app: &App) -> WhiteBalancePreset {
    if app.develop.temp == 0.0 && app.develop.tint == 0.0 {
        return WhiteBalancePreset::AsShot;
    }
    if (app.develop.temp - app.last_auto_temp).abs() < 1.0
        && (app.develop.tint - app.last_auto_tint).abs() < 1.0
    {
        return WhiteBalancePreset::Auto;
    }
    app.develop.wb_preset()
}

/// Apply a white-balance preset and mark state as dirty.
fn apply_preset(app: &mut App, preset: WhiteBalancePreset, any: &mut bool) {
    app.develop.apply_wb_preset(preset);
    if preset == WhiteBalancePreset::Auto {
        if let Some(linear) = &app.develop_preview.linear {
            let (t, ti) = auto_wb(linear);
            app.develop.temp = t;
            app.develop.tint = ti;
            app.last_auto_temp = t;
            app.last_auto_tint = ti;
        }
    }
    *any = true;
    app.develop_preview.set_tone(app.develop.tone(), false);
}

/// `Loading.` → `Loading..` → `Loading...` loop (~2.5 cycles/sec).
fn loading_dots(ctx: &egui::Context) -> String {
    let t = ctx.input(|i| i.time);
    let n = ((t * 2.5) as usize % 3) + 1;
    format!("Loading{}", ".".repeat(n))
}

/// Persist current develop settings to the catalog and XMP sidecar.
pub(crate) fn flush_develop(app: &mut App) {
    if !app.develop_dirty {
        return;
    }
    let Some(id) = app.develop_loaded_id.or(app.develop_photo_id) else {
        return;
    };
    let Some(cat) = app.catalog.clone() else {
        return;
    };

    if let Err(e) = cat.set_develop(id, &app.develop) {
        eprintln!("save develop settings: {e}");
        return;
    }

    if let Ok(Some(photo)) = cat.find_photo_by_id(id) {
        let path = PathBuf::from(&photo.path);
        if let Err(e) = update_sidecar_develop(&path, &app.develop) {
            eprintln!("write develop XMP sidecar: {e}");
        }
    }

    app.develop_dirty = false;
    // Ensure a thumb refresh is queued after settings hit disk.
    schedule_thumb_refresh(app);
}

/// Load develop settings for `id` into the app (flushes previous dirty first).
fn load_develop_for(app: &mut App, id: i64) {
    if app.develop_loaded_id == Some(id) {
        return;
    }
    flush_develop(app);
    flush_thumb_refresh(app);

    let settings = app
        .catalog
        .as_ref()
        .and_then(|c| c.get_develop(id).ok())
        .unwrap_or_default();
    app.develop = settings;
    app.develop_loaded_id = Some(id);
    app.develop_dirty = false;
    app.develop_dragging = false;
    app.last_auto_temp = 0.0;
    app.last_auto_tint = 0.0;
}

/// Ensure the develop preview is loading the currently selected photo.
fn ensure_preview(app: &mut App) {
    let Some(id) = app.develop_photo_id else {
        if app.develop_preview.photo_id.is_some() || app.develop_loaded_id.is_some() {
            flush_develop(app);
            flush_thumb_refresh(app);
            app.develop_preview.clear();
            app.develop_loaded_id = None;
        }
        return;
    };

    load_develop_for(app, id);

    if app.develop_preview.is_active_for(id) {
        return;
    }

    let Some(cat) = app.catalog.as_ref() else {
        app.develop_preview.clear();
        return;
    };

    match cat.find_photo_by_id(id) {
        Ok(Some(photo)) => {
            let catalog_dir = cat.dir().to_path_buf();
            app.develop_preview.open(
                id,
                PathBuf::from(&photo.path),
                photo.orientation,
                catalog_dir,
                app.develop.tone(),
            );
        }
        Ok(None) => {
            flush_develop(app);
            app.develop_preview.clear();
            app.develop_photo_id = None;
            app.develop_loaded_id = None;
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

    egui::SidePanel::right("develop_adjustments")
        .resizable(false)
        .default_width(260.0)
        .width_range(200.0..=400.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                let mut any = false;
                let mut dragging = false;

                section_header(ui, "White Balance");

                // ── Preset dropdown + Eyedropper ──
                let display = current_preset_display(app);
                ui.horizontal(|ui| {
                    ui.add_sized([80.0, 0.0], egui::Label::new("Preset"));
                    egui::ComboBox::from_id_salt("wb_preset")
                        .selected_text(display.name())
                        .show_ui(ui, |ui| {
                            for preset in WhiteBalancePreset::ALL {
                                if ui
                                    .selectable_label(display == *preset, preset.name())
                                    .clicked()
                                {
                                    if *preset != display {
                                        apply_preset(app, *preset, &mut any);
                                    }
                                }
                            }
                        });
                    let eye_btn = egui::Button::new(regular::EYEDROPPER)
                        .selected(app.eyedropper_active)
                        .min_size(egui::vec2(28.0, 0.0));
                    if ui.add(eye_btn).clicked() {
                        app.eyedropper_active = !app.eyedropper_active;
                    }
                });

                // ── Kelvin slider ──
                {
                    let mut kelvin = app.develop.kelvin();
                    ui.horizontal(|ui| {
                        ui.add_sized([80.0, 0.0], egui::Label::new("Kelvin"));
                        let r = ui.add(
                            egui::Slider::new(&mut kelvin, 2000.0..=25000.0).show_value(true),
                        );
                        if r.changed() || r.drag_stopped() {
                            kelvin = kelvin.clamp(2000.0, 25000.0);
                            app.develop.set_kelvin(kelvin);
                            any = true;
                            app.develop_preview
                                .set_tone(app.develop.tone(), r.drag_stopped());
                        }
                        if r.dragged() && !r.drag_stopped() {
                            dragging = true;
                        }
                    });
                }

                // ── Tint slider ──
                {
                    let hit = slider_row(ui, "Tint", &mut app.develop.tint, -100.0..=100.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }

                ui.add_space(12.0);
                section_header(ui, "Light");
                {
                    let hit = slider_row(ui, "Exposure", &mut app.develop.exposure, -5.0..=5.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }
                {
                    let hit = slider_row(ui, "Contrast", &mut app.develop.contrast, -100.0..=100.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }
                {
                    let hit =
                        slider_row(ui, "Highlights", &mut app.develop.highlights, -100.0..=100.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }
                {
                    let hit = slider_row(ui, "Shadows", &mut app.develop.shadows, -100.0..=100.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }
                {
                    let hit = slider_row(ui, "Whites", &mut app.develop.whites, -100.0..=100.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }
                {
                    let hit = slider_row(ui, "Blacks", &mut app.develop.blacks, -100.0..=100.0);
                    if hit.changed {
                        any = true;
                        app.develop_preview
                            .set_tone(app.develop.tone(), hit.dragging);
                    }
                    dragging |= hit.dragging;
                }

                ui.add_space(12.0);
                section_header(ui, "Presence");
                {
                    let h = slider_row(ui, "Clarity", &mut app.develop.clarity, -100.0..=100.0);
                    any |= h.changed;
                    dragging |= h.dragging;
                }
                {
                    let h = slider_row(ui, "Vibrance", &mut app.develop.vibrance, -100.0..=100.0);
                    any |= h.changed;
                    dragging |= h.dragging;
                }
                {
                    let h = slider_row(ui, "Saturation", &mut app.develop.saturation, -100.0..=100.0);
                    any |= h.changed;
                    dragging |= h.dragging;
                }

                if any {
                    app.develop_dirty = true;
                    schedule_thumb_refresh(app);
                }
                app.develop_dragging = dragging;

                ui.add_space(16.0);
                if ui.button("Reset").clicked() {
                    app.develop = DevelopSettings::default();
                    app.develop_dirty = true;
                    app.develop_dragging = false;
                    app.develop_preview
                        .set_tone(app.develop.tone(), false);
                    schedule_thumb_refresh(app);
                }
            });
        });

    if app.develop_dirty && !app.develop_dragging {
        flush_develop(app);
    }

    if app.eyedropper_active {
        ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::Crosshair);
    }

    egui::CentralPanel::default().show(ctx, |ui| {
        if app.develop_photo_id.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label("Double-click a RAW photo in the Library to open Develop.");
            });
            return;
        }

        let avail = ui.available_size();
        let ppp = ui.ctx().pixels_per_point();
        app.develop_preview.set_view_size(avail, ppp);

        let loading = app.develop_preview.is_loading();
        let demosaic_progress = app.develop_preview.demosaic_progress();
        let status = app.develop_preview.status.clone();
        let tex_info = app
            .develop_preview
            .texture()
            .map(|t| (t.id(), t.size_vec2()));
        let reveal = app
            .develop_preview
            .reveal_texture()
            .map(|t| t.id());
        let reveal_alpha = app.develop_preview.reveal_alpha();

        if loading || reveal_alpha.is_some() || demosaic_progress.is_some() {
            ctx.request_repaint();
        }

        if let Some((base_id, size)) = tex_info {
            // Stable fit from locked aspect — never depends on texture pixel size.
            let aspect = app
                .develop_preview
                .display_aspect()
                .unwrap_or_else(|| (size.x / size.y.max(1.0)).max(0.01));
            let display = {
                let mut w = avail.x;
                let mut h = w / aspect;
                if h > avail.y {
                    h = avail.y;
                    w = h * aspect;
                }
                egui::vec2(w.max(1.0), h.max(1.0))
            };
            ui.centered_and_justified(|ui| {
                let sense = if app.eyedropper_active {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                };
                let (rect, response) = ui.allocate_exact_size(display, sense);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                // Base stays the thumbnail for the whole crossfade (no pop).
                ui.painter().image(base_id, rect, uv, egui::Color32::WHITE);
                // Demosaic fades in on top: result = demosaic*a + thumb*(1-a).
                if let (Some(rev_id), Some(a)) = (reveal, reveal_alpha) {
                    if a > 0.001 {
                        let a_u8 = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
                        let tint = egui::Color32::from_rgba_unmultiplied(255, 255, 255, a_u8);
                        ui.painter().image(rev_id, rect, uv, tint);
                    }
                }
                // Eyedropper click: sample pixel and compute WB.
                if app.eyedropper_active && response.clicked() {
                    if let Some(pos) = response.hover_pos() {
                        let u = ((pos.x - rect.min.x) / rect.size().x).clamp(0.0, 1.0);
                        let v = ((pos.y - rect.min.y) / rect.size().y).clamp(0.0, 1.0);
                        if let Some((r, g, b)) = app.develop_preview.sample_pixel(u, v) {
                            let (temp, tint) = eyedropper_wb(r, g, b);
                            app.develop.temp = temp;
                            app.develop.tint = tint;
                            app.develop_preview.set_tone(app.develop.tone(), false);
                            app.develop_dirty = true;
                            schedule_thumb_refresh(app);
                        }
                    }
                    app.eyedropper_active = false;
                }
            });
            if let Some(p) = demosaic_progress {
                paint_demosaic_progress(ui, p);
            } else if loading {
                let origin = ui.min_rect().left_top() + egui::vec2(8.0, 8.0);
                let text = loading_dots(ctx);
                ui.allocate_new_ui(
                    egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                        origin,
                        egui::vec2(320.0, 24.0),
                    )),
                    |ui| {
                        ui.label(egui::RichText::new(text).small().weak());
                    },
                );
            }
        } else if let Some(p) = demosaic_progress {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.label("Demosaicing…");
                    ui.add_space(8.0);
                    ui.add(
                        egui::ProgressBar::new(p)
                            .desired_width(280.0)
                            .show_percentage()
                            .animate(true),
                    );
                });
            });
        } else if let Some(status) = status {
            ui.centered_and_justified(|ui| {
                if loading {
                    ui.spinner();
                    ui.add_space(8.0);
                    ui.label(loading_dots(ctx));
                } else {
                    ui.colored_label(egui::Color32::LIGHT_RED, status);
                }
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
                ui.label(loading_dots(ctx));
            });
        }
    });
}

/// Top-left overlay progress while demosaic is running over a placeholder.
fn paint_demosaic_progress(ui: &mut egui::Ui, progress: f32) {
    let origin = ui.min_rect().left_top() + egui::vec2(8.0, 8.0);
    ui.allocate_new_ui(
        egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
            origin,
            egui::vec2(280.0, 40.0),
        )),
        |ui| {
            ui.label(egui::RichText::new("Demosaicing…").small().weak());
            ui.add(
                egui::ProgressBar::new(progress)
                    .desired_width(260.0)
                    .show_percentage()
                    .animate(true),
            );
        },
    );
}
