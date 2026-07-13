//! Background import worker.
//!
//! [`import_batch`] takes a list of files (or directories) and the
//! already-discovered candidates, then:
//! 1. Ensures the parent folder exists in the catalog.
//! 2. Computes a SHA-1 hash (sequential file read).
//! 3. Extracts EXIF data (kamadak-exif).
//! 4. Upserts a row into `photos`.
//! 5. Reports progress through a `TaskContext`.
//!
//! Each file is its own task. A single parent "Import" group is created
//! so the user can cancel the whole batch with one click.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use sha1::{Digest, Sha1};
use time::OffsetDateTime;

use crate::catalog::Catalog;
use crate::import::exif::extract_exif;
use crate::import::xmp::{find_sidecar, parse_xmp_file, sidecar_dest_for};
use crate::import::ImportAction;
use crate::task::{GroupId, Task, TaskContext, TaskManager};

/// Attach a group to a task only if `parent_group` is `Some`. Lets the
/// caller stay agnostic of whether the import is nested.
fn attach_group(mut t: Task, g: Option<GroupId>) -> Task {
    if let Some(g) = g {
        t = t.group(g);
    }
    t
}

/// Summary of an [`import_batch`] run. Returned via a channel when the
/// batch finishes; the app reads it to display a final dialog.
#[derive(Debug, Default, Clone)]
pub struct ImportSummary {
    pub imported: usize,
    pub skipped_duplicates: usize,
    pub errors: usize,
    pub total: usize,
    /// Sample of the first few error messages for display in a dialog.
    pub sample_errors: Vec<String>,
}

impl ImportSummary {
    pub fn is_success(&self) -> bool {
        self.errors == 0
    }
}

/// One file passed to [`import_batch`].
#[derive(Debug, Clone)]
pub struct ImportFile {
    pub path: PathBuf,
}

