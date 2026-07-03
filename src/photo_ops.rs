//! Photo-level operations (remove, rename, …) that span the catalog,
//! the thumbnail cache, and the UI.
//!
//! Each operation is exposed as a standalone function for direct use
//! and a dialog struct for the UI confirmation flow backed by the
//! task system.
//!
//! # Example
//!
//! ```ignore
//! let mut dialog = RemoveDialog::default();
//! // … user right-clicks a thumbnail and picks "Remove" …
//! dialog.request(photo_id, &photo.path);
//! // … every frame …
//! if dialog.show(ctx, task_manager, catalog_arc).unwrap_or(false) {
//!     library.refresh(&catalog, None);
//! }
//! ```

use std::path::Path;
use std::sync::Arc;

use eframe::egui;

use crate::catalog::thumbnail_cache;
use crate::catalog::Catalog;
use crate::task::{Task, TaskContext, TaskId, TaskManager};

// ---------------------------------------------------------------------------
// Standalone helpers
// ---------------------------------------------------------------------------

/// Delete a photo from the catalog database and remove its cached
/// thumbnail from disk. Returns `true` if the photo existed and was
/// deleted.
///
/// This is a synchronous helper. For background deletion with progress
/// reporting, use [`spawn_delete_task`] instead.
pub fn delete_photo(catalog: &Catalog, photo_id: i64) -> Result<bool, String> {
    let deleted = catalog.delete_photo(photo_id).map_err(|e| e.to_string())?;
    if deleted {
        let thumb_path = thumbnail_cache::thumbnail_path(catalog.dir(), photo_id);
        let _ = std::fs::remove_file(&thumb_path);
    }
    Ok(deleted)
}

/// Spawn a background task that deletes a photo from the catalog and
/// removes its cached thumbnail. Returns the task id so callers can
/// track completion.
pub fn spawn_delete_task(
    mgr: &mut TaskManager,
    catalog: Arc<Catalog>,
    photo_id: i64,
    path: &str,
) -> TaskId {
    let filename = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("#{photo_id}"));
    let task = Task::new(
        format!("Remove {filename}"),
        format!("Delete photo #{photo_id} from catalog"),
    )
    .work(move |_ctx: &TaskContext| {
        let deleted = catalog
            .delete_photo(photo_id)
            .map_err(|e| format!("DB delete failed: {e}"))?;
        if deleted {
            let thumb_path = thumbnail_cache::thumbnail_path(catalog.dir(), photo_id);
            let _ = std::fs::remove_file(&thumb_path);
        }
        Ok(())
    });
    let tid = mgr.add_task(task);
    mgr.start();
    tid
}

/// Spawn a group of background tasks that delete multiple photos
/// from the catalog, each with its own progress entry.
pub fn spawn_batch_delete_tasks(
    mgr: &mut TaskManager,
    catalog: Arc<Catalog>,
    ids: &[i64],
    paths: &[String],
) -> TaskId {
    let group_id = mgr.add_group(
        format!("Remove {} photos", ids.len()),
        None,
    );
    let mut last_tid = None;
    for (photo_id, path) in ids.iter().zip(paths.iter()) {
        let filename = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("#{photo_id}"));
        let cat = catalog.clone();
        let pid = *photo_id;
        let task = Task::new(
            format!("Remove {filename}"),
            format!("Delete photo #{pid} from catalog"),
        )
        .group(group_id)
        .work(move |_ctx: &TaskContext| {
            let deleted = cat
                .delete_photo(pid)
                .map_err(|e| format!("DB delete failed: {e}"))?;
            if deleted {
                let thumb_path = thumbnail_cache::thumbnail_path(cat.dir(), pid);
                let _ = std::fs::remove_file(&thumb_path);
            }
            Ok(())
        });
        last_tid = Some(mgr.add_task(task));
    }
    mgr.start();
    last_tid.unwrap()
}

// ---------------------------------------------------------------------------
// Remove-confirmation dialog
// ---------------------------------------------------------------------------

/// A pending or in-progress removal request, either for a single
/// photo or a batch of selected photos.
#[derive(Clone)]
struct RemoveRequest {
    ids: Vec<i64>,
    paths: Vec<String>,
}

/// State for the "Remove photo?" confirmation dialog.
///
/// Call [`request`][Self::request] when the user triggers a removal
/// (e.g. from a context menu), then call [`show`][Self::show] every
/// frame. The dialog handles the confirmation, spawns a background
/// task for the deletion, and reports completion.
pub struct RemoveDialog {
    /// Pending removal request while the confirmation dialog is visible.
    pending: Option<RemoveRequest>,
    /// Error message to display inside the dialog, if the last
    /// operation failed.
    error: Option<String>,
    /// Task id of the in-flight deletion, if any.
    task_id: Option<TaskId>,
}

