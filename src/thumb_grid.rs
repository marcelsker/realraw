//! Shared thumbnail-grid rendering used by both the import dialog and
//! the main library page.
//!
//! The module owns:
//! * the per-card layout (3:2 image area + filename strip),
//! * selection / in-catalog visual states (selectable opt-in),
//! * the row-by-row horizontally-centered grid layout,
//! * the GPU texture cache (so we re-upload at most once per cell).
//!
//! Callers provide the data and the bytes; we draw.

use std::collections::HashMap;
use std::sync::Mutex;

use eframe::egui;

/// Target longest edge for the decoded thumbnail, in pixels.
pub const THUMB_MAX_DIM: u32 = 1024;

/// Pixel size of the longest edge of every cell in the grid.
pub const THUMB_CELL: f32 = 156.0;
/// Min / max cells per row.
pub const MIN_COLS: usize = 3;
pub const MAX_COLS: usize = 8;
/// 3:2 aspect ratio (typical for full-frame cameras and most dSLRs /
/// mirrorless bodies). Used as the canonical card aspect when we
/// don't have real dimensions for the source.
pub const CARD_ASPECT_W: f32 = 3.0;
pub const CARD_ASPECT_H: f32 = 2.0;
/// Horizontal spacing between cards in a row, in pixels.
pub const COL_SPACING: f32 = 8.0;
/// Vertical spacing between rows, in pixels.
pub const ROW_SPACING: f32 = 8.0;
/// Height of the filename strip below the image.
pub const LABEL_H: f32 = 18.0;
/// Margin around the image inside its card.
pub const INNER_MARGIN: f32 = 4.0;

/// A decoded thumbnail ready to be uploaded to the GPU.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The largest dimension we asked for. Useful for layout.
    pub max_dim: u32,
}

/// Thumbnail bytes carried alongside the row data so the render
/// thread can read them without re-extracting.
#[derive(Debug, Clone)]
pub struct ThumbnailBytes {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What a card should look like. Most fields are optional; the
/// defaults describe the plain library card (no selection, no hints).
#[derive(Debug, Clone)]
pub struct ThumbCardConfig {
    /// Cell width in pixels.
    pub cell_w: f32,
    /// Force the "already in catalog" visual state (grey label).
    /// Used by the import dialog to mark duplicates.
    pub in_catalog: bool,
    /// If `Some`, use this text as the label instead of the
    /// caller's filename.
    pub label_override: Option<String>,
    /// Show a selection checkbox in the top-left corner.
    pub selectable: bool,
    /// Current checked state (when `selectable` is true).
    pub selected: bool,
    /// Enable click/double-click interaction (e.g. for the library
    /// page so double-click navigates to Develop mode).
    pub clickable: bool,
}

impl Default for ThumbCardConfig {
    fn default() -> Self {
        Self {
            cell_w: THUMB_CELL,
            in_catalog: false,
            label_override: None,
            selectable: false,
            selected: false,
            clickable: false,
        }
    }
}

/// Outcome of drawing a card, returned to the caller so it can update
/// its own state.
#[derive(Debug, Clone, Copy)]
pub struct CardResponse {
    /// `true` if the card is currently being hovered.
    pub hovered: bool,
    /// `true` if the user clicked "Remove" in the card's context
    /// menu. The caller should handle the removal flow.
    pub remove_requested: bool,
    /// `true` if the user clicked the selection checkbox.
    pub checkbox_toggled: bool,
    /// `true` if the card was double-clicked. Used by the library to
    /// navigate to Develop mode.
    pub double_clicked: bool,
    /// Screen-space rectangle of the card, in the caller's coordinate
    /// space.
    pub rect: egui::Rect,
}

impl Default for CardResponse {
    fn default() -> Self {
        Self {
            hovered: false,
            remove_requested: false,
            checkbox_toggled: false,
            double_clicked: false,
            rect: egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::ZERO),
        }
    }
}

/// Grid metrics computed from the available width.
#[derive(Debug, Clone, Copy)]
pub struct GridLayout {
    pub cells_per_row: usize,
    pub cell_w: f32,
    pub full_row_w: f32,
    /// Width of a single cell including its right-side column gap
    /// (but not the trailing gap on the last cell of a row).
    pub cell_pitch: f32,
}