/// Spawn a batch of import tasks under a single group. The function
/// returns once every task has been queued. Progress is reported through
/// the task system; the final summary is delivered on `summary_rx` once
/// the last task finishes.
///
/// `parent_group`, when `Some`, makes the import a sub-group of an
/// existing group (e.g. an outer "Import batch" shown in the task panel).
pub fn import_batch(
    mgr: &mut TaskManager,
    catalog: Arc<Catalog>,
    files: Vec<ImportFile>,
    label: impl Into<String>,
    parent_group: Option<GroupId>,
    action: ImportAction,
    collection_path: Option<PathBuf>,
) -> Receiver<ImportSummary> {
    let _ = label; // reserved for the future "Import batch #N" naming.
    let (summary_tx, summary_rx) = channel::<ImportSummary>();
    let (summary_acc_tx, summary_acc_rx) = channel::<ImportUpdate>();

    let total = files.len().max(1);

    // Spinner task: aggregates per-file results and emits the final
    // summary. Keeps the public API simple -- callers get exactly one
    // summary on `summary_rx` per batch.
    let spinner_id = mgr.add_task(
        attach_group(
            Task::new("Finalize import", "Aggregate per-file results"),
            parent_group,
        )
        .work(move |ctx: &TaskContext| {
            let mut acc = ImportSummary::default();
            let total = total;
            while let Ok(update) = summary_acc_rx.recv() {
                acc.total += 1;
                match update {
                    ImportUpdate::Imported => acc.imported += 1,
                    ImportUpdate::Duplicate => acc.skipped_duplicates += 1,
                    ImportUpdate::Failed(msg) => {
                        acc.errors += 1;
                        if acc.sample_errors.len() < 5 {
                            acc.sample_errors.push(msg);
                        }
                    }
                }
                ctx.set_progress(acc.total as f32 / total as f32);
            }
            let _ = summary_tx.send(acc);
            Ok(())
        }),
    );
    let _ = spinner_id; // held by the manager

    // Per-file tasks.
    for f in files {
        let path = f.path.clone();
        let catalog = catalog.clone();
        let summary_acc = summary_acc_tx.clone();
        let collection_path = collection_path.clone();

        let task = attach_group(
            Task::new(
                format!(
                    "Import {}",
                    path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                ),
                "Hash + EXIF + XMP + upsert",
            ),
            parent_group,
        )
        .work(move |ctx: &TaskContext| {
            ctx.set_message("hashing");
            let result = process_one(
                &path,
                &catalog,
                ctx,
                action,
                collection_path.as_deref(),
            );
            let update = match &result {
                Ok(ImportOutcome::Imported) => ImportUpdate::Imported,
                Ok(ImportOutcome::Duplicate) => ImportUpdate::Duplicate,
                Err(e) => ImportUpdate::Failed(format!("{}: {}", path.display(), e)),
            };
            let _ = summary_acc.send(update);
            match result {
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        });
        mgr.add_task(task);
    }

    // Drop the local clone so the spinner sees channel close.
    drop(summary_acc_tx);

    mgr.start();
    summary_rx
}

enum ImportUpdate {
    Imported,
    Duplicate,
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
enum ImportOutcome {
    Imported,
    Duplicate,
}

#[derive(Debug, thiserror::Error)]
enum ImportErr {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("hash: {0}")]
    #[allow(dead_code)]
    Hash(String),
    #[error("catalog: {0}")]
    Catalog(#[from] crate::catalog::CatalogError),
    #[error("exif: {0}")]
    #[allow(dead_code)]
    Exif(String),
}

fn process_one(
    path: &Path,
    catalog: &Catalog,
    ctx: &TaskContext,
    action: ImportAction,
    collection_path: Option<&Path>,
) -> Result<ImportOutcome, ImportErr> {
    if ctx.is_cancelled() {
        return Err(ImportErr::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "cancelled",
        )));
    }

    // Cheap pre-check: path already in the catalog? This is a read,
    // so we don't take the write lock for it.
    let path_str = path.to_string_lossy().into_owned();
    if let Some(existing) = catalog.find_photo_by_path(&path_str)?
        && existing.sha1.is_some()
    {
        // Same path AND has a hash recorded: assume duplicate. Saves a
        // full re-hash when re-running an import.
        ctx.set_progress(1.0);
        ctx.set_message("already in catalog");
        return Ok(ImportOutcome::Duplicate);
    }

    // Build a fresh PhotoInsert from the path and a freshly-read EXIF
    // block, then hash the file. All of this is local (filesystem +
    // EXIF / XMP parsing), no SQLite involved.
    let mut insert = Catalog::photo_insert_from_path(path)?;

    ctx.set_message("reading EXIF");
    ctx.set_progress(0.05);
    if let Ok(exif) = extract_exif(path) {
        exif.apply_to(&mut insert);
    }
    // EXIF parse errors are non-fatal; we still keep the path / size.

    // XMP sidecar (rating, keywords, label, develop, …). Non-fatal if missing/broken.
    let source_sidecar = find_sidecar(path);
    let mut xmp_develop = None;
    if let Some(ref side) = source_sidecar {
        ctx.set_message("reading XMP");
        ctx.set_progress(0.08);
        if let Ok(xmp) = parse_xmp_file(side) {
            xmp.apply_to(&mut insert);
            xmp_develop = xmp.develop;
        }
    }

    // Hash from the SOURCE (single full-file read). The SHA1 is
    // content-based so it's valid regardless of where the file sits.
    ctx.set_message("hashing");
    ctx.set_progress(0.15);
    let sha1 = sha1_file_with_progress(path, |frac| {
        ctx.set_progress(0.15 + frac * 0.45);
    })?;
    insert.sha1 = Some(sha1.to_vec());

    // Compute destination and copy/move the file before taking the
    // write lock. This keeps the expensive I/O outside the critical
    // section so other import tasks aren't blocked.
    let work_path: PathBuf;
    if let Some(coll) = collection_path {
        let dest = destination_path(path, coll, insert.date_taken);
        if dest != path {
            ctx.set_message(match action {
                ImportAction::Copy => "copying to collection",
                ImportAction::Move => "moving to collection",
            });
            ctx.set_progress(0.65);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match action {
                ImportAction::Copy => {
                    std::fs::copy(path, &dest)?;
                }
                ImportAction::Move => {
                    if std::fs::rename(path, &dest).is_err() {
                        std::fs::copy(path, &dest)?;
                        std::fs::remove_file(path)?;
                    }
                }
            }
            // Keep the XMP sidecar next to the image.
            if let Some(ref side) = source_sidecar {
                let dest_side = sidecar_dest_for(path, side, &dest);
                let _ = match action {
                    ImportAction::Copy => std::fs::copy(side, &dest_side).map(|_| ()),
                    ImportAction::Move => {
                        if std::fs::rename(side, &dest_side).is_err() {
                            std::fs::copy(side, &dest_side).ok();
                            let _ = std::fs::remove_file(side);
                        }
                        Ok(())
                    }
                };
            }
            insert.path = dest.to_string_lossy().into_owned();
            if let Ok(meta) = std::fs::metadata(&dest) {
                insert.file_size = Some(meta.len() as i64);
                insert.mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
            }
            work_path = dest;
        } else {
            work_path = path.to_path_buf();
        }
    } else {
        work_path = path.to_path_buf();
    }

    // "Check then write" — hold the catalog's write lock for the
    // whole sequence so two background tasks can't both decide to
    // write the same row at the same time. Since the hash was
    // computed from the source, a concurrent task for the same
    // content would have the same hash and this check serialises
    // the insert under the lock.
    let _write_guard = catalog.write_lock();
    ctx.set_message("checking duplicates");
    ctx.set_progress(0.9);
    if let Some(other) = find_by_sha1(catalog, &sha1)?
        && other.path != insert.path
    {
        // Another task already wrote this content while we were
        // doing the copy. Clean up the duplicate file.
        let _ = std::fs::remove_file(&work_path);
        ctx.set_message("duplicate content");
        return Ok(ImportOutcome::Duplicate);
    }

    ctx.set_message("ensuring folder");
    ctx.set_progress(0.95);
    if let Some(parent) = work_path.parent() {
        let fid = catalog.ensure_folder(parent)?;
        insert.folder_id = Some(fid);
    }

    ctx.set_progress(0.97);
    let photo_id = catalog.upsert_photo(&insert)?;
    if let Some(ref dev) = xmp_develop
        && !dev.is_identity()
    {
        let _ = catalog.set_develop(photo_id, dev);
    }
    ctx.set_progress(1.0);
    Ok(ImportOutcome::Imported)
}

