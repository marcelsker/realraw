#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

BIN_NAME="realraw"
ACTION="${1:-help}"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)"
VERSION="${VERSION:-0.1.0}"

usage() {
    cat <<EOF
Usage: ./scripts/package.sh <command>

Commands:
  app-macos        Build .app bundle for macOS (requires cargo-bundle)
  dmg              Build .app then wrap in .dmg (requires create-dmg)
  deb              Build .deb package for Debian/Ubuntu (requires cargo-deb)
  appimage         Build AppImage for Linux
  exe              Build Windows .exe (icon embedded automatically via build.rs)
  nsis             Build Windows NSIS installer (.exe setup)
  wix              Build Windows WiX MSI installer
  all              Run all available commands for the current OS
  help             Show this help
EOF
}

require_cmd() {
    if ! command -v "$1" &>/dev/null; then
        echo "error: '$1' is required but not installed."
        if [ -n "${2:-}" ]; then
            echo "install with: $2"
        else
            echo "install with: cargo install $1"
        fi
        exit 1
    fi
}

release_bin() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) echo "target/release/${BIN_NAME}.exe" ;;
        *) echo "target/release/${BIN_NAME}" ;;
    esac
}

# Build release binary only if missing (CI should pre-build via cargo test --release).
ensure_release_bin() {
    local bin
    bin="$(release_bin)"
    if [ -f "$bin" ]; then
        echo "==> Using existing $bin"
        return 0
    fi
    echo "==> Building release binary..."
    cargo build --release
}

# Resolve a Windows tool that may live under Program Files (Git Bash PATH often incomplete).
find_win_cmd() {
    local name="$1"
    if command -v "$name" &>/dev/null; then
        command -v "$name"
        return 0
    fi
    local p
    local home="${USERPROFILE:-$HOME}"
    # Convert Windows path to Git Bash style if needed
    home="${home//\\//}"
    case "$home" in
        [A-Za-z]:*) home="/${home:0:1}${home:2}" ;;
    esac

    # CI portable tools dir
    local repo_root
    repo_root="$(pwd)"
    for p in \
        "$repo_root/tools/nsis-3.10/${name}.exe" \
        "$repo_root/tools/nsis/${name}.exe" \
        "$repo_root/tools/wix/${name}.exe" \
        "$home/scoop/shims/${name}.exe" \
        "$home/scoop/apps/nsis/current/${name}.exe" \
        "$home/scoop/apps/wixtoolset/current/bin/${name}.exe" \
        "$home/.dotnet/tools/${name}.exe" \
        "/c/Program Files (x86)/NSIS/${name}.exe" \
        "/c/Program Files/NSIS/${name}.exe" \
        "/c/Program Files/WiX/${name}.exe" \
        "/c/Program Files (x86)/WiX/${name}.exe" \
        ; do
        if [ -x "$p" ] || [ -f "$p" ]; then
            echo "$p"
            return 0
        fi
    done
    # Glob WiX Toolset versions (MSI install) and Scoop versioned dirs
    local match
    match="$(ls -d "/c/Program Files (x86)/WiX Toolset v"*"/bin/${name}.exe" 2>/dev/null | head -n1 || true)"
    if [ -n "$match" ] && [ -f "$match" ]; then
        echo "$match"
        return 0
    fi
    match="$(ls -d "$home/scoop/apps/wixtoolset/"*"/bin/${name}.exe" 2>/dev/null | head -n1 || true)"
    if [ -n "$match" ] && [ -f "$match" ]; then
        echo "$match"
        return 0
    fi
    match="$(ls -d "$repo_root/tools/nsis-"*"/${name}.exe" 2>/dev/null | head -n1 || true)"
    if [ -n "$match" ] && [ -f "$match" ]; then
        echo "$match"
        return 0
    fi
    return 1
}

cmd_app_macos() {
    require_cmd cargo-bundle
    ensure_release_bin
    echo "==> Building .app bundle..."
    # cargo-bundle invokes cargo; after a prior release build this is incremental.
    cargo bundle --release
    echo "==> Done: target/release/bundle/osx/$BIN_NAME.app"
}

cmd_dmg() {
    require_cmd cargo-bundle
    require_cmd create-dmg
    cmd_app_macos
    local app_path="target/release/bundle/osx/$BIN_NAME.app"
    local dmg_path="target/release/$BIN_NAME.dmg"
    echo "==> Building .dmg..."
    create-dmg \
        --volname "$BIN_NAME" \
        --window-pos 200 120 \
        --window-size 800 400 \
        --icon-size 100 \
        --app-drop-link 600 185 \
        --icon "$BIN_NAME.app" 200 185 \
        "$dmg_path" \
        "$app_path"
    echo "==> Done: $dmg_path"
}

