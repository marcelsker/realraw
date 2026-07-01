# realraw

<p align="center">
  <img src="assets/icon-2048.png" width="200" alt="realraw logo" />
</p>

An open-source Lightroom alternative, written in Rust.

Realraw is a native desktop photo management app with an SQLite-backed catalog,
a multi-stage import pipeline (discovery, EXIF extraction, hashing), and an
egui-based GUI.

## Features

- **SQLite catalog** -- photos, folders, collections, keywords, full-text search
- **Import pipeline** -- file discovery across ~25 raw/image formats, EXIF/IPTC
  metadata extraction, SHA-1 deduplication, embedded thumbnail extraction with
  smart JPEG scanning fallback
- **Background task system** -- concurrent workers with progress reporting,
  dependency tracking, and task-group cancellation
- **Thumbnail grid** -- GPU-cached thumbnails with 3:2 aspect-ratio cards,
  selection state, and lazy loading
- **egui/eframe GUI** -- library view, import dialog with preview, tasks panel,
  menubar and status bar

## Requirements

- Rust toolchain (edition 2024)

No system dependencies beyond what your desktop provides (OpenGL/Metal/Vulkan
are handled by eframe; SQLite is bundled).

## Build & Run

```bash
cargo build --release
cargo run
```

On first launch, a default catalog is created at `~/Pictures/realraw/catalog.sqlite`.

## Tests

```bash
cargo test
```

## Packaging

The binary can be packaged into platform-specific installers via `scripts/package.sh`:

| Command | Requires | Output |
|---------|----------|--------|
| `./scripts/package.sh app-macos` | `cargo install cargo-bundle` | `.app` bundle |
| `./scripts/package.sh dmg` | `cargo-bundle` + `brew install create-dmg` | `.dmg` disk image |
| `./scripts/package.sh deb` | `cargo install cargo-deb` | `.deb` (Debian/Ubuntu) |
| `./scripts/package.sh appimage` | `cargo install cargo-appimage` | AppImage (any Linux) |
| `./scripts/package.sh exe` | nothing extra | `.exe` with icon embedded |
| `./scripts/package.sh all` | all of the above | runs available commands for the current OS |

The Windows `.exe` icon is embedded automatically at compile time via `build.rs`.  
The macOS `.icns` and Windows `.ico` are generated from `assets/icon-2048.png`.

## License

AGPL-3.0-or-later. See [`LICENSE`](LICENSE).
