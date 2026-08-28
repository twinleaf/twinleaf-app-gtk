// SPDX-License-Identifier: Apache-2.0
//! Shared, main-thread application state (the gtk-rs analogue of the SwiftUI
//! `BridgeClient` view-model). Held in an `Rc<AppState>` captured by widget
//! callbacks and the event pump; data lives behind a `RefCell`. The worker
//! threads never touch this — they only send `CoreEvent`s.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;
use twinleaf_core::{
    ColumnKeyDto, CoreCommand, ExportFormat, PlotMode, PlotPaneConfig, TwinleafCore, ViewConfig,
};

use crate::model::{AvailableDevice, DeviceInfo};

/// Per-pane plot state: the plotted columns plus that pane's own view config
/// (mode, time window, FFT log axes, ...). Each graph is configured
/// independently.
#[derive(Default, Clone)]
pub struct PaneState {
    pub columns: Vec<ColumnKeyDto>,
    pub view: ViewConfig,
    /// Frozen view end for pan/scrub-back; `None` follows the latest data.
    pub viewport_end: Option<f64>,
}

#[derive(Default)]
pub struct AppData {
    pub available: Vec<AvailableDevice>,
    pub devices: Vec<DeviceInfo>,
    /// One entry per plot pane, indexed by pane id (== position). Always has at
    /// least one pane.
    pub panes: Vec<PaneState>,
    pub column_values: HashMap<ColumnKeyDto, f64>,
    pub connected: bool,
    /// Whether the current session is logging to a `.tio` file, and where.
    pub logging: bool,
    pub log_path: Option<PathBuf>,
    /// Temp path of a recording made this session that hasn't been saved to a
    /// permanent location yet. Set when recording starts; cleared after a Save.
    pub recording_path: Option<PathBuf>,
    /// User-chosen log destination (via "Save As…"); `None` records to a temp
    /// file. Reset on each connection.
    pub log_destination: Option<PathBuf>,
    /// Whether logging has been auto-started for the current connection, so the
    /// "start logging on connect" preference fires only once per session.
    pub auto_logged: bool,
    /// When true, the live plot display is frozen (UI-side only).
    pub paused: bool,
    /// Whether a firmware-update check has been issued for this connection
    /// (drives the once-per-connect auto-check).
    pub upgrade_checked: bool,
    /// True while inspecting an opened `.tio` log (no live device to read
    /// RPC values from).
    pub inspection: bool,
    /// In inspection mode, the opened log's `[start, end]` time range (from the
    /// `playback` event); used to clamp pan to the available data.
    pub playback_range: Option<(f64, f64)>,
}

pub struct AppState {
    pub core: TwinleafCore,
    pub data: RefCell<AppData>,
    rpc_seq: Cell<u64>,
}

impl AppState {
    pub fn new(core: TwinleafCore) -> Self {
        Self {
            core,
            data: RefCell::new(AppData {
                panes: vec![PaneState::default()],
                ..AppData::default()
            }),
            rpc_seq: Cell::new(0),
        }
    }

    fn next_request_id(&self) -> String {
        let id = self.rpc_seq.get() + 1;
        self.rpc_seq.set(id);
        format!("req-{id}")
    }

    /// Call a device RPC. `arg = None` reads a value (or triggers an
    /// action/capture); `arg = Some(value)` writes. Replies arrive as
    /// `rpcResult` events tagged with the returned `requestId`.
    pub fn call_rpc(&self, route: String, name: String, arg: Option<Value>) {
        self.core.send(CoreCommand::CallRpc {
            request_id: self.next_request_id(),
            route,
            name,
            arg,
        });
    }

    /// Export the current recording to `output_path`. Returns false if there is
    /// nothing recorded yet. Result arrives as an `exportResult` event.
    pub fn export_log(&self, output_path: String, format: ExportFormat) -> bool {
        let source = self.data.borrow().log_path.clone();
        match source {
            Some(path) => {
                self.core.send(CoreCommand::ExportLog {
                    request_id: self.next_request_id(),
                    source_path: path.to_string_lossy().into_owned(),
                    output_path,
                    format,
                });
                true
            }
            None => false,
        }
    }

