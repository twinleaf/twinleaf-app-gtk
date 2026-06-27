# Packaging

Cross-platform distribution for `twinleaf-gtk`. App id: `com.twinleaf.AppGtk`.

These are working starting points; treat the Flatpak/macOS manifests as
templates to validate on a real builder.

## CI / releases (GitHub Actions)

- **`.github/workflows/ci.yml`** — build + test on every push/PR (ubuntu-24.04;
  GTK 4.14 satisfies the crate's `v4_10`/`v1_4` features). Mirrors
  `.gitlab-ci.yml`, which still runs on code.twinleaf.com.
- **`.github/workflows/release.yml`** — on a `v*` tag (or manual dispatch with a
  tag input) builds and attaches to one GitHub Release:
  - **Linux**: `com.twinleaf.AppGtk.flatpak` (Flatpak single-file bundle, GNOME 47 runtime).
  - **Windows**: `twinleaf-gtk-windows-x86_64.zip` (portable; MSYS2/UCRT64 build + `bundle-windows.sh`).
  - **macOS**: `twinleaf-gtk-macos-{arm64,x86_64}.zip` (self-contained `Twinleaf.app`; signed + notarized when secrets are set).

  ```sh
  git tag v0.1.0 && git push github.com v0.1.0   # 'github.com' is this repo's GitHub remote
  ```

### macOS signing secrets (repo → Settings → Secrets → Actions)

The macOS sign/notarize steps run only when `MACOS_SIGN_IDENTITY` is set;
without the secrets the job still uploads an unsigned `.app` zip (users
right-click → Open once, or `xattr -dr com.apple.quarantine Twinleaf.app`).

| Secret | What it is |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | base64 of your Developer ID Application cert (`base64 -i cert.p12`) |
| `MACOS_CERTIFICATE_PASSWORD` | password for the `.p12` |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Twinleaf LLC (TEAMID)` |
| `MACOS_KEYCHAIN_PASSWORD` | any throwaway string (temp keychain in CI) |
| `APPLE_ID` | Apple ID email for notarization |
| `APPLE_TEAM_ID` | 10-char Team ID |
| `APPLE_APP_PASSWORD` | app-specific password (appleid.apple.com) |

> **Expect 1–2 CI round-trips to go fully green.** The fiddly parts are the
> Windows DLL/loader collection (`bundle-windows.sh`) and the macOS dylib/loader
> relinking (`bundle-macos.sh`) — both can't be validated off-runner. The app
> icon (`linux/com.twinleaf.AppGtk.svg`) and `metainfo.xml` are placeholders.

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
