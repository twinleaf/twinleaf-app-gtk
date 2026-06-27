// SPDX-License-Identifier: Apache-2.0
//! Cross-platform preference persistence: a `serde` struct serialized to TOML
//! under the platform config dir (via `directories`) — `~/.config/twinleaf/` on
//! Linux, `%APPDATA%\twinleaf\` on Windows, `~/Library/Application Support/…` on
//! macOS. No GSettings/gschema dependency, so it ships anywhere GTK4 runs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub window_width: i32,
    pub window_height: i32,
    pub sidebar_width: i32,
    pub rpc_sidebar_position: i32,
    pub window_seconds: f64,
    /// "system" | "light" | "dark".
    pub theme: String,
    pub remembered_urls: Vec<String>,
    pub show_all_serial_ports: bool,
    /// Show the bottom status/connection bar. Off by default.
    pub show_status_bar: bool,
    /// Seconds to wait after a capture completes before auto-retriggering the
    /// next one (when a capture graph's "Auto" toggle is on).
    pub capture_auto_delay_seconds: f64,
    /// Start recording to a `.tio` automatically once a connection is streaming.
    pub start_logging_on_connect: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            window_width: 1000,
            window_height: 640,
            sidebar_width: 330,
            rpc_sidebar_position: 640,
            window_seconds: 10.0,
            theme: "system".to_string(),
            remembered_urls: Vec::new(),
            show_all_serial_ports: false,
            show_status_bar: false,
            capture_auto_delay_seconds: 0.5,
            start_logging_on_connect: false,
        }
    }
}

impl Preferences {
    pub fn config_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "twinleaf", "Twinleaf")
            .map(|dirs| dirs.config_dir().join("preferences.toml"))
    }

    /// Load preferences, falling back to defaults on any error.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write preferences to disk (best-effort; errors are ignored).
    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, text);
        }
    }

    /// Add a device URL to the remembered list (deduplicated, order-preserving).
    /// A multi-device session is stored as its whitespace-separated URL string.
    pub fn remember_url(&mut self, url: &str) {
        let url = url.trim();
        if !url.is_empty() && !self.remembered_urls.iter().any(|u| u == url) {
            self.remembered_urls.push(url.to_string());
        }
    }

    /// Remove a URL from the remembered list.
    pub fn forget_url(&mut self, url: &str) {
        let url = url.trim();
        self.remembered_urls.retain(|u| u != url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let mut prefs = Preferences::default();
        prefs.window_width = 1234;
        prefs.window_seconds = 25.0;
        prefs.remember_url("udp://127.0.0.1:7855");
        prefs.remember_url("udp://127.0.0.1:7855"); // deduped
        prefs.remember_url("serial:///dev/cu.usbserial");

        let text = toml::to_string_pretty(&prefs).unwrap();
        let back: Preferences = toml::from_str(&text).unwrap();

        assert_eq!(back.window_width, 1234);
        assert_eq!(back.window_seconds, 25.0);
        assert_eq!(back.remembered_urls.len(), 2);
        assert_eq!(back.theme, "system");
    }

    #[test]
    fn falls_back_to_defaults_on_garbage() {
        let prefs: Preferences = toml::from_str("this is not valid === toml").unwrap_or_default();
        assert_eq!(prefs.window_width, 1000);
        assert!(prefs.remembered_urls.is_empty());
    }

    #[test]
    fn partial_toml_uses_field_defaults() {
        let prefs: Preferences = toml::from_str("window_width = 800").unwrap();
        assert_eq!(prefs.window_width, 800);
        assert_eq!(prefs.window_height, 640); // default preserved
    }
}