    /// Copy a pane's data as TSV to the clipboard (arrives as a `viewData`
    /// event). Driven by the plot's right-click menu.
    pub fn copy_view_data_pane(&self, pane: usize) {
        self.core.send(CoreCommand::CopyViewData {
            request_id: self.next_request_id(),
            pane_id: Some(pane),
            viewport_end: None,
        });
    }

    /// Open a `.tio` log for inspection/playback.
    pub fn open_log(&self, path: String) {
        {
            let mut data = self.data.borrow_mut();
            data.inspection = true;
            // Start clean so the opened file's default channel is auto-selected.
            for p in data.panes.iter_mut() {
                p.columns.clear();
            }
        }
        self.core.send(CoreCommand::OpenLog { path });
    }

    /// Move the playback cursor (inspection mode), in seconds.
    pub fn set_playback(&self, position: f64) {
        self.core.send(CoreCommand::SetPlayback { position });
    }

    /// Check connected devices for available firmware updates.
    pub fn check_upgrade(&self) {
        self.core.send(CoreCommand::CheckUpgrade);
    }

    /// Flash the available firmware update for a device route.
    /// (Invoked by the firmware popover — UI wiring is the M8 follow-up.)
    #[allow(dead_code)]
    pub fn perform_upgrade(&self, route: String) {
        self.core.send(CoreCommand::PerformUpgrade { route });
    }

    /// Connect to a device URL (optionally at a sub-route). The URL is added to
    /// the remembered list for quick reconnects.
    pub fn connect(&self, url: String, route: Option<String>) {
        {
            let mut data = self.data.borrow_mut();
            data.upgrade_checked = false;
            data.inspection = false;
            data.playback_range = None;
            data.log_destination = None;
            data.auto_logged = false;
            for p in data.panes.iter_mut() {
                p.viewport_end = None; // resume live on a fresh connection
                p.columns.clear(); // start clean so a default channel is auto-selected
            }
        }
        let mut prefs = crate::persistence::Preferences::load();
        prefs.remember_url(&url);
        prefs.save();
        self.core.send(CoreCommand::Connect {
            url,
            route,
            log_path: None,
        });
    }

    /// Ask the core to (re)enumerate available devices.
    pub fn refresh_devices(&self) {
        self.core.send(CoreCommand::ListDevices {
            include_all: Some(false),
        });
    }

    /// Start or stop logging the live session to a `.tio` file in the temp
    /// directory. Only effective while connected (ignored otherwise). A proper
    /// document/Save-As model arrives in M7.
    pub fn set_logging(&self, enabled: bool) {
        let default = std::env::temp_dir().join("twinleaf-recording.tio");
        // Record to the user-chosen destination if one was set via "Save As…",
        // otherwise a temp file.
        let (path, to_temp) = match self.data.borrow().log_destination.clone() {
            Some(dest) => (dest, false),
            None => (default, true),
        };
        let log_path = enabled.then(|| path.to_string_lossy().into_owned());
        {
            let mut data = self.data.borrow_mut();
            data.logging = enabled;
            data.log_path = enabled.then(|| path.clone());
            // Only a temp recording needs the save-on-close prompt; a chosen
            // destination is already where the user wants it.
            if enabled {
                data.recording_path = to_temp.then(|| path.clone());
            }
        }
        self.core
            .send(CoreCommand::SetLogging { enabled, log_path });
    }

    /// Set the log file destination ("Save As…"). If recording is in progress,
    /// move the recording to `path` and continue there; otherwise the path takes
    /// effect when recording next starts.
    pub fn save_log_as(&self, path: PathBuf) {
        let relocate = {
            let mut data = self.data.borrow_mut();
            data.log_destination = Some(path.clone());
            if data.logging {
                data.log_path = Some(path.clone());
                data.recording_path = None; // now recording to a permanent path
                true
            } else {
                false
            }
        };
        if relocate {
            self.core.send(CoreCommand::RelocateLog {
                path: path.to_string_lossy().into_owned(),
            });
        }
    }

    /// Add or remove a set of columns from pane 0 together (a unified channel
    /// toggles every matching device's column at once), then resync.
    pub fn toggle_columns(&self, keys: Vec<ColumnKeyDto>, active: bool) {
        {
            let mut data = self.data.borrow_mut();
            if data.panes.is_empty() {
                data.panes.push(PaneState::default());
            }
            for key in &keys {
                data.panes[0].columns.retain(|k| k != key);
            }
            if active {
                for key in keys {
                    data.panes[0].columns.push(key);
                }
            }
        }
        self.resync_plot();
    }

