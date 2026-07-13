use std::time::{Duration, Instant};
use eframe::egui;

pub struct Toast {
    pub message: String,
    pub created_at: Instant,
    pub duration: Duration,
}

#[derive(Default)]
pub struct Toasts {
    list: Vec<Toast>,
}

impl Toasts {
    pub fn add(&mut self, message: impl Into<String>) {
        self.list.push(Toast {
            message: message.into(),
            created_at: Instant::now(),
            duration: Duration::from_secs(3),
        });
    }

    pub fn update(&mut self) {
        let now = Instant::now();
        self.list.retain(|toast| now.duration_since(toast.created_at) < toast.duration);
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        self.update();
        if self.list.is_empty() {
            return;
        }

        // Keep repainting to animate the toast lifecycle
        ctx.request_repaint_after(Duration::from_millis(16));

        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 10.0);

                    let now = Instant::now();
                    for toast in &self.list {
                        let age = now.duration_since(toast.created_at);
                        let remaining = toast.duration.saturating_sub(age);
                        
                        // Calculate opacity/alpha for fade in and fade out
                        let alpha = if age.as_secs_f32() < 0.25 {
                            // Fade in
                            age.as_secs_f32() / 0.25
                        } else if remaining.as_secs_f32() < 0.35 {
                            // Fade out
                            remaining.as_secs_f32() / 0.35
                        } else {
                            1.0
                        };

                        // Use theme-aware colors with alpha transparency
                        let dark_mode = ui.visuals().dark_mode;
                        
                        let background_color = if dark_mode {
                            egui::Color32::from_rgba_unmultiplied(24, 24, 27, (alpha * 240.0) as u8) // Zinc 900
                        } else {
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, (alpha * 240.0) as u8) // White
                        };

                        let border_color = if dark_mode {
                            egui::Color32::from_rgba_unmultiplied(63, 63, 70, (alpha * 255.0) as u8) // Zinc 700
                        } else {
                            egui::Color32::from_rgba_unmultiplied(228, 228, 231, (alpha * 255.0) as u8) // Zinc 200
                        };

                        let text_color = if dark_mode {
                            egui::Color32::from_rgba_unmultiplied(244, 244, 245, (alpha * 255.0) as u8) // Zinc 100
                        } else {
                            egui::Color32::from_rgba_unmultiplied(24, 24, 27, (alpha * 255.0) as u8) // Zinc 900
                        };

                        // Premium UI container styling
                        egui::Frame::NONE
                            .fill(background_color)
                            .stroke(egui::Stroke::new(1.0, border_color))
                            .corner_radius(egui::CornerRadius::same(4))
                            .inner_margin(egui::Margin {
                                left: 14,
                                right: 14,
                                top: 10,
                                bottom: 10,
                            })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.colored_label(text_color, &toast.message);
                                });
                            });
                    }
                });
            });
    }
}
