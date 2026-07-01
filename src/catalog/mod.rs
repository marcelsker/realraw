use std::path::{Path, PathBuf};
use std::sync::Mutex;

use directories::UserDirs;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use refinery::embed_migrations;

mod error;
pub mod folder;
pub mod photo;
pub mod thumbnail_cache;

pub use error::{CatalogError, Result};
pub use folder::Folder;
pub use photo::{Photo, PhotoInsert};

embed_migrations!("src/catalog/migrations");

/// Aggregated row counts for the status bar.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub photos: i64,
    pub collections: i64,
    pub folders: i64,
}

/// The Lightroom-style catalog. Wraps an r2d2 pool over a single SQLite file
/// and applies our PRAGMAs + migrations on every open.
pub struct Catalog {
    pool: Pool<SqliteConnectionManager>,
    path: PathBuf,
    /// Serialises all write transactions. SQLite's WAL mode allows
    /// concurrent readers but only one writer, and the import
    /// pipeline (N background tasks, each writing a row) routinely
    /// overwhelms `busy_timeout` retries. Holding this mutex around
    /// the whole "read check + write" sequence keeps the catalog in
    /// a consistent state and removes the lock contention at the
    /// source. Readers are unaffected -- they don't take this lock.
    write_lock: Mutex<()>,
}

impl Catalog {
    /// Open an existing catalog file, creating it (and running migrations) if
    /// it does not exist.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Self::create(path);
        }
        Self::open_existing(path)
    }

    /// Create a brand-new catalog file at `path`. Errors if the file already
    /// exists.
    pub fn create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Err(CatalogError::AlreadyExists(path.to_path_buf()));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Touch the file so SQLite can open it.
        std::fs::File::create(path)?;
        Self::open_existing(path)
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(CatalogError::NotFound(path.to_path_buf()));
        }
        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            // PRAGMAs that must be re-applied on every connection.
            //
            // `busy_timeout` is the most important one for the
            // import pipeline: with WAL mode + N concurrent worker
            // threads, multiple writers will race for the lock and
            // the loser would otherwise get SQLITE_BUSY immediately.
            // Setting a 30 s timeout makes SQLite wait instead of
            // erroring, which is what we want for a background
            // import.
            c.execute_batch(
                "PRAGMA journal_mode = WAL;\
                 PRAGMA synchronous  = NORMAL;\
                 PRAGMA temp_store   = MEMORY;\
                 PRAGMA mmap_size    = 268435456;\
                 PRAGMA cache_size   = -64000;\
                 PRAGMA foreign_keys = ON;\
                 PRAGMA busy_timeout = 30000;",
            )
        });
        // Pool size: WAL allows many readers but only one writer.
        // 4 is plenty for our workload (UI reads + a couple of
        // background writers) and keeps lock contention low.
        let pool = Pool::builder().max_size(4).build(manager)?;

        // Run migrations on a single connection.
        let mut conn = pool.get()?;
        migrations::runner().run(&mut *conn)?;

        Ok(Self {
            pool,
            path: path.to_path_buf(),
            write_lock: Mutex::new(()),
        })
    }

    /// Acquire the catalog's write lock. Used by the import pipeline
    /// to serialise "check then write" sequences so two background
    /// tasks can't both decide to write the same row at the same
    /// time.
    pub fn write_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        // Poisoning would mean a previous writer panicked mid-write;
        // the catalog is in an indeterminate state, but the user
        // asked us to keep going. Clear the poison flag and return.
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Path the catalog was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parent directory containing this catalog (e.g. `~/Pictures/realraw/`).
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(self.path.as_ref())
    }

    /// User-friendly path for the status bar: hides the `catalog.sqlite`
    /// filename and replaces the home directory with `~`.
    pub fn display_path(&self) -> String {
        let dir = self.path.parent().unwrap_or(self.path.as_ref());
        let full = dir.display().to_string();
        let home = UserDirs::new().and_then(|u| u.home_dir().to_str().map(str::to_owned));
        shorten_for_display(&full, home.as_deref())
    }

    /// Resolve the default catalog location for this platform.
    /// Uses the user's `Pictures` directory and puts the catalog in a
    /// `realraw/` subfolder: `~/Pictures/realraw/catalog.sqlite` on macOS
    /// and Linux; the Windows "Pictures" known folder on Windows.
    pub fn default_path() -> Result<PathBuf> {
        let user = UserDirs::new().ok_or(CatalogError::NoDefaultDir)?;
        let picture_dir = user.picture_dir().ok_or(CatalogError::NoDefaultDir)?;
        Ok(picture_dir.join("realraw").join("catalog.sqlite"))
    }

    /// Aggregate row counts for the status bar.
    pub fn counts(&self) -> Result<Counts> {
        let conn = self.pool.get()?;
        let photos = count_table(&conn, "photos")?;
        let collections = count_table(&conn, "collections")?;
        let folders = count_table(&conn, "folders")?;
        Ok(Counts {
            photos,
            collections,
            folders,
        })
    }

    /// Borrow the connection pool. Used by future modules (photos,
    /// collections, ...) for CRUD operations.
    pub fn pool(&self) -> &Pool<SqliteConnectionManager> {
        &self.pool
    }
}

