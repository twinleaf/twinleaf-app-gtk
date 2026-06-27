// SPDX-License-Identifier: Apache-2.0
//! Header/status popovers driven by core events:
//! - [`FirmwarePopover`] — available updates (`upgradeStatus`) with a per-device
//!   Flash button (`PerformUpgrade`) and live flash progress (`upgradeProgress`).
//! - [`HealthIndicator`] — a status-bar button whose label summarizes stream
//!   health and whose popover lists per-stream diagnostics (`health`).

use std::rc::Rc;

use gtk::prelude::*;
use serde_json::Value;

use crate::app::AppState;
use crate::model::AvailableDevice;

pub struct FirmwarePopover {
    pub button: gtk::MenuButton,
    results: gtk::Box,
    progress_label: gtk::Label,
    progress_bar: gtk::ProgressBar,
    state: Rc<AppState>,
}

impl FirmwarePopover {
    pub fn new(state: &Rc<AppState>) -> Rc<Self> {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(10);
        content.set_margin_end(10);
        content.set_width_request(300);

        let title = gtk::Label::new(Some("Firmware"));
        title.set_xalign(0.0);
        title.add_css_class("heading");
        content.append(&title);

        let results = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&results);

        let progress_label = gtk::Label::new(None);
        progress_label.set_xalign(0.0);
        progress_label.set_wrap(true);
        progress_label.set_visible(false);
        let progress_bar = gtk::ProgressBar::new();
        progress_bar.set_visible(false);
        content.append(&progress_label);
        content.append(&progress_bar);

        let check = gtk::Button::with_label("Check for updates");
        {
            let state = state.clone();
            check.connect_clicked(move |_| state.check_upgrade());
        }
        content.append(&check);

        let popover = gtk::Popover::new();
        popover.set_child(Some(&content));
        // A green up-arrow that stays hidden until an update is actually
        // available (see `set_status`).
        let button = gtk::MenuButton::new();
        button.set_icon_name("tl-update-symbolic");
        button.add_css_class("flat");
        button.add_css_class("tl-update");
        button.set_tooltip_text(Some("A firmware update is available — click to install"));
        button.set_visible(false);
        button.set_popover(Some(&popover));
        // Re-scan whenever the popover is opened.
        {
            let state = state.clone();
            button.connect_active_notify(move |b| {
                if b.is_active() {
                    state.check_upgrade();
                }
            });
        }