cmd_deb() {
    require_cmd cargo-deb
    ensure_release_bin
    echo "==> Building .deb package..."
    cargo deb --no-build
    echo "==> Done: target/debian/${BIN_NAME}_*.deb"
}

cmd_appimage() {
    echo "==> Building AppImage..."

    local appdir="target/AppDir"
    local binary="target/release/$BIN_NAME"
    local appimagetool="/tmp/appimagetool"

    ensure_release_bin

    # Create AppDir structure
    rm -rf "$appdir"
    mkdir -p "$appdir/usr/bin"
    mkdir -p "$appdir/usr/share/applications"
    mkdir -p "$appdir/usr/share/icons/hicolor/64x64/apps"

    cp "$binary" "$appdir/usr/bin/"
    cp assets/realraw.desktop "$appdir/usr/share/applications/"
    cp assets/icon-64.png "$appdir/usr/share/icons/hicolor/64x64/apps/realraw.png"

    # AppImage discovery: AppRun + top-level symlinks
    cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/bash
exec "$(dirname "$0")/usr/bin/realraw"
APPRUN
    chmod +x "$appdir/AppRun"
    ln -s "usr/share/applications/$BIN_NAME.desktop" "$appdir/$BIN_NAME.desktop" 2>/dev/null || true
    ln -s "usr/share/icons/hicolor/64x64/apps/realraw.png" "$appdir/realraw.png" 2>/dev/null || true
    ln -s "usr/share/icons/hicolor/64x64/apps/realraw.png" "$appdir/.DirIcon" 2>/dev/null || true

    # Download appimagetool if not cached
    if [ ! -f "$appimagetool" ]; then
        echo "Downloading appimagetool..."
        wget -q "https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage" -O "$appimagetool"
        chmod +x "$appimagetool"
    fi

    # Build the AppImage
    ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" "$appdir" "target/release/${BIN_NAME}-x86_64.AppImage"

    echo "==> Done: target/release/${BIN_NAME}-x86_64.AppImage"
}

cmd_exe() {
    ensure_release_bin
    echo "==> Done: $(release_bin)"
}

cmd_nsis() {
    local makensis
    if ! makensis="$(find_win_cmd makensis)"; then
        echo "error: 'makensis' is required but not installed."
        echo "install with:"
        echo "  scoop bucket add extras"
        echo "  scoop install wixtoolset nsis"
        exit 1
    fi

    ensure_release_bin

    local project_root="$PWD"
    # Convert to Windows native path when running under Git Bash/MSYS2
    if command -v cygpath &>/dev/null; then
        project_root="$(cygpath -w "$PWD")"
    fi

    echo "==> Building NSIS installer (v${VERSION})..."
    "$makensis" \
        -DVERSION="$VERSION" \
        -DPROJECT_ROOT="$project_root" \
        packaging/windows/realraw.nsi
    echo "==> Done: target/release/${BIN_NAME}-${VERSION}-setup.exe"
}

cmd_wix() {
    local wix
    if ! wix="$(find_win_cmd wix)"; then
        echo "error: 'wix' (WiX Toolset v4+) is required but not installed."
        echo "install with:"
        echo "  dotnet tool install --global wix"
        echo "  or download from https://github.com/wixtoolset/wix/releases"
        exit 1
    fi

    ensure_release_bin

    local msi="target/release/${BIN_NAME}-${VERSION}-x64.msi"

    echo "==> Building WiX MSI (v${VERSION})..."
    "$wix" build -nologo -arch x64 -d ProductVersion="${VERSION}" -out "$msi" packaging/windows/realraw.wxs
    echo "==> Done: $msi"
}

cmd_all() {
    case "$(uname -s)" in
        Darwin)
            cmd_app_macos
            if command -v create-dmg &>/dev/null; then
                cmd_dmg
            fi
            ;;
        Linux)
            cmd_deb
            cmd_appimage
            ;;
        MINGW*|MSYS*|CYGWIN*)
            cmd_exe
            if find_win_cmd makensis &>/dev/null; then
                cmd_nsis
            else
                echo "==> Skipping NSIS (makensis not found)"
            fi
            if find_win_cmd wix &>/dev/null; then
                cmd_wix
            else
                echo "==> Skipping WiX (wix not found)"
            fi
            ;;
        *)
            echo "unknown OS: $(uname -s)"
            exit 1
            ;;
    esac
}

case "$ACTION" in
    app-macos)  cmd_app_macos ;;
    dmg)        cmd_dmg ;;
    deb)        cmd_deb ;;
    appimage)   cmd_appimage ;;
    exe)        cmd_exe ;;
    nsis)       cmd_nsis ;;
    wix)        cmd_wix ;;
    all)        cmd_all ;;
    help|--help|-h) usage ;;
    *)          echo "unknown command: $ACTION"; usage; exit 1 ;;
esac
