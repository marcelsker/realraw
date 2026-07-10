//! realraw -- an open source Lightroom alternative.
//!
//! This crate exposes:
//! * [`catalog`] -- SQLite-backed Lightroom-style catalog (photos, folders,
//!   collections, keywords).
//! * [`task`] -- background task system with progress reporting,
//!   dependencies, smart grouping, and egui widgets.
//! * [`thumb_grid`] -- shared thumbnail card + grid rendering used by
//!   both the import dialog and the main library page.
//! * [`import`] -- photo import pipeline: discovery, EXIF, embedded
//!   thumbnails, and the in-window import dialog.
//! * [`develop`] -- RAW develop preview (embedded JPEG + demosaic).
//! * [`app`] -- top-level `App` state + eframe integration.

pub mod app;
pub mod catalog;
pub mod develop;
pub mod import;
pub mod photo_ops;
pub mod task;
pub mod thumb_grid;

/// Raw bytes of the application logo / icon (64x64 PNG).
pub static ICON_PNG: &[u8] = include_bytes!("../assets/icon-64.png");