        let this = Rc::new(Self {
            button,
            results,
            progress_label,
            progress_bar,
            state: state.clone(),
        });
        this.set_idle();
        this
    }

    fn clear_results(&self) {
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
    }

    fn set_idle(&self) {
        self.clear_results();
        let label = gtk::Label::new(Some("Open to check for updates."));
        label.set_xalign(0.0);
        label.add_css_class("dim-label");
        self.results.append(&label);
    }

    /// Populate from an `upgradeStatus` payload.
    pub fn set_status(&self, value: &Value) {
        self.progress_label.set_visible(false);
        self.progress_bar.set_visible(false);
        self.clear_results();

        let available = value
            .get("available")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();

        // The header arrow is only shown when there is something to install.
        self.button.set_visible(!available.is_empty());
        if available.is_empty() {
            let label = gtk::Label::new(Some("Firmware is up to date."));
            label.set_xalign(0.0);
            self.results.append(&label);
            return;
        }

        for entry in &available {
            let route = entry
                .get("route")
                .and_then(|v| v.as_str())
                .unwrap_or("/")
                .to_string();
            let name = entry
                .get("deviceName")
                .and_then(|v| v.as_str())
                .unwrap_or(route.as_str());
            let cur = entry
                .get("currentVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let new = entry
                .get("newVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("?");

            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let info = gtk::Label::new(Some(&format!("{name}\n{cur}  →  {new}")));
            info.set_xalign(0.0);
            info.set_hexpand(true);
            let flash = gtk::Button::with_label("Flash");
            flash.add_css_class("destructive-action");
            flash.set_valign(gtk::Align::Center);
            {
                let state = self.state.clone();
                let route = route.clone();
                flash.connect_clicked(move |_| state.perform_upgrade(route.clone()));
            }
            row.append(&info);
            row.append(&flash);
            self.results.append(&row);
        }
    }

    /// Update from an `upgradeProgress` payload.
    pub fn set_progress(&self, value: &Value) {
        let phase = value.get("phase").and_then(|v| v.as_str()).unwrap_or("");
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(phase);
        self.progress_label.set_visible(true);
        self.progress_label.set_text(message);
        self.progress_bar.set_visible(true);
        if let Some(fraction) = value.get("fraction").and_then(|v| v.as_f64()) {
            self.progress_bar.set_fraction(fraction);
        } else if phase == "complete" {
            self.progress_bar.set_fraction(1.0);
        } else if phase == "error" {
            self.progress_bar.set_fraction(0.0);
        } else {
            self.progress_bar.pulse();
        }
    }
}

pub struct HealthIndicator {
    pub button: gtk::MenuButton,
    streams: gtk::Box,
}

impl HealthIndicator {
    pub fn new() -> Rc<Self> {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(10);
        content.set_margin_end(10);
        content.set_width_request(300);

        let title = gtk::Label::new(Some("Connection health"));
        title.set_xalign(0.0);
        title.add_css_class("heading");
        content.append(&title);

        let streams = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.append(&streams);

        let popover = gtk::Popover::new();
        popover.set_child(Some(&content));
        // A trailing toolbar button (info icon) shown only while streaming.
        let button = gtk::MenuButton::new();
        button.set_icon_name("tl-health-symbolic");
        button.add_css_class("flat");
        button.set_tooltip_text(Some("Connection health"));
        button.set_visible(false);
        button.set_popover(Some(&popover));

        let this = Rc::new(Self { button, streams });
        this.set_idle();
        this
    }

    fn clear(&self) {
        while let Some(child) = self.streams.first_child() {
            self.streams.remove(&child);
        }
    }

    fn set_idle(&self) {
        self.clear();
        let label = gtk::Label::new(Some("No stream data yet."));
        label.set_xalign(0.0);
        label.add_css_class("dim-label");
        self.streams.append(&label);
    }

    /// Update from a `health` payload.
    pub fn set_health(&self, value: &Value) {
        let streams = value
            .get("streams")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();

        // Only surface the toolbar button once we actually have stream data.
        self.button.set_visible(!streams.is_empty());

        self.clear();
        if streams.is_empty() {
            let label = gtk::Label::new(Some("No stream data yet."));
            label.set_xalign(0.0);
            label.add_css_class("dim-label");
            self.streams.append(&label);
            return;
        }
        for s in &streams {
            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let route = s.get("route").and_then(|v| v.as_str()).unwrap_or("/");
            let rate = s.get("rateHz").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let jitter = s.get("jitterMs").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let ppm = s.get("ppm").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let dropped = s.get("dropped").and_then(|v| v.as_u64()).unwrap_or(0);
            let stale = s.get("stale").and_then(|v| v.as_bool()).unwrap_or(false);

            let block = gtk::Box::new(gtk::Orientation::Vertical, 1);
            let head = gtk::Label::new(Some(&format!(
                "{name}  ({route}){}",
                if stale { "   ⚠ stale" } else { "" }
            )));
            head.set_xalign(0.0);
            head.add_css_class("heading");
            let detail = gtk::Label::new(Some(&format!(
                "{rate:.2} Hz · jitter {jitter:.2} ms · {ppm:.0} ppm · dropped {dropped}"
            )));
            detail.set_xalign(0.0);
            detail.add_css_class("dim-label");
            block.append(&head);
            block.append(&detail);
            self.streams.append(&block);
        }
    }
}

/// The "plug" header button: a popover device picker — auto-discovered devices
/// plus a saved/manual connection list. Each saved entry has a disclosure that
/// expands to multi-line input for a multi-device session (one URL per line,
/// mounted at /0, /1, …).
pub struct DevicePopover {
    pub button: gtk::MenuButton,
    discovered: gtk::ListBox,
    saved: gtk::Box,
    state: Rc<AppState>,
}

impl DevicePopover {
    pub fn new(state: &Rc<AppState>) -> Rc<Self> {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(10);
        content.set_margin_end(10);
        content.set_width_request(340);

        let title = gtk::Label::new(Some("Connect to a device"));
        title.set_xalign(0.0);
        title.add_css_class("heading");
        content.append(&title);

        // Auto-discovered devices.
        content.append(&dim_label("Discovered"));
        let discovered = gtk::ListBox::new();
        discovered.set_selection_mode(gtk::SelectionMode::None);
        discovered.add_css_class("boxed-list");
        let disc_scroll = gtk::ScrolledWindow::new();
        disc_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        disc_scroll.set_min_content_height(120);
        disc_scroll.set_child(Some(&discovered));
        content.append(&disc_scroll);

        // Saved / manual connections.
        content.append(&dim_label("Saved connections"));
        let saved = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&saved);

        // "New connection" box: a URL field + a "+" button. For a multi-device
        // session, space-separate the URLs (the core mounts each at /0, /1, …).
        let add_entry = gtk::Entry::new();
        add_entry.set_placeholder_text(Some(
            "tcp://host:port   (space-separate for several devices)",
        ));
        add_entry.set_hexpand(true);
        let add_btn = gtk::Button::from_icon_name("tl-add-symbolic");
        add_btn.set_tooltip_text(Some("Add this connection"));
        let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        add_row.append(&add_entry);
        add_row.append(&add_btn);
        content.append(&add_row);

        let popover = gtk::Popover::new();
        popover.set_child(Some(&content));
        let button = gtk::MenuButton::new();
        button.set_icon_name("tl-connect-symbolic");
        button.set_tooltip_text(Some("Connect to a device"));
        // Primary/accent styling to invite a connection; cleared once streaming
        // (see the Status handler in window.rs).
        button.add_css_class("suggested-action");
        button.set_popover(Some(&popover));

        let this = Rc::new(Self {
            button,
            discovered,
            saved,
            state: state.clone(),
        });

        // Re-enumerate devices and reload saved connections when opened.
        {
            let this = this.clone();
            let button = this.button.clone();
            button.connect_active_notify(move |b| {
                if b.is_active() {
                    this.state.refresh_devices();
                    this.reload_saved();
                }
            });
        }

        // "+": save the typed connection (whitespace-separated URLs become a
        // multi-device session).
        let add: Rc<dyn Fn()> = {
            let this = this.clone();
            let add_entry = add_entry.clone();
            Rc::new(move || {
                let text = add_entry
                    .text()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.is_empty() {
                    return;
                }
                let mut prefs = crate::persistence::Preferences::load();
                prefs.remember_url(&text);
                prefs.save();
                this.reload_saved();
                add_entry.set_text("");
            })
        };
        {
            let add = add.clone();
            add_btn.connect_clicked(move |_| add());
        }
        {
            let add = add.clone();
            add_entry.connect_activate(move |_| add());
        }

        this.reload_saved();
        this
    }

    fn clear(&self) {
        while let Some(child) = self.discovered.first_child() {
            self.discovered.remove(&child);
        }
    }

    /// Rebuild the saved-connection rows from remembered preferences.
    fn reload_saved(&self) {
        while let Some(child) = self.saved.first_child() {
            self.saved.remove(&child);
        }
        for url in crate::persistence::Preferences::load().remembered_urls {
            self.append_saved_row(Some(url));
        }
    }

    /// Append one memorized-connection row: an editable URL field with Connect
    /// and Remove — no disclosure (multi-device editing lives in the "New
    /// connection" box). A multi-device session shows as its space-separated
    /// URL string and can be edited inline.
    fn append_saved_row(&self, initial: Option<String>) {
        let url = initial.clone().unwrap_or_default();
        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        entry.set_placeholder_text(Some("tcp://host:port"));
        entry.set_text(url.trim());
        let connect_btn = gtk::Button::with_label("Connect");
        connect_btn.add_css_class("suggested-action");
        connect_btn.set_valign(gtk::Align::Center);
        let remove_btn = gtk::Button::with_label("✕");
        remove_btn.add_css_class("flat");
        remove_btn.set_valign(gtk::Align::Center);
        remove_btn.set_tooltip_text(Some("Remove this connection"));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&entry);
        row.append(&connect_btn);
        row.append(&remove_btn);

        let do_connect: Rc<dyn Fn()> = {
            let entry = entry.clone();
            let state = self.state.clone();
            let button = self.button.clone();
            let original = initial.clone();
            Rc::new(move || {
                let url = entry.text().trim().to_string();
                if url.is_empty() {
                    return;
                }
                // Editing a saved entry replaces it rather than duplicating.
                if let Some(orig) = &original {
                    if orig != &url {
                        let mut prefs = crate::persistence::Preferences::load();
                        prefs.forget_url(orig);
                        prefs.save();
                    }
                }
                state.connect(url, None);
                button.popdown();
            })
        };
        {
            let d = do_connect.clone();
            connect_btn.connect_clicked(move |_| d());
        }
        {
            let d = do_connect.clone();
            entry.connect_activate(move |_| d());
        }

        {
            let saved = self.saved.clone();
            let row = row.clone();
            let original = initial.clone();
            remove_btn.connect_clicked(move |_| {
                if let Some(orig) = &original {
                    let mut prefs = crate::persistence::Preferences::load();
                    prefs.forget_url(orig);
                    prefs.save();
                }
                saved.remove(&row);
            });
        }

        self.saved.append(&row);
    }

    /// Rebuild the discovered-devices list from a `deviceList` payload. Each row
    /// connects to that device and closes the popover.
    pub fn set_available(&self, devices: &[AvailableDevice]) {
        self.clear();
        if devices.is_empty() {
            let row = gtk::Label::new(Some("Searching for devices…"));
            row.set_xalign(0.0);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.add_css_class("dim-label");
            self.discovered.append(&row);
            return;
        }
        for dev in devices {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_top(4);
            row.set_margin_bottom(4);
            row.set_margin_start(4);
            row.set_margin_end(4);
            let title = if dev.label.is_empty() {
                &dev.url
            } else {
                &dev.label
            };
            let detail = if dev.detail.is_empty() {
                &dev.url
            } else {
                &dev.detail
            };
            let info = gtk::Box::new(gtk::Orientation::Vertical, 0);
            info.set_hexpand(true);
            let name = gtk::Label::new(Some(title));
            name.set_xalign(0.0);
            let sub = gtk::Label::new(Some(detail));
            sub.set_xalign(0.0);
            sub.add_css_class("dim-label");
            info.append(&name);
            info.append(&sub);
            let btn = gtk::Button::with_label("Connect");
            btn.set_valign(gtk::Align::Center);
            {
                let state = self.state.clone();
                let url = dev.url.clone();
                let button = self.button.clone();
                btn.connect_clicked(move |_| {
                    state.connect(url.clone(), None);
                    button.popdown();
                });
            }
            row.append(&info);
            row.append(&btn);
            self.discovered.append(&row);
        }
    }
}

fn dim_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_margin_top(4);
    label.add_css_class("dim-label");
    label
}