/// Compute the destination path inside `collection_dir` using a
/// `<YYYY>/<MM>/<DD>/<filename>` layout.
///
/// Date priority:
/// 1. `date_taken` (from EXIF)
/// 2. File creation (birth) time
/// 3. Current time (import moment)
///
/// If the computed destination already exists on disk, a counter suffix
/// is appended before the extension to avoid collisions.
fn destination_path(
    source: &Path,
    collection_dir: &Path,
    date_taken: Option<i64>,
) -> PathBuf {
    let ts = date_taken.unwrap_or_else(|| {
        // Fallback: file creation / birth time.
        let meta = std::fs::metadata(source).ok();
        meta.and_then(|m| m.created().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            // Final fallback: current time.
            .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp())
    });
    let dt = OffsetDateTime::from_unix_timestamp(ts).unwrap_or_else(|_| OffsetDateTime::now_utc());
    let year = dt.year();
    let month: u8 = dt.month().into();
    let day = dt.day();
    let filename = source
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("unknown"));

    let base = collection_dir
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(format!("{day:02}"))
        .join(filename);

    resolve_conflict(&base)
}

/// If `path` already exists on disk, append `_1`, `_2`, … before the
/// extension until a free name is found.
fn resolve_conflict(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..=999 {
        let candidate = parent.join(format!("{stem}_{i}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Last resort: append a timestamp.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!("{stem}_{stamp}{ext}"))
}

/// SHA-1 of a file, with a progress callback that fires roughly every
/// 4 MiB of input.
fn sha1_file_with_progress(
    path: &Path,
    mut on_progress: impl FnMut(f32),
) -> Result<[u8; 20], ImportErr> {
    let file = std::fs::File::open(path)?;
    let total = file.metadata()?.len() as f64;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    let mut read: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
        if total > 0.0 {
            on_progress((read as f64 / total) as f32);
        }
        if read.is_multiple_of(4 * 1024 * 1024) {
            // Yield occasionally so the OS can breathe on big files.
            std::thread::yield_now();
        }
    }
    let out = hasher.finalize();
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&out);
    Ok(arr)
}

fn find_by_sha1(
    catalog: &Catalog,
    sha1: &[u8],
) -> Result<Option<crate::catalog::Photo>, ImportErr> {
    let conn = catalog.pool().get().map_err(catalog_pool_err)?;
    let mut stmt = conn
        .prepare("SELECT * FROM photos WHERE sha1 = ?1 LIMIT 1")
        .map_err(catalog_sqlite_err)?;
    let mut rows = stmt.query([sha1]).map_err(catalog_sqlite_err)?;
    let Some(row) = rows.next().map_err(catalog_sqlite_err)? else {
        return Ok(None);
    };
    let photo = crate::catalog::Photo::from_row(row).map_err(catalog_sqlite_err)?;
    Ok(Some(photo))
}

fn catalog_pool_err(e: r2d2::Error) -> ImportErr {
    ImportErr::Catalog(crate::catalog::CatalogError::Pool(e))
}

fn catalog_sqlite_err(e: rusqlite::Error) -> ImportErr {
    ImportErr::Catalog(crate::catalog::CatalogError::Sqlite(e))
}

/// Placeholder so the file's structure stays symmetrical.
#[allow(dead_code)]
fn _noop() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_jpeg(p: &Path) {
        // Minimal 1x1 JPEG: solid red.
        let bytes: &[u8] = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
            0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
            0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
            0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
            0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02,
            0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10,
            0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00,
            0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
            0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42,
            0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16,
            0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55,
            0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73,
            0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
            0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
            0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6,
            0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
            0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08,
            0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFB, 0xD0, 0xFF, 0xD9,
        ];
        std::fs::File::create(p).unwrap().write_all(bytes).unwrap();
    }

    #[test]
    fn sha1_of_known_content() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"hello world").unwrap();
        let h = sha1_file_with_progress(&p, |_| {}).unwrap();
        // sha1("hello world") = 2aae6c35c94fcfb415dbe95f408b9ce91ee846ed
        let expected = hex::decode("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").unwrap();
        assert_eq!(h.to_vec(), expected);
    }

    #[test]
    fn import_a_jpeg_writes_a_photo_row() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let photo = src.join("a.jpg");
        write_jpeg(&photo);

        let cat_dir = tempdir().unwrap();
        let cat_path = cat_dir.path().join("cat.sqlite");
        let cat = Arc::new(Catalog::create(&cat_path).unwrap());

        use crate::import::ImportAction;
        let mut mgr = TaskManager::new();
        let summary_rx = import_batch(
            &mut mgr,
            cat.clone(),
            vec![ImportFile { path: photo }],
            "Test import",
            None,
            ImportAction::Copy,
            Some(cat.dir().to_path_buf()),
        );

        // Drain until the summary arrives.
        let mut summary = ImportSummary::default();
        for _ in 0..1000 {
            mgr.sync();
            if let Ok(s) = summary_rx.try_recv() {
                summary = s;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(summary.imported, 1, "summary: {summary:?}");
        assert_eq!(summary.errors, 0, "summary: {summary:?}");
        assert_eq!(cat.counts().unwrap().photos, 1);
    }

    #[test]
    fn import_applies_xmp_sidecar_and_copies_it() {
        use crate::import::xmp::{serialize_xmp, XmpData};

        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let photo = src.join("shot.jpg");
        write_jpeg(&photo);

        let xmp = XmpData {
            rating: Some(4),
            label: Some("Blue".into()),
            keywords: vec!["beach".into(), "ocean".into()],
            copyright: Some("© Test".into()),
            ..Default::default()
        };
        // Adobe-style sidecar next to the image.
        std::fs::write(src.join("shot.jpg.xmp"), serialize_xmp(&xmp)).unwrap();

        let cat_dir = tempdir().unwrap();
        let cat = Arc::new(Catalog::create(&cat_dir.path().join("cat.sqlite")).unwrap());

        let mut mgr = TaskManager::new();
        let summary_rx = import_batch(
            &mut mgr,
            cat.clone(),
            vec![ImportFile { path: photo }],
            "XMP import",
            None,
            ImportAction::Copy,
            Some(cat.dir().to_path_buf()),
        );

        let mut summary = ImportSummary::default();
        for _ in 0..1000 {
            mgr.sync();
            if let Ok(s) = summary_rx.try_recv() {
                summary = s;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(summary.imported, 1, "summary: {summary:?}");
        assert_eq!(summary.errors, 0, "summary: {summary:?}");

        let photos = cat.list_photos(None).unwrap();
        assert_eq!(photos.len(), 1);
        let p = &photos[0];
        assert_eq!(p.rating, 4);
        assert_eq!(p.color_label, 4); // Blue
        assert_eq!(p.copyright.as_deref(), Some("© Test"));
        let kws = cat.photo_keywords(p.id).unwrap();
        assert_eq!(kws, vec!["beach", "ocean"]);

        // Sidecar copied next to the destination image.
        let dest_side = PathBuf::from(format!("{}.xmp", p.path));
        assert!(
            dest_side.is_file(),
            "expected sidecar at {}",
            dest_side.display()
        );
    }

    #[test]
    fn import_loads_develop_settings_from_xmp() {
        use crate::develop::DevelopSettings;
        use crate::import::xmp::{serialize_xmp, XmpData};

        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let photo = src.join("dev.jpg");
        write_jpeg(&photo);

        let xmp = XmpData {
            develop: Some(DevelopSettings {
                exposure: 1.25,
                contrast: 30.0,
                shadows: 10.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        std::fs::write(src.join("dev.jpg.xmp"), serialize_xmp(&xmp)).unwrap();

        let cat_dir = tempdir().unwrap();
        let cat = Arc::new(Catalog::create(&cat_dir.path().join("cat.sqlite")).unwrap());

        let mut mgr = TaskManager::new();
        let summary_rx = import_batch(
            &mut mgr,
            cat.clone(),
            vec![ImportFile { path: photo }],
            "Develop import",
            None,
            ImportAction::Copy,
            Some(cat.dir().to_path_buf()),
        );

        let mut summary = ImportSummary::default();
        for _ in 0..1000 {
            mgr.sync();
            if let Ok(s) = summary_rx.try_recv() {
                summary = s;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(summary.imported, 1, "summary: {summary:?}");

        let p = &cat.list_photos(None).unwrap()[0];
        let dev = cat.get_develop(p.id).unwrap();
        assert!((dev.exposure - 1.25).abs() < 0.01);
        assert!((dev.contrast - 30.0).abs() < 0.01);
        assert!((dev.shadows - 10.0).abs() < 0.01);
    }
}