/// Compute the grid metrics for the available width. Always clamps
/// to at least [`MIN_COLS`] so very narrow windows still render.
pub fn compute_grid(ui: &egui::Ui) -> GridLayout {
    let available = ui.available_width();
    let cell_w = THUMB_CELL.max(96.0);
    let cells_per_row = ((available / (cell_w + COL_SPACING)).floor() as usize).clamp(MIN_COLS, MAX_COLS);
    let cell_pitch = cell_w + COL_SPACING;
    let full_row_w = cells_per_row as f32 * cell_pitch - COL_SPACING;
    GridLayout {
        cells_per_row,
        cell_w,
        full_row_w,
        cell_pitch,
    }
}

/// Total height of a card (image + optional label + margin), used
/// for the vertical scroll area estimate. Pass `false` for
/// `show_label` when the library hides the label strip.
pub fn card_height(cell_w: f32, show_label: bool) -> f32 {
    let image_h = cell_w * (CARD_ASPECT_H / CARD_ASPECT_W);
    image_h + INNER_MARGIN * 2.0 + if show_label { LABEL_H } else { 0.0 }
}

/// Opaque, hashable cache key for a cell's GPU texture. The library
/// uses the photo's database id; the import dialog hashes the file
/// path. Either way, a stable key keeps the texture alive across
/// refreshes and avoids re-uploading the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(pub u64);

impl CacheKey {
    /// Hash a file path into a stable 64-bit cache key. Uses Rust's
    /// default hasher for speed; the values are process-local so
    /// we don't need cryptographic strength.
    pub fn from_path(path: &str) -> Self {
        use std::hash::{BuildHasher, Hasher, RandomState};
        let mut h = RandomState::new().build_hasher();
        h.write(path.as_bytes());
        CacheKey(h.finish())
    }

