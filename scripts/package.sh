#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

BIN_NAME="realraw"
ACTION="${1:-help}"

usage() {
    cat <<EOF
Usage: ./scripts/package.sh <command>

Commands:
  app-macos        Build .app bundle for macOS (requires cargo-bundle)
  dmg              Build .app then wrap in .dmg (requires create-dmg)
  deb              Build .deb package for Debian/Ubuntu (requires cargo-deb)
  rpm              Build .rpm package for Fedora/RHEL (requires cargo-bundle)
  appimage         Build AppImage for Linux (requires cargo-appimage)
  exe              Build Windows .exe (icon embedded automatically via build.rs)
  all              Run all available commands for the current OS
  help             Show this help
EOF
}

require_cmd() {
    if ! command -v "$1" &>/dev/null; then
        echo "error: '$1' is required but not installed."
        echo "install with: cargo install $1"
        exit 1
    fi
}

cmd_app_macos() {
    require_cmd cargo-bundle
    echo "==> Building .app bundle..."
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
    echo "==> Building .deb package..."
    cargo deb -- --release
    echo "==> Done: target/debian/${BIN_NAME}_*.deb"
}

cmd_rpm() {
    require_cmd cargo-bundle
    echo "==> Building .rpm package..."
    cargo bundle --release --format rpm
    echo "==> Done: target/release/bundle/rpm/"
}

cmd_appimage() {
    require_cmd cargo-appimage
    echo "==> Building AppImage..."
    cargo appimage --release
    echo "==> Done: target/release/${BIN_NAME}*.AppImage"
}

cmd_exe() {
    echo "==> Building release exe (icon embedded via build.rs)..."
    cargo build --release
    echo "==> Done: target/release/$BIN_NAME.exe"
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
            if command -v cargo-bundle &>/dev/null; then
                cmd_rpm
            fi
            if command -v cargo-appimage &>/dev/null; then
                cmd_appimage
            fi
            ;;
        MINGW*|MSYS*|CYGWIN*)
            cmd_exe
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
    rpm)        cmd_rpm ;;
    appimage)   cmd_appimage ;;
    exe)        cmd_exe ;;
    all)        cmd_all ;;
    help|--help|-h) usage ;;
    *)          echo "unknown command: $ACTION"; usage; exit 1 ;;
esac
