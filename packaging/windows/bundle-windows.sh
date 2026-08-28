#!/usr/bin/env bash
# Collect a portable GTK4 runtime around twinleaf-gtk.exe.
#
# Run inside an MSYS2 UCRT64 shell after `cargo build --release`. Produces
# dist/twinleaf-gtk/ laid out like an MSYS2 prefix (exe + DLLs at the root,
# with lib/ and share/ siblings) so GTK4's relocatable lookup finds its
# loaders, schemas, and icons relative to the .exe with no env vars.
set -euo pipefail

cd "${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"

PREFIX="${MSYSTEM_PREFIX:-/ucrt64}"
OUT="dist/twinleaf-gtk"
EXE="target/release/twinleaf-gtk.exe"
PB="lib/gdk-pixbuf-2.0/2.10.0"

[ -f "$EXE" ] || { echo "missing $EXE — build first" >&2; exit 1; }
rm -rf "$OUT"
mkdir -p "$OUT"
cp "$EXE" "$OUT/"

# Recursively copy the MSYS2-prefix DLLs a binary depends on. Fed via process
# substitution (not a pipe) so the copies run in this shell, and tolerant of
# ldd/grep returning non-zero — a leaf DLL with no prefix deps makes grep exit
# 1, which under `set -o pipefail` + `set -e` would otherwise abort silently.
copy_dll_deps() {
  local bin="$1" dll base
  while read -r dll; do
    [ -n "$dll" ] || continue
    base="$(basename "$dll")"
    [ -e "$OUT/$base" ] && continue
    cp "$dll" "$OUT/"
    copy_dll_deps "$dll"
  done < <(ldd "$bin" 2>/dev/null | awk '{print $3}' | grep -iE "^${PREFIX}/bin/" || true)
}

echo ">> collecting dependent DLLs"
copy_dll_deps "$EXE"
echo ">> $(find "$OUT" -maxdepth 1 -name '*.dll' | wc -l) DLLs copied"

# GdkPixbuf loaders (incl. the SVG loader our symbolic icons need) + their DLLs.
echo ">> gdk-pixbuf loaders"
mkdir -p "$OUT/$PB/loaders"
if compgen -G "$PREFIX/$PB/loaders/*.dll" > /dev/null; then
  cp "$PREFIX/$PB/loaders/"*.dll "$OUT/$PB/loaders/"
  for so in "$OUT/$PB/loaders/"*.dll; do copy_dll_deps "$so"; done
fi
# Cache with loader paths relative to the cache file's own directory.
( cd "$OUT/$PB" && GDK_PIXBUF_MODULEDIR=loaders gdk-pixbuf-query-loaders > loaders.cache )

# Compiled GSettings schemas (libadwaita/GTK read these).
echo ">> gsettings schemas"
mkdir -p "$OUT/share/glib-2.0/schemas"
cp "$PREFIX/share/glib-2.0/schemas/gschemas.compiled" "$OUT/share/glib-2.0/schemas/" 2>/dev/null \
  || glib-compile-schemas "$PREFIX/share/glib-2.0/schemas" --targetdir "$OUT/share/glib-2.0/schemas"

# Icon themes for stock icons not embedded in our GResource.
echo ">> icons"
mkdir -p "$OUT/share/icons"
cp -r "$PREFIX/share/icons/Adwaita" "$OUT/share/icons/" 2>/dev/null || true
cp -r "$PREFIX/share/icons/hicolor" "$OUT/share/icons/" 2>/dev/null || true
rm -rf "$OUT/share/icons/Adwaita/cursors" 2>/dev/null || true
# Our app icon (shared PNG set) so the window/taskbar icon resolves by name.
for s in 16 32 64 128 256 512; do
  mkdir -p "$OUT/share/icons/hicolor/${s}x${s}/apps"
  cp "packaging/linux/icons/hicolor/${s}x${s}/apps/com.twinleaf.AppGtk.png" \
    "$OUT/share/icons/hicolor/${s}x${s}/apps/"
done

echo ">> bundled $OUT"
