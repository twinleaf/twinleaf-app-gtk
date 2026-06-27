// SPDX-License-Identifier: Apache-2.0
//! twinleaf-gtk — GTK4 user interface for the Twinleaf app, built on
//! `twinleaf-core`. Milestone M1: device discovery, connect, channel selection,
//! and a live timeseries plot.

mod app;
mod bridge;
mod controls;
mod model;
mod persistence;
mod plot;
mod popovers;
mod prefs_ui;
mod sidebar;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "com.twinleaf.AppGtk";

fn main() -> glib::ExitCode {
    // Register the bundled symbolic icons (compiled into the binary by build.rs).
    let resource = gio::Resource::from_data(&glib::Bytes::from_static(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/twinleaf.gresource"
    ))))
    .expect("valid embedded gresource");
    gio::resources_register(&resource);

    let app = adw::Application::builder().application_id(APP_ID).build();

    // Platform-native shortcuts. `<Primary>` resolves to Cmd on macOS and Ctrl
    // elsewhere, so this is Cmd-Q on a Mac and Ctrl-Q on Linux/Windows.
    app.connect_startup(|app| {
        let quit = gio::SimpleAction::new("quit", None);
        let app_for_quit = app.clone();
        quit.connect_activate(move |_, _| {
            // Close each window first so it saves its geometry/preferences; the
            // app then exits once the last window is gone.
            for window in app_for_quit.windows() {
                window.close();
            }
        });
        app.add_action(&quit);

        // Cmd-W / Ctrl-W: close the focused window (the main window saves and,
        // being the last, exits).
        let close = gio::SimpleAction::new("close-window", None);
        let app_for_close = app.clone();
        close.connect_activate(move |_, _| {
            if let Some(window) = app_for_close.active_window() {
                window.close();
            }
        });
        app.add_action(&close);

        // GTK4 binds `<Primary>` to Control even on macOS — it does NOT map it to
        // Command — so platform-native Cmd shortcuts need the `<Meta>` modifier
        // explicitly. Pick the right one per-OS. (The `win.*` actions are defined
        // per main window; see window.rs.)
        let primary = if cfg!(target_os = "macos") {
            "<Meta>"
        } else {
            "<Primary>"
        };
        app.set_accels_for_action("app.quit", &[&format!("{primary}q")]);
        app.set_accels_for_action("app.close-window", &[&format!("{primary}w")]);
        app.set_accels_for_action("win.open", &[&format!("{primary}o")]);
        app.set_accels_for_action("win.preferences", &[&format!("{primary}comma")]);
        app.set_accels_for_action("win.record", &[&format!("{primary}r")]);
    });

    app.connect_activate(window::build_main_window);
    app.run()
}
