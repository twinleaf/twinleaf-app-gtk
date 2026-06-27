# Packaging

Cross-platform distribution for `twinleaf-gtk`. App id: `com.twinleaf.AppGtk`.

These are working starting points; treat the Flatpak/macOS manifests as
templates to validate on a real builder. CI (`.gitlab-ci.yml`) builds + tests on
Linux on every push.

## Linux

Two common paths:

- **Flatpak** (`flatpak/com.twinleaf.AppGtk.yaml`): builds against the
  `org.gnome.Platform` runtime with the Rust SDK extension. For a reproducible
  offline build, generate `cargo-sources.json` with
  [flatpak-cargo-generator](https://github.com/flatpak/flatpak-builder-tools)
  and reference it from the manifest (the manifest notes where). Build:
  ```sh
  flatpak-builder --user --install --force-clean build-dir \
    packaging/flatpak/com.twinleaf.AppGtk.yaml
  ```
- **AppImage**: build `--release`, then bundle `libgtk-4` and its deps with
  [linuxdeploy](https://github.com/linuxdeploy/linuxdeploy) + its GTK plugin,
  shipping the `.desktop` file and an icon.

Install the desktop entry (`linux/com.twinleaf.AppGtk.desktop`) and an icon into
the usual XDG locations for a non-bundled local install.

## macOS

`macos/bundle-macos.sh` builds `build/Twinleaf.app`, copies the release binary,
and relocates the Homebrew GTK4 dylibs into the bundle with `dylibbundler`
(`brew install dylibbundler`). A fully self-contained app also needs the
GdkPixbuf loaders, GIO modules, and compiled GSettings schemas copied into
`Contents/Resources` and the corresponding `GDK_PIXBUF_*` / `GIO_*` /
`XDG_DATA_DIRS` env wired in a launcher — see comments in the script. Code-sign
and notarize for distribution.

## Windows

Build GTK4 with [gvsbuild](https://github.com/wingtk/gvsbuild), point
`PKG_CONFIG_PATH` at it, then `cargo build --release`. Bundle next to
`twinleaf-gtk.exe`: the GTK4 DLLs, `lib/gdk-pixbuf-2.0`, and (if GSettings are
used) `share/glib-2.0/schemas`. Wrap in an MSI/NSIS installer as desired. HDF5
export (`--features hdf5`) is the painful dependency on Windows; leave it off
unless required.
