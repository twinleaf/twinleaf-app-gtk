#!/usr/bin/env bash
# Build a relocatable Twinleaf.app bundling the Homebrew GTK4 dylibs.
# Requires: a Rust toolchain and `brew install gtk4 dylibbundler`.
set -euo pipefail
cd "$(dirname "$0")/../.."

APP="build/Twinleaf.app"
BIN="target/release/twinleaf-gtk"

cargo build --release -p twinleaf-gtk

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/lib"
cp "$BIN" "$APP/Contents/MacOS/twinleaf-gtk"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Twinleaf</string>
  <key>CFBundleDisplayName</key><string>Twinleaf</string>
  <key>CFBundleIdentifier</key><string>com.twinleaf.AppGtk</string>
  <key>CFBundleExecutable</key><string>twinleaf-gtk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Relocate GTK4 + transitive dylibs into the bundle and rewrite install names.
dylibbundler -od -b \
  -x "$APP/Contents/MacOS/twinleaf-gtk" \
  -d "$APP/Contents/Resources/lib" \
  -p "@executable_path/../Resources/lib"

# NOTE: a fully self-contained bundle also needs these copied into
# Contents/Resources and wired via a launcher (GDK_PIXBUF_MODULE_FILE /
# GIO_MODULE_DIR / XDG_DATA_DIRS) — see packaging/README.md:
#   $(brew --prefix)/lib/gdk-pixbuf-2.0      image loaders + loaders.cache
#   $(brew --prefix)/lib/gio/modules         TLS (firmware catalog over HTTPS)
#   $(brew --prefix)/share/glib-2.0/schemas  compiled GSettings schemas
#   $(brew --prefix)/share/icons/Adwaita     symbolic icons

echo "Built $APP"