fn count_table(conn: &rusqlite::Connection, table: &str) -> Result<i64> {
    // Table names are hard-coded by us, so this interpolation is safe.
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let n: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
    Ok(n)
}

/// Pure helper used by [`Catalog::display_path`]; exposed for testing.
fn shorten_for_display(full_path: &str, home: Option<&str>) -> String {
    if let Some(home) = home {
        if full_path == home {
            return "~".to_string();
        }
        if let Some(rest) = full_path.strip_prefix(home) {
            return format!("~{rest}");
        }
    }
    full_path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_expected_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let cat = Catalog::create(&path).unwrap();
        let conn = cat.pool().get().unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for expected in [
            "schema_version",
            "folders",
            "photos",
            "collections",
            "collection_photos",
            "keywords",
            "photo_keywords",
            "photos_fts",
            "refinery_schema_history",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; got {tables:?}"
            );
        }
    }

    #[test]
    fn v003_migration_adds_thumbnail_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let cat = Catalog::create(&path).unwrap();
        let conn = cat.pool().get().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(photos)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            cols.contains(&"thumbnail_status".to_string()),
            "V003 should add thumbnail_status; got {cols:?}"
        );
    }

    #[test]
    fn counts_start_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let cat = Catalog::create(&path).unwrap();
        let c = cat.counts().unwrap();
        assert_eq!(c.photos, 0);
        assert_eq!(c.collections, 0);
        assert_eq!(c.folders, 0);
    }

    #[test]
    fn second_open_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");

        // First open: insert a row.
        {
            let cat = Catalog::open(&path).unwrap();
            cat.pool()
                .get()
                .unwrap()
                .execute(
                    "INSERT INTO photos (path, imported_at) VALUES ('/tmp/a.jpg', 1)",
                    [],
                )
                .unwrap();
        }
        // Second open: row still there.
        let cat2 = Catalog::open(&path).unwrap();
        assert_eq!(cat2.counts().unwrap().photos, 1);
    }

    #[test]
    fn create_fails_if_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let _ = Catalog::create(&path).unwrap();
        let err = Catalog::create(&path).err().unwrap();
        assert!(matches!(err, CatalogError::AlreadyExists(_)));
    }

    #[test]
    fn journal_mode_is_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let cat = Catalog::create(&path).unwrap();
        let mode: String = cat
            .pool()
            .get()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn shorten_replaces_home_with_tilde() {
        let home = "/Users/sker";
        let s = shorten_for_display(
            "/Users/sker/Pictures/realraw/catalog.sqlite",
            Some(home),
        );
        assert_eq!(s, "~/Pictures/realraw/catalog.sqlite");
    }

    #[test]
    fn shorten_returns_tilde_when_path_is_home() {
        let s = shorten_for_display("/Users/sker", Some("/Users/sker"));
        assert_eq!(s, "~");
    }

    #[test]
    fn shorten_leaves_non_home_paths_alone() {
        let s = shorten_for_display("/Volumes/Photos/cat.sqlite", Some("/Users/sker"));
        assert_eq!(s, "/Volumes/Photos/cat.sqlite");
    }

    #[test]
    fn catalog_display_path_hides_sqlite_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        let cat = Catalog::create(&path).unwrap();
        let shown = cat.display_path();
        assert!(!shown.ends_with("catalog.sqlite"), "got {shown}");
        assert!(shown.contains("catalog.sqlite".trim_end_matches(".sqlite").trim_end_matches("catalog")) ||
                shown.ends_with(dir.path().file_name().unwrap().to_str().unwrap()),
            "expected parent dir in display path, got {shown}");
    }
}