impl Default for RemoveDialog {
    fn default() -> Self {
        Self {
            pending: None,
            error: None,
            task_id: None,
        }
    }
}

impl RemoveDialog {
    /// Request removal of the given photo. Opens the confirmation
    /// dialog on the next [`show`][Self::show] call.
    ///
    /// Only one removal can be in-flight at a time; subsequent
    /// requests while a dialog is pending or a task is running are
    /// silently ignored.
    pub fn request(&mut self, photo_id: i64, path: &str) {
        if self.pending.is_some() || self.task_id.is_some() {
            return;
        }
        self.pending = Some(RemoveRequest {
            ids: vec![photo_id],
            paths: vec![path.to_owned()],
        });
        self.error = None;
    }

    /// Request removal of multiple photos at once. Opens the
    /// confirmation dialog on the next [`show`][Self::show] call.
    ///
    /// Only one removal can be in-flight at a time; subsequent
    /// requests while a dialog is pending or a task is running are
    /// silently ignored.
    pub fn request_batch(&mut self, ids: &[i64], paths: &[String]) {
        if self.pending.is_some() || self.task_id.is_some() {
            return;
        }
        self.pending = Some(RemoveRequest {
            ids: ids.to_vec(),
            paths: paths.to_vec(),
        });
        self.error = None;
    }

    /// Returns `true` when there is a pending removal or a running
    /// deletion task.
    pub fn active(&self) -> bool {
        self.pending.is_some() || self.task_id.is_some()
    }

    /// Render the confirmation modal and manage the background
    /// deletion task.
    ///
    /// Returns `Ok(true)` if a deletion task just completed (the
    /// caller should refresh its photo list), `Ok(false)` if no
    /// action is needed, or `Err` on failure.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        mgr: &mut TaskManager,
        catalog: Arc<Catalog>,
    ) -> Result<bool, String> {
        // --- poll in-flight task -------------------------------------------
        if let Some(task_id) = self.task_id {
            if mgr.get_task(task_id).is_none_or(|t| t.status().is_terminal()) {
                self.task_id = None;
            }
            if self.task_id.is_none() {
                // Task finished. Grab the error, if any, from the
                // task snapshot on the last frame (get_task above
                // already cleared).
                return Ok(true);
            }
            return Ok(false);
        }

        // --- show confirmation dialog --------------------------------------
        let Some(req) = self.pending.clone() else {
            return Ok(false);
        };

        let mut confirmed = false;

        let response = egui::Modal::new(egui::Id::new("remove_photo_modal")).show(ctx, |ui| {
            ui.set_max_width(300.0);
            ui.vertical_centered(|ui| {
                ui.heading("Remove photo");
                ui.add_space(4.0);
                if req.ids.len() > 1 {
                    ui.label("Are you sure you want to remove these photos?");
                    ui.label(
                        egui::RichText::new(format!("You will remove {} files.", req.ids.len()))
                            .size(14.0)
                            .strong(),
                    );
                } else {
                    ui.label("Are you sure you want to remove this photo?");
                    ui.label(
                        egui::RichText::new(
                            Path::new(&req.paths[0])
                                .file_name()
                                .map(|n| n.to_string_lossy())
                                .unwrap_or_else(|| req.paths[0].as_str().into()),
                        )
                        .size(14.0)
                        .strong(),
                    );
                }
                ui.add_space(4.0);
                ui.label("The file(s) on disk will not be affected.");
                ui.add_space(8.0);

                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let remove_btn = egui::Button::new("Remove")
                                .fill(egui::Color32::from_rgb(180, 40, 40));
                            if ui.add(remove_btn).clicked() {
                                confirmed = true;
                            }
                            ui.add_space(8.0);
                            if ui.button("Cancel").clicked() {
                                self.pending = None;
                                self.error = None;
                            }
                        },
                    );
                });
            });
        });

        if response.should_close() {
            self.pending = None;
            self.error = None;
        } else if confirmed {
            if req.ids.len() > 1 {
                let tid = spawn_batch_delete_tasks(mgr, catalog.clone(), &req.ids, &req.paths);
                self.task_id = Some(tid);
            } else {
                let cat = catalog.clone();
                let tid = spawn_delete_task(mgr, cat, req.ids[0], &req.paths[0]);
                self.task_id = Some(tid);
            }
            self.pending = None;
            self.error = None;
        }

        Ok(false)
    }
}
