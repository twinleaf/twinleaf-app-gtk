// SPDX-License-Identifier: Apache-2.0
//! Preferences window (`AdwPreferencesWindow`). Edits the persisted
//! [`Preferences`] (TOML via the `directories` crate). The theme choice applies
//! live through the `AdwStyleManager`; other settings take effect on next use.

use adw::prelude::*;

use crate::persistence::Preferences;

/// Show the preferences window, transient for `parent`. `status_bar` is the
/// bottom status area, toggled live by the "Status bar" switch.
pub fn show_preferences(
    parent: &impl IsA<gtk::Window>,
    style: &adw::StyleManager,
    status_bar: &gtk::Box,
) {
    let prefs = Preferences::load();

    let window = adw::PreferencesWindow::new();
    window.set_title(Some("Preferences"));
    window.set_transient_for(Some(parent));
    window.set_search_enabled(false);

    let page = adw::PreferencesPage::new();

    // Appearance.
    let appearance = adw::PreferencesGroup::new();
    appearance.set_title("Appearance");
    let theme_row = adw::ComboRow::new();
    theme_row.set_title("Theme");
    theme_row.set_subtitle("Follows the system unless overridden");
    let theme_model = gtk::StringList::new(&["System", "Light", "Dark"]);
    theme_row.set_model(Some(&theme_model));
    theme_row.set_selected(match prefs.theme.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
    {
        let style = style.clone();
        theme_row.connect_selected_notify(move |row| {
            let (scheme, name) = match row.selected() {
                1 => (adw::ColorScheme::ForceLight, "light"),
                2 => (adw::ColorScheme::ForceDark, "dark"),
                _ => (adw::ColorScheme::Default, "system"),
            };
            style.set_color_scheme(scheme);
            let mut prefs = Preferences::load();
            prefs.theme = name.to_string();
            prefs.save();
        });
    }
    appearance.add(&theme_row);
    page.add(&appearance);

    // Interface.
    let interface = adw::PreferencesGroup::new();
    interface.set_title("Interface");
    let status_row = adw::SwitchRow::new();
    status_row.set_title("Status bar");
    status_row.set_subtitle("Show the status/connection bar at the bottom");
    status_row.set_active(prefs.show_status_bar);
    {
        let status_bar = status_bar.clone();
        status_row.connect_active_notify(move |row| {
            status_bar.set_visible(row.is_active());
            let mut prefs = Preferences::load();
            prefs.show_status_bar = row.is_active();
            prefs.save();
        });
    }
    interface.add(&status_row);
    page.add(&interface);

    // Logging.
    let logging = adw::PreferencesGroup::new();
    logging.set_title("Logging");
    let autolog_row = adw::SwitchRow::new();
    autolog_row.set_title("Start recording on connect");
    autolog_row.set_subtitle("Begin logging to a .tio as soon as a device streams");
    autolog_row.set_active(prefs.start_logging_on_connect);
    autolog_row.connect_active_notify(move |row| {
        let mut prefs = Preferences::load();
        prefs.start_logging_on_connect = row.is_active();
        prefs.save();
    });
    logging.add(&autolog_row);
    page.add(&logging);

    // Plotting.
    let plotting = adw::PreferencesGroup::new();
    plotting.set_title("Plotting");
    let skip_hub_row = adw::SwitchRow::new();
    skip_hub_row.set_title("Skip comm/hub default graph");
    skip_hub_row.set_subtitle("Auto-plot the first sensor's data instead of a comm/hub board's");
    skip_hub_row.set_active(prefs.skip_comm_hub_default_plot);
    skip_hub_row.connect_active_notify(move |row| {
        let mut prefs = Preferences::load();
        prefs.skip_comm_hub_default_plot = row.is_active();
        prefs.save();
    });
    plotting.add(&skip_hub_row);
    let window_adj = gtk::Adjustment::new(prefs.window_seconds, 1.0, 1000.0, 1.0, 10.0, 0.0);
    let window_row = adw::SpinRow::new(Some(&window_adj), 1.0, 0);
    window_row.set_title("Default plot window");
    window_row.set_subtitle("Seconds of timeseries shown");
    window_adj.connect_value_changed(move |adj| {
        let mut prefs = Preferences::load();
        prefs.window_seconds = adj.value();
        prefs.save();
    });
    plotting.add(&window_row);

    let capture_adj =
        gtk::Adjustment::new(prefs.capture_auto_delay_seconds, 0.05, 10.0, 0.05, 0.5, 0.0);
    let capture_row = adw::SpinRow::new(Some(&capture_adj), 0.05, 2);
    capture_row.set_title("Capture auto-retrigger delay");
    capture_row.set_subtitle("Seconds to wait after a capture before re-triggering");
    capture_adj.connect_value_changed(move |adj| {
        let mut prefs = Preferences::load();
        prefs.capture_auto_delay_seconds = adj.value();
        prefs.save();
    });
    plotting.add(&capture_row);
    page.add(&plotting);

    window.add(&page);
    window.present();
}