    /// Add a column to a specific pane (channel drag-and-drop), then resync.
    pub fn add_column_to_pane(&self, pane: usize, key: ColumnKeyDto) {
        {
            let mut data = self.data.borrow_mut();
            if let Some(p) = data.panes.get_mut(pane) {
                if !p.columns.contains(&key) {
                    p.columns.push(key);
                }
            }
        }
        self.resync_plot();
    }

    /// Append an empty plot pane (max 5). Returns its index, or None if full.
    pub fn add_pane(&self) -> Option<usize> {
        let mut data = self.data.borrow_mut();
        if data.panes.len() >= 5 {
            return None;
        }
        // New graphs inherit the most recent graph's settings.
        let view = data
            .panes
            .last()
            .map(|p| p.view.clone())
            .unwrap_or_default();
        data.panes.push(PaneState {
            columns: Vec::new(),
            view,
            viewport_end: None,
        });
        Some(data.panes.len() - 1)
    }

    /// Remove the pane at `idx`. Zero panes is allowed — the main view can then
    /// show only captures/sliders. Returns true if a pane was removed.
    pub fn remove_pane(&self, idx: usize) -> bool {
        {
            let mut data = self.data.borrow_mut();
            if idx >= data.panes.len() {
                return false;
            }
            data.panes.remove(idx);
        }
        self.resync_plot();
        true
    }

    /// Freeze/unfreeze the live plot display (UI-side only; the core keeps
    /// streaming so resuming jumps to the latest data).
    pub fn set_paused(&self, paused: bool) {
        self.data.borrow_mut().paused = paused;
    }

    /// A copy of a pane's current view config (to initialize its settings UI).
    pub fn pane_view(&self, pane: usize) -> ViewConfig {
        self.data
            .borrow()
            .panes
            .get(pane)
            .map(|p| p.view.clone())
            .unwrap_or_default()
    }

    /// Switch one pane's plot mode (timeseries / FFT) and resync.
    pub fn set_pane_mode(&self, pane: usize, mode: PlotMode) {
        if let Some(p) = self.data.borrow_mut().panes.get_mut(pane) {
            p.view.mode = mode;
        }
        self.resync_plot();
    }

    /// Change one pane's displayed time window (seconds) and resync.
    pub fn set_pane_window(&self, pane: usize, seconds: f64) {
        if let Some(p) = self.data.borrow_mut().panes.get_mut(pane) {
            p.view.window_seconds = seconds;
        }
        self.resync_plot();
    }

    /// Freeze the pane's view ending at `viewport_end` (pan/scrub back), or
    /// follow the latest data when `None`. Resyncs.
    pub fn set_pane_viewport(&self, pane: usize, viewport_end: Option<f64>) {
        if let Some(p) = self.data.borrow_mut().panes.get_mut(pane) {
            p.viewport_end = viewport_end;
        }
        self.resync_plot();
    }

    /// Set one pane's FFT log-axis flags and resync.
    pub fn set_pane_fft_log(&self, pane: usize, log_x: bool, log_y: bool) {
        if let Some(p) = self.data.borrow_mut().panes.get_mut(pane) {
            p.view.fft_log_x = log_x;
            p.view.fft_log_y = log_y;
        }
        self.resync_plot();
    }

    /// Push all panes to the core: the union of their columns as the active
    /// set (for live values) and one `PlotPaneConfig` per pane (shared view).
    pub fn resync_plot(&self) {
        let panes = self.data.borrow().panes.clone();
        let mut active: Vec<ColumnKeyDto> = Vec::new();
        for p in &panes {
            for key in &p.columns {
                if !active.contains(key) {
                    active.push(key.clone());
                }
            }
        }
        self.core
            .send(CoreCommand::SetActiveColumns { columns: active });
        let configs = panes
            .into_iter()
            .enumerate()
            .map(|(id, p)| PlotPaneConfig {
                id,
                view: p.view,
                columns: p.columns,
                viewport_end: p.viewport_end,
            })
            .collect();
        self.core.send(CoreCommand::SetPlotPanes { panes: configs });
    }
}