    /// Build a key directly from a database id.
    pub fn from_id(id: i64) -> Self {
        // Spread the bits so adjacent ids don't all map to a
        // contiguous slice of the texture atlas (if we ever use
        // one). The 0x9E3779B97F4A7C15 constant is the golden
        // ratio fractional; the multiply mixes the bits.
        CacheKey((id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Draw a single thumbnail card. Reserves its own space; the caller
/// just calls this inside a `ui.horizontal` (or any other layout
/// context) and we take care of the rect.
#[allow(clippy::too_many_arguments)]
pub fn thumb_card<K>(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    cache_key: K,
    config: &ThumbCardConfig,
    thumb_bytes: Option<&ThumbnailBytes>,
    thumb_error: Option<&str>,
    full_path: &str,
    textures: &Mutex<HashMap<K, egui::TextureHandle>>,
    item_id: Option<i64>,
) -> CardResponse
where
    K: std::hash::Hash + Eq + Copy + std::fmt::Display + Send,
{
    let cell_w = config.cell_w;
    let image_h = cell_w * (CARD_ASPECT_H / CARD_ASPECT_W);
    let show_label = config.label_override.is_some() || config.in_catalog;
    let card_h = image_h + INNER_MARGIN * 2.0 + if show_label { LABEL_H } else { 0.0 };
    let card_size = egui::vec2(cell_w, card_h);

    let sense = if config.selectable || config.clickable {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (card_rect, response) = ui.allocate_exact_size(card_size, sense);
    let response = response.on_hover_text(full_path);

    // Right-click context menu.
    let remove_clicked = std::cell::Cell::new(false);
    response.context_menu(|ui| {
        if ui.button("Open").clicked() {
            eprintln!("context menu: Open (not implemented)");
            ui.close_menu();
        }
        if item_id.is_some() && ui.button("Remove").clicked() {
            remove_clicked.set(true);
            ui.close_menu();
        }
        if ui.button("Reveal").clicked() {
            reveal_in_file_manager(full_path);
            ui.close_menu();
        }
    });

    let checkbox_toggled = config.selectable && response.clicked();
    let double_clicked = response.double_clicked();

    let card_response = CardResponse {
        hovered: response.hovered(),
        remove_requested: remove_clicked.get(),
        checkbox_toggled,
        double_clicked,
        rect: card_rect,
    };

    let in_catalog = config.in_catalog;
    let visuals = ui.style().visuals.clone();
    ui.painter().rect_filled(
        card_rect,
        egui::CornerRadius::same(4),
        visuals.widgets.noninteractive.bg_fill,
    );

    let border_color = if response.hovered() {
        visuals.widgets.hovered.bg_stroke.color
    } else {
        visuals.widgets.noninteractive.bg_stroke.color
    };
    ui.painter().rect_stroke(
        card_rect,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.0, border_color),
        egui::StrokeKind::Inside,
    );

    let image_rect = egui::Rect::from_min_size(
        card_rect.min + egui::vec2(INNER_MARGIN, INNER_MARGIN),
        egui::vec2(cell_w - INNER_MARGIN * 2.0, image_h),
    );

    draw_thumbnail(
        ctx,
        ui,
        cache_key,
        thumb_bytes,
        thumb_error,
        image_rect,
        textures,
    );

    if show_label {
        let label_rect = egui::Rect::from_min_size(
            egui::pos2(card_rect.min.x + INNER_MARGIN, image_rect.max.y + 1.0),
            egui::vec2(cell_w - INNER_MARGIN * 2.0, LABEL_H),
        );
        let label_color = if in_catalog {
            visuals.weak_text_color()
        } else {
            visuals.text_color()
        };
        let label_text = if in_catalog {
            "already imported".to_string()
        } else {
            config.label_override.clone().unwrap_or_default()
        };
        ui.painter().text(
            label_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label_text,
            egui::FontId::proportional(12.0),
            label_color,
        );
    }

    card_response
}

/// Draw the thumbnail itself (or a placeholder if not loaded yet).
/// Letterboxes the image into the 3:2 frame so non-3:2 sources
/// (square scans, video frames, 1:1 crops) don't distort.
pub fn draw_thumbnail<K>(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    cache_key: K,
    thumb_bytes: Option<&ThumbnailBytes>,
    thumb_error: Option<&str>,
    image_rect: egui::Rect,
    textures: &Mutex<HashMap<K, egui::TextureHandle>>,
) where
    K: std::hash::Hash + Eq + Copy + std::fmt::Display,
{
    if let Some(bytes) = thumb_bytes {
        let mut cache = textures.lock().unwrap();
        cache.entry(cache_key).or_insert_with(|| {
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [bytes.width as usize, bytes.height as usize],
                &bytes.rgba,
            );
            ctx.load_texture(
                format!("thumb-{cache_key}"),
                color,
                egui::TextureOptions::LINEAR,
            )
        });
        if let Some(tex) = cache.get(&cache_key) {
            let src_w = tex.size()[0] as f32;
            let src_h = tex.size()[1] as f32;
            let dest = fit_inside(image_rect, src_w, src_h);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            // Letterbox background.
            ui.painter().rect_filled(
                image_rect,
                egui::CornerRadius::same(2),
                ui.style().visuals.widgets.noninteractive.bg_fill,
            );
            ui.painter().image(tex.id(), dest, uv, egui::Color32::WHITE);
        }
    } else if let Some(err) = thumb_error {
        // Last attempt errored: paint the letterbox background
        // and a small error label centred in the cell. This gives
        // the user feedback for permanently broken files (e.g.
        // moved/deleted between import and library open) instead
        // of an infinite spinner.
        ui.painter().rect_filled(
            image_rect,
            egui::CornerRadius::same(2),
            ui.style().visuals.widgets.noninteractive.bg_fill,
        );
        ui.painter().text(
            image_rect.center(),
            egui::Align2::CENTER_CENTER,
            "err",
            egui::FontId::proportional(20.0),
            egui::Color32::LIGHT_RED,
        );
        let trimmed = err.chars().take(40).collect::<String>();
        ui.painter().text(
            egui::pos2(image_rect.center().x, image_rect.max.y - 4.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{trimmed}..."),
            egui::FontId::proportional(10.0),
            ui.style().visuals.weak_text_color(),
        );
        // Also place the full error on hover.
        // (Caller has already attached the full path on the card.)
    } else {
        // Still loading: paint an animated spinner directly in the
        // image rect. Using `Spinner::paint_at` (instead of
        // `ui.add(Spinner::new())` or `allocate_new_ui`) keeps the
        // spinner in pure paint space: it doesn't consume any
        // layout in the parent ui, so the label below the image
        // stays put when the thumbnail loads. The spinner itself
        // centres itself inside its rect (it draws at
        // `rect.center()`), so H+V centring is automatic.
        //
        // `paint_at` ignores the `.size(...)` setter and instead
        // fills the whole rect -- so we build a small square
        // centred in `image_rect` for it. 20 px is a comfortable
        // size that doesn't overwhelm the 156 px cell.
        const SPINNER_SIZE: f32 = 20.0;
        let spinner_rect = egui::Rect::from_center_size(image_rect.center(), egui::vec2(SPINNER_SIZE, SPINNER_SIZE));
        egui::Spinner::new().paint_at(ui, spinner_rect);
    }
}

/// Compute a rectangle that fits `src_w x src_h` inside `outer`,
/// preserving aspect ratio and centring the result.
pub fn fit_inside(outer: egui::Rect, src_w: f32, src_h: f32) -> egui::Rect {
    if src_w <= 0.0 || src_h <= 0.0 {
        return outer;
    }
    let outer_w = outer.width();
    let outer_h = outer.height();
    let scale = (outer_w / src_w).min(outer_h / src_h);
    let w = src_w * scale;
    let h = src_h * scale;
    let x = outer.min.x + (outer_w - w) * 0.5;
    let y = outer.min.y + (outer_h - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

/// Per-item info needed to render a grid cell. The grid helper takes
/// a slice of these and renders the rows. The caller supplies a
/// stable cache key for each cell via the `key_for` closure in
/// [`show_thumb_grid`] so a refresh that re-orders the list doesn't
/// invalidate loaded textures.
#[derive(Debug, Clone)]
pub struct GridItem {
    /// Stable id for this cell (e.g. `Photo::id`). The import dialog
    /// leaves this as `None` and hashes the path. The library
    /// sets it so the GPU texture survives catalog refreshes.
    pub id: Option<i64>,
    /// Full file path. Used for hover and as the default label.
    pub full_path: String,
    /// Decoded thumbnail bytes, or `None` while the worker is
    /// still loading (or has failed; see `thumb_error`).
    pub thumb_bytes: Option<ThumbnailBytes>,
    /// Human-readable error from the last thumbnail attempt. When
    /// `thumb_bytes` is `None` *and* this is `Some`, the cell
    /// shows the error text instead of a spinner so the user has
    /// feedback for permanently broken files.
    pub thumb_error: Option<String>,
    /// Visual / behavioural config for this cell.
    pub config: ThumbCardConfig,
    /// Screen-space rectangle of the card, updated by the renderer
    /// each frame.
    pub rect: egui::Rect,
}

impl Default for GridItem {
    fn default() -> Self {
        Self {
            id: None,
            full_path: String::new(),
            thumb_bytes: None,
            thumb_error: None,
            config: ThumbCardConfig::default(),
            rect: egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::ZERO),
        }
    }
}

/// Render a centred, row-by-row grid of items inside a scrollable
/// area constrained to `max_height`. Each row is a `ui.horizontal`
/// block so the partial trailing row stays at the same horizontal
/// offset as the full rows above it.
///
/// Returns a tuple of `(remove_ids, double_clicked_ids)`.
pub fn show_thumb_grid<K, F>(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    items: &mut [GridItem],
    textures: &Mutex<HashMap<K, egui::TextureHandle>>,
    max_height: f32,
    key_for: F,
) -> (Vec<i64>, Vec<i64>)
where
    K: std::hash::Hash + Eq + Copy + std::fmt::Display + Send,
    F: Fn(&GridItem) -> K,
{
    let scroll = egui::ScrollArea::vertical()
        .max_height(max_height)
        .auto_shrink([false, false]);
    scroll.show(ui, |ui| {
        show_thumb_rows(ctx, ui, items, textures, key_for)
    }).inner
}

/// Row-by-row thumbnail grid rendering without a scroll wrapper.
/// Useful when the caller wants to embed the grid inside its own
/// scroll area (e.g. to insert date dividers between groups).
///
/// Returns a tuple of `(remove_ids, double_clicked_ids)`.
pub(crate) fn show_thumb_rows<K, F>(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    items: &mut [GridItem],
    textures: &Mutex<HashMap<K, egui::TextureHandle>>,
    key_for: F,
) -> (Vec<i64>, Vec<i64>)
where
    K: std::hash::Hash + Eq + Copy + std::fmt::Display + Send,
    F: Fn(&GridItem) -> K,
{
    let layout = compute_grid(ui);
    let n = items.len();
    let mut remove_ids = Vec::new();
    let mut activate_ids = Vec::new();

    for (row_idx, row_start) in (0..n).step_by(layout.cells_per_row).enumerate() {
        let row_end = (row_start + layout.cells_per_row).min(n);
        let in_row = row_end - row_start;
        let row_w = in_row as f32 * layout.cell_pitch - COL_SPACING;
        let available = ui.available_width();
        let reference_w = layout.full_row_w.max(row_w);
        let pad = ((available - reference_w) * 0.5).max(0.0);

        let resp = ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.spacing_mut().item_spacing.x = COL_SPACING;
            for item in items[row_start..row_end].iter_mut() {
                let cache_key = key_for(item);
                let resp = thumb_card(
                    ctx,
                    ui,
                    cache_key,
                    &item.config,
                    item.thumb_bytes.as_ref(),
                    item.thumb_error.as_deref(),
                    &item.full_path,
                    textures,
                    item.id,
                );
                item.rect = resp.rect;
                if resp.checkbox_toggled {
                    item.config.selected = !item.config.selected;
                }
                if resp.remove_requested && let Some(id) = item.id {
                    remove_ids.push(id);
                }
                if resp.double_clicked && let Some(id) = item.id {
                    activate_ids.push(id);
                }
            }
        });
        let _ = resp;
        if row_idx + 1 < n.div_ceil(layout.cells_per_row) {
            ui.add_space(ROW_SPACING);
        }
    }

    (remove_ids, activate_ids)
}

/// Open the system's file manager at the folder containing `path` and
/// **select** the file itself, so the user sees it highlighted.
///
/// The `open` crate would open the file with its default application or
/// open the parent folder — it does not support "reveal with selection"
/// — so we use platform-specific commands directly.
fn reveal_in_file_manager(path: &str) {
    let path = std::path::Path::new(path);

    #[cfg(target_os = "macos")]
    {
        // `open -R` reveals and selects the file in Finder.
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }

    #[cfg(target_os = "windows")]
    {
        // `/select,` opens Explorer with the file highlighted.
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,\"{}\"", path.display()))
            .spawn();
    }

    #[cfg(target_os = "linux")]
    {
        // Try the freedesktop FileManager1 D-Bus interface first
        // (works in GNOME/Nautilus, KDE/Dolphin). If that fails,
        // fall back to `xdg-open` on the parent directory. We
        // still prefer the D-Bus approach because xdg-open on a
        // folder just opens it without selecting the file.
        let file_uri = format!(
            "file://{}",
            path.canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .display()
        );
        let dbus = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{file_uri}"),
                "string:\"\"",
            ])
            .spawn();
        if dbus.is_err() {
            if let Some(parent) = path.parent().and_then(|p| p.to_str()) {
                let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        if let Some(parent) = path.parent().and_then(|p| p.to_str()) {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_inside_preserves_aspect_and_centers() {
        let outer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        let r = fit_inside(outer, 300.0, 200.0);
        assert!((r.width() - 100.0).abs() < 0.01);
        assert!((r.height() - 100.0 * (2.0 / 3.0)).abs() < 0.01);
        assert_eq!(r.min.x, 0.0);
        assert!(r.min.y > 0.0);
    }

    #[test]
    fn fit_inside_letterboxes_tall_sources() {
        let outer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        let r = fit_inside(outer, 200.0, 200.0);
        assert!((r.width() - 100.0).abs() < 0.01);
        assert!((r.height() - 100.0).abs() < 0.01);
        assert_eq!(r.min, egui::pos2(0.0, 0.0));
    }

    #[test]
    fn card_height_with_label() {
        let h = card_height(THUMB_CELL, true);
        // 156 * (2/3) + 18 + 8 = 104 + 18 + 8 = 130
        assert!((h - 130.0).abs() < 0.5);
    }

    #[test]
    fn card_height_without_label() {
        let h = card_height(THUMB_CELL, false);
        // 156 * (2/3) + 8 = 104 + 8 = 112
        assert!((h - 112.0).abs() < 0.5);
    }
}
