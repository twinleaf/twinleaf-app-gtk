#!/usr/bin/env bash
# Build a self-contained Twinleaf.app from the Homebrew GTK4 stack.
#
# Requires: a Rust toolchain and `brew install gtk4 libadwaita dylibbundler`.
# Produces build/Twinleaf.app with the GTK4 dylibs, GdkPixbuf loaders (incl.
# SVG), GIO TLS modules (HTTPS firmware catalog), compiled GSettings schemas,
# and Adwaita icons bundled. A launcher wires the env so the app is relocatable.
#
# Code signing + notarization are handled by the release workflow; to sign a
# local build, set CODESIGN_IDENTITY and this script will sign at the end.
set -euo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"

APP="build/Twinleaf.app"
BIN="target/release/twinleaf-gtk"
RES="$APP/Contents/Resources"
LIB="$RES/lib"
BREW="$(brew --prefix)"
PB="lib/gdk-pixbuf-2.0/2.10.0"

# dylibbundler search paths. Without these it can't resolve @rpath deps (e.g.
# librsvg-2.2.dylib, pulled in by the SVG pixbuf loader) and stops to prompt on
# stderr — with no stdin in CI that prompt loops forever ("…does not exist. Try
# again"). Giving it the Homebrew lib dirs means it never has to ask.
DB_SEARCH=(-s "$BREW/lib" -s "$BREW/opt/librsvg/lib")

cargo build --release --locked -p twinleaf-gtk

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$LIB"
cp "$BIN" "$APP/Contents/MacOS/twinleaf-gtk-bin"

# Bundle icon: compose the .icns from the shared PNG set (the real app icon,
# imported from the twinleaf-app repo). The app still launches without it.
ICON=""
if command -v iconutil >/dev/null; then
  ICONSET="build/Twinleaf.iconset"; rm -rf "$ICONSET"; mkdir -p "$ICONSET"
  PNGS=packaging/linux/icons/hicolor
  for s in 16 32 128 256 512; do
    cp "$PNGS/${s}x${s}/apps/com.twinleaf.AppGtk.png" "$ICONSET/icon_${s}x${s}.png"
    cp "$PNGS/$((s*2))x$((s*2))/apps/com.twinleaf.AppGtk.png" "$ICONSET/icon_${s}x${s}@2x.png"
  done
  iconutil -c icns "$ICONSET" -o "$RES/Twinleaf.icns" && ICON=Twinleaf.icns || ICON=""
fi

cat > "$APP/Contents/Info.plist" <<PLIST
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
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
$( [ -n "$ICON" ] && printf '  <key>CFBundleIconFile</key><string>Twinleaf.icns</string>\n' )
</dict>
</plist>
PLIST

# Launcher: point GTK at the bundled runtime, then exec the real binary.
cat > "$APP/Contents/MacOS/twinleaf-gtk" <<'SH'
#!/bin/bash
here="$(cd "$(dirname "$0")" && pwd)"
res="$here/../Resources"
export GDK_PIXBUF_MODULE_FILE="$res/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
export GIO_MODULE_DIR="$res/lib/gio/modules"
export GSETTINGS_SCHEMA_DIR="$res/share/glib-2.0/schemas"
export XDG_DATA_DIRS="$res/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
exec "$here/twinleaf-gtk-bin" "$@"
SH
chmod +x "$APP/Contents/MacOS/twinleaf-gtk"

# Relocate the binary's dylibs and rewrite their install names.
dylibbundler -od -b "${DB_SEARCH[@]}" -x "$APP/Contents/MacOS/twinleaf-gtk-bin" \
  -d "$LIB" -p "@executable_path/../Resources/lib"

# GdkPixbuf loaders (incl. SVG); fix each loader's deps, then rebuild the cache
# with paths relative to the cache file's own directory.
mkdir -p "$RES/$PB/loaders"
cp "$BREW/$PB/loaders/"*.so "$RES/$PB/loaders/"
for so in "$RES/$PB/loaders/"*.so; do
  dylibbundler -of -b "${DB_SEARCH[@]}" -x "$so" -d "$LIB" -p "@loader_path/../../../"
done
( cd "$RES/$PB" && GDK_PIXBUF_MODULEDIR=loaders gdk-pixbuf-query-loaders > loaders.cache )

# GIO modules (TLS, needed for the HTTPS firmware catalog).
mkdir -p "$LIB/gio/modules"
cp "$BREW/lib/gio/modules/"*.so "$LIB/gio/modules/" 2>/dev/null || true
for so in "$LIB/gio/modules/"*.so; do
  [ -f "$so" ] && dylibbundler -of -b "${DB_SEARCH[@]}" -x "$so" -d "$LIB" -p "@loader_path/../../"
done

# Compiled GSettings schemas.
mkdir -p "$RES/share/glib-2.0/schemas"
cp "$BREW/share/glib-2.0/schemas/"*.xml "$RES/share/glib-2.0/schemas/" 2>/dev/null || true
glib-compile-schemas "$RES/share/glib-2.0/schemas" >/dev/null 2>&1 || true

# Adwaita icons for stock icons not embedded in our GResource.
mkdir -p "$RES/share/icons"
cp -R "$BREW/share/icons/Adwaita" "$RES/share/icons/" 2>/dev/null || true

# Optional local signing (CI does this with the Developer ID secrets instead).
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
  find "$LIB" -type f \( -name '*.dylib' -o -name '*.so' \) \
    -exec codesign --force --options runtime --timestamp -s "$CODESIGN_IDENTITY" {} +
  codesign --force --options runtime --timestamp \
    --entitlements packaging/macos/entitlements.plist \
    -s "$CODESIGN_IDENTITY" "$APP/Contents/MacOS/twinleaf-gtk-bin"
  codesign --force --options runtime --timestamp -s "$CODESIGN_IDENTITY" "$APP/Contents/MacOS/twinleaf-gtk"
  codesign --force --options runtime --timestamp \
    --entitlements packaging/macos/entitlements.plist -s "$CODESIGN_IDENTITY" "$APP"
fi

echo "Built $APP"
