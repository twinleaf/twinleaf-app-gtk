// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]

use crossbeam::channel::{self, Receiver, Sender};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use twinleaf::data::{BoundaryReason, DeviceDataParser, DeviceFullMetadata, Sample};
use twinleaf::device::capture::{read_capture, CaptureReadout, CaptureRpc};
#[cfg(feature = "serial")]
use twinleaf::device::discovery::PortInterface;
#[cfg(any(feature = "serial", feature = "mdns"))]
use twinleaf::device::discovery::{self};
use twinleaf::device::{
    DeviceEvent, DeviceTree, RpcDescriptor, RpcMetaFlags, RpcValueType, TreeEvent,
};
#[cfg(feature = "firmware")]
use twinleaf::firmware::{self, FirmwareCatalog, FlashEvent, StopOutcome, UpdateStatus};
use twinleaf::tio::proto::meta::MetadataEpoch;
use twinleaf::tio::proto::{DeviceRoute, Payload, RpcMethod};
use twinleaf::tio::proxy;
use twinleaf::tio::util::TioRpcReplyable;
use twinleaf::{tio, Device};
use welch_sde::{Build, SpectralDensity};

#[cfg(feature = "serial")]
const DEVICE_LIST_DISCOVERY_WINDOW: Duration = Duration::from_millis(300);
#[cfg(feature = "serial")]
const DEVICE_LIST_NAME_RPC_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(feature = "serial")]
const DEVICE_LIST_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Upper bound on each mDNS browse. `enumerate_mdns` exits early once results
/// go quiet, so a warm cache resolves well under this; the cap only applies on
/// a cold first browse. Kept short so periodic refreshes stay snappy.
#[cfg(feature = "mdns")]
const MDNS_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1500);

const RPC_METADATA_RETRY_ATTEMPTS: usize = 3;
const RPC_METADATA_RETRY_DELAY: Duration = Duration::from_millis(50);
const CONNECTION_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECTION_METADATA_RETRY_DELAY: Duration = Duration::from_millis(500);
const RPC_METADATA_RECOVERY_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum spacing between metadata/RPC refreshes triggered by a device reset,
/// so a device that reboots repeatedly can't keep the session busy refetching.
const DEVICE_RESET_REFRESH_COOLDOWN: Duration = Duration::from_secs(5);

/// Health diagnostics (mirrors `tio health`): jitter window length, the sample
/// count before drift/PPM is trusted, the stale-stream floor, and the emit
/// cadence to the UI.
const HEALTH_JITTER_WINDOW_SECONDS: u64 = 10;
const HEALTH_MIN_DRIFT_SAMPLES: u64 = 50;
const HEALTH_STALE_FLOOR: Duration = Duration::from_millis(2000);
const HEALTH_EMIT_INTERVAL: Duration = Duration::from_millis(500);

/// Plot frame handed to the UI in-process (replaces the old binary EVENT_PLOT
/// payload). Carries owned, already-decimated series for one pane.
#[derive(Debug, Clone)]
pub struct PlotFrame {
    pub pane_id: usize,
    pub mode: PlotMode,
    pub viewport_end: Option<f64>,
    pub series: Vec<PlotSeriesData>,
}

/// One decimated series within a [`PlotFrame`].
#[derive(Debug, Clone)]
pub struct PlotSeriesData {
    pub key: ColumnKeyDto,
    pub sample_rate: f64,
    /// True when the column has data but none falls inside the pane's displayed
    /// time window; sent with no points so the legend can still flag it.
    pub outside_window: bool,
    pub points: Vec<Point>,
    /// Robust white-noise ASD estimate for FFT series, in `units/sqrt(Hz)`.
    /// `None` outside FFT mode, or when the spectrum is too short or too
    /// contaminated to support an estimate.
    pub noise_floor: Option<f64>,
}

/// Events published by the core runtime to the UI. The hot path (plot frames)
/// and common control events are fully typed; remaining structured events are
/// carried as tagged JSON (a `type` field) during the port and will be typed
/// incrementally as the UI consumes them.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    Status { state: String, message: String },
    Error(String),
    Debug(String),
    Plot(Arc<PlotFrame>),
    Json(Value),
}

/// Sink used by the runtime to publish [`CoreEvent`]s to the UI. Cloneable so
/// every worker thread (session, FFT, firmware, logging) can emit; sends use
/// `send_blocking`, which is safe from any thread.
#[derive(Clone)]
pub struct EventSink {
    tx: async_channel::Sender<CoreEvent>,
}

impl EventSink {
    pub fn new(tx: async_channel::Sender<CoreEvent>) -> Self {
        Self { tx }
    }

    fn send(&self, event: CoreEvent) {
        // A closed receiver means the UI is gone and we are shutting down.
        let _ = self.tx.send_blocking(event);
    }

    /// Emit a structured event as tagged JSON. Keeps every existing
    /// `emit(&json!({"type": ...}))` call site working unchanged.
    fn emit<T: Serialize>(&self, value: &T) {
        if let Ok(value) = serde_json::to_value(value) {
            self.send(CoreEvent::Json(value));
        }
    }

    fn emit_plot_frame(
        &self,
        pane_id: usize,
        mode: PlotMode,
        viewport_end: Option<f64>,
        series: &[BinaryPlotSeries<'_>],
    ) -> io::Result<()> {
        let series = series
            .iter()
            .map(|item| PlotSeriesData {
                key: item.key.clone(),
                sample_rate: item.sample_rate,
                outside_window: item.outside_window,
                noise_floor: item.noise_floor,
                points: item.points.collect(),
            })
            .collect();
        self.send(CoreEvent::Plot(Arc::new(PlotFrame {
            pane_id,
            mode,
            viewport_end,
            series,
        })));
        Ok(())
    }

    fn status(&self, state: &str, message: impl Into<String>) {
        self.send(CoreEvent::Status {
            state: state.to_string(),
            message: message.into(),
        });
    }

    fn error(&self, message: impl Into<String>) {
        self.send(CoreEvent::Error(message.into()));
    }

    fn debug(&self, message: impl AsRef<str>) {
        if !bridge_debug_enabled() {
            return;
        }
        let message = message.as_ref();
        eprintln!("[twinleaf-core] {message}");
        self.send(CoreEvent::Debug(message.to_string()));
    }
}

/// Top-level runtime command loop (ported from the SwiftUI app's
/// `run_direct_runtime`). Owns the streaming session thread and the playback
/// session, dispatching [`CoreCommand`]s sent by the UI.
pub fn run_runtime(command_rx: Receiver<CoreCommand>, emitter: EventSink) {
    let mut session: Option<(Sender<SessionCommand>, thread::JoinHandle<()>)> = None;
    let mut playback: Option<PlaybackSession> = None;
    emitter.status("ready", "Twinleaf core ready");

    while let Ok(command) = command_rx.recv() {
        match command {
            CoreCommand::ListDevices { include_all } => {
                let devices = list_available_devices(include_all.unwrap_or(false));
                emitter.emit(&json!({
                    "type": "deviceList",
                    "devices": devices
                }));
            }
            CoreCommand::Connect {
                url,
                route,
                log_path,
            } => {
                emitter.debug(format!("connect command received for {url}"));
                stop_direct_session(&mut session);
                playback = None;
                let (tx, rx) = channel::unbounded();
                let session_emitter = emitter.clone();
                let handle = match thread::Builder::new()
                    .name("twinleaf-session".into())
                    .spawn(move || run_session(url, route, log_path, rx, session_emitter))
                {
                    Ok(handle) => handle,
                    Err(err) => {
                        emitter.error(format!("Failed to start session thread: {err}"));
                        continue;
                    }
                };
                session = Some((tx, handle));
            }
            CoreCommand::SetLogging { enabled, log_path } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::SetLogging { enabled, log_path });
                } else {
                    emitter.debug("logging change ignored; no active streaming session");
                }
            }
            CoreCommand::RelocateLog { path } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::RelocateLog { path });
                } else {
                    emitter.debug("relocate log ignored; no active streaming session");
                }
            }
            CoreCommand::OpenLog { path } => {
                emitter.debug(format!("openLog command received for {path}"));
                stop_direct_session(&mut session);
                match load_log_file(&path, &emitter) {
                    Ok(session_state) => {
                        emitter.status("inspection", format!("Opened log file {path}"));
                        emitter.emit(&json!({
                            "type": "metadata",
                            "devices": &session_state.devices
                        }));
                        session_state.emit_state(&emitter);
                        session_state.emit_plot(&emitter);
                        session_state.emit_stream_values(&emitter);
                        playback = Some(session_state);
                    }
                    Err(err) => {
                        playback = None;
                        emitter.error(format!("Failed to open log file: {err}"));
                    }
                }
            }
            CoreCommand::ExportLog {
                request_id,
                source_path,
                output_path,
                format,
            } => {
                emitter.status(
                    "exporting",
                    format!("Exporting {} as {:?}", source_path, format),
                );
                let result = export_log_file(&source_path, &output_path, format, &emitter);
                emit_export_result(&emitter, request_id, output_path, format, result);
            }
            CoreCommand::Disconnect => {
                emitter.debug("disconnect command received");
                stop_direct_session(&mut session);
                playback = None;
                emitter.status("disconnected", "Disconnected");
            }
            CoreCommand::SetPlayback { position } => {
                if let Some(playback) = playback.as_mut() {
                    playback.set_position(position);
                    playback.emit_state(&emitter);
                    playback.emit_plot(&emitter);
                    playback.emit_stream_values(&emitter);
                }
            }
            CoreCommand::CopyViewData {
                request_id,
                pane_id,
                viewport_end,
            } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::CopyViewData {
                        request_id,
                        pane_id,
                        viewport_end,
                    });
                } else if let Some(playback) = playback.as_mut() {
                    let end = viewport_end.unwrap_or(playback.position);
                    let pane = playback
                        .plot_panes
                        .iter()
                        .find(|pane| Some(pane.id) == pane_id)
                        .or_else(|| playback.plot_panes.first());
                    let (columns, view) = pane
                        .map(|pane| (pane_columns_set(pane), pane.view.clone()))
                        .unwrap_or_else(|| {
                            (playback.active_columns.clone(), playback.view.clone())
                        });
                    let text = build_view_data_tsv(
                        &playback.column_states,
                        &columns,
                        &view,
                        Some(playback.clamp_position(end)),
                    );
                    emit_view_data(&emitter, request_id, Ok(text));
                } else {
                    emit_view_data(&emitter, request_id, Err("No active data view".to_string()));
                }
            }
            CoreCommand::SetActiveColumns { columns } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::SetActiveColumns(columns));
                } else if let Some(playback) = playback.as_mut() {
                    playback.set_active_columns(columns);
                    emit_active_columns(&emitter, &playback.active_columns);
                    playback.emit_plot(&emitter);
                    playback.emit_stream_values(&emitter);
                }
            }
            CoreCommand::SetPlotPanes { panes } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::SetPlotPanes(panes));
                } else if let Some(playback) = playback.as_mut() {
                    playback.set_plot_panes(panes);
                    emit_active_columns(&emitter, &playback.active_columns);
                    playback.emit_state(&emitter);
                    playback.emit_plot(&emitter);
                    playback.emit_stream_values(&emitter);
                }
            }
            CoreCommand::SetView { view } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::SetView(view));
                } else if let Some(playback) = playback.as_mut() {
                    playback.set_view(view);
                    playback.emit_state(&emitter);
                    playback.emit_plot(&emitter);
                    playback.emit_stream_values(&emitter);
                }
            }
            CoreCommand::CallRpc {
                request_id,
                route,
                name,
                arg,
            } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::CallRpc {
                        request_id,
                        route,
                        name,
                        arg,
                    });
                }
            }
            CoreCommand::CheckUpgrade => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::CheckUpgrade);
                }
            }
            CoreCommand::PerformUpgrade { route } => {
                if let Some((tx, _)) = &session {
                    let _ = tx.send(SessionCommand::PerformUpgrade { route });
                }
            }
            CoreCommand::Shutdown => {
                emitter.debug("shutdown command received");
                stop_direct_session(&mut session);
                emitter.status("exiting", "Core shutting down");
                break;
            }
        }
    }

    stop_direct_session(&mut session);
}

fn stop_direct_session(session: &mut Option<(Sender<SessionCommand>, thread::JoinHandle<()>)>) {
    if let Some((tx, handle)) = session.take() {
        let _ = tx.send(SessionCommand::Stop);
        let _ = handle.join();
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CoreCommand {
    ListDevices {
        include_all: Option<bool>,
    },
    Connect {
        url: String,
        route: Option<String>,
        log_path: Option<String>,
    },
    SetLogging {
        enabled: bool,
        log_path: Option<String>,
    },
    /// Move an in-progress recording to `path` and continue logging there.
    RelocateLog {
        path: String,
    },
    OpenLog {
        path: String,
    },
    ExportLog {
        request_id: String,
        source_path: String,
        output_path: String,
        format: ExportFormat,
    },
    Disconnect,
    SetPlayback {
        position: f64,
    },
    CopyViewData {
        request_id: String,
        pane_id: Option<usize>,
        viewport_end: Option<f64>,
    },
    SetActiveColumns {
        columns: Vec<ColumnKeyDto>,
    },
    SetPlotPanes {
        panes: Vec<PlotPaneConfig>,
    },
    SetView {
        view: ViewConfig,
    },
    CallRpc {
        request_id: String,
        route: String,
        name: String,
        arg: Option<Value>,
    },
    CheckUpgrade,
    PerformUpgrade {
        route: String,
    },
    Shutdown,
}

#[derive(Debug)]
enum SessionCommand {
    Stop,
    SetActiveColumns(Vec<ColumnKeyDto>),
    SetLogging {
        enabled: bool,
        log_path: Option<String>,
    },
    RelocateLog {
        path: String,
    },
    SetView(ViewConfig),
    SetPlotPanes(Vec<PlotPaneConfig>),
    CallRpc {
        request_id: String,
        route: String,
        name: String,
        arg: Option<Value>,
    },
    CopyViewData {
        request_id: String,
        pane_id: Option<usize>,
        viewport_end: Option<f64>,
    },
    CheckUpgrade,
    PerformUpgrade {
        route: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct ColumnKeyDto {
    pub route: String,
    pub stream_id: u8,
    pub column_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewConfig {
    pub mode: PlotMode,
    pub window_seconds: f64,
    pub resolution_multiplier: usize,
    pub plot_width_pixels: usize,
    pub decimation_method: DecimationMethod,
    pub detrend: DetrendMethod,
    pub fft_log_x: bool,
    pub fft_log_y: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlotPaneConfig {
    pub id: usize,
    pub view: ViewConfig,
    pub columns: Vec<ColumnKeyDto>,
    /// Freeze the live view ending at this time (pan/scrub back through the
    /// retained buffer); `None` follows the latest data.
    #[serde(default)]
    pub viewport_end: Option<f64>,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            mode: PlotMode::Timeseries,
            window_seconds: 10.0,
            resolution_multiplier: 100,
            plot_width_pixels: 800,
            decimation_method: DecimationMethod::Fpcs,
            detrend: DetrendMethod::Quadratic,
            fft_log_x: true,
            fft_log_y: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PlotMode {
    Timeseries,
    Fft,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DecimationMethod {
    None,
    Fpcs,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DetrendMethod {
    None,
    Mean,
    Linear,
    Quadratic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExportFormat {
    Csv,
    Hdf5,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone)]
struct ViewDataSeries {
    label: String,
    points: Vec<Point>,
}

#[derive(Debug, Clone, Copy)]
struct PlotEmitStats {
    mode: PlotMode,
    series_count: usize,
    point_count: usize,
    viewport_end: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
enum PlotPointSource<'a> {
    Slice(&'a [Point]),
    DequeRange {
        points: &'a VecDeque<Point>,
        start: usize,
        end: usize,
    },
}

impl<'a> PlotPointSource<'a> {
    fn len(&self) -> usize {
        match self {
            Self::Slice(points) => points.len(),
            Self::DequeRange { start, end, .. } => end.saturating_sub(*start),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Materialize the referenced points into an owned vector for a plot frame.
    fn collect(&self) -> Vec<Point> {
        match self {
            Self::Slice(points) => points.to_vec(),
            Self::DequeRange { points, start, end } => {
                (*start..*end).map(|index| points[index]).collect()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BinaryPlotSeries<'a> {
    key: &'a ColumnKeyDto,
    sample_rate: f64,
    points: PlotPointSource<'a>,
    /// True when this column has data, but none of it falls inside the pane's
    /// displayed time window (its time reference is not compatible with the
    /// pane's anchor stream). The series is sent with no points so the legend
    /// can flag it.
    outside_window: bool,
    /// Robust white-noise ASD estimate for FFT series, in `units/sqrt(Hz)`.
    noise_floor: Option<f64>,
}

struct PlotCadenceProfiler {
    enabled: bool,
    window_start: Instant,
    count: u64,
    total_emit_micros: u128,
    total_points: usize,
    last_viewport_end: Option<f64>,
}

impl PlotCadenceProfiler {
    fn new() -> Self {
        let enabled = profile_env_enabled("TWINLEAF_PLOT_PROFILE");

        Self {
            enabled,
            window_start: Instant::now(),
            count: 0,
            total_emit_micros: 0,
            total_points: 0,
            last_viewport_end: None,
        }
    }

    fn record(&mut self, elapsed: Duration, stats: PlotEmitStats) {
        if !self.enabled {
            return;
        }

        self.count += 1;
        self.total_emit_micros += elapsed.as_micros();
        self.total_points += stats.point_count;
        self.last_viewport_end = stats.viewport_end;

        let window = self.window_start.elapsed();
        if window < Duration::from_secs(1) {
            return;
        }

        let seconds = window.as_secs_f64();
        let rate = self.count as f64 / seconds;
        let avg_emit_ms = self.total_emit_micros as f64 / self.count as f64 / 1_000.0;
        let avg_points = self.total_points as f64 / self.count as f64;
        eprintln!(
            "[tio-bridge] profile plot.emit: {:.1}/s mode={:?} avg_emit={:.2} ms avg_points={:.0} last_series={} last_points={} last_viewport={}",
            rate,
            stats.mode,
            avg_emit_ms,
            avg_points,
            stats.series_count,
            stats.point_count,
            self.last_viewport_end
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|| "none".to_string())
        );

        self.window_start = Instant::now();
        self.count = 0;
        self.total_emit_micros = 0;
        self.total_points = 0;
    }
}

fn profile_env_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn bridge_debug_enabled() -> bool {
    [
        "TWINLEAF_BRIDGE_DEBUG",
        "TWINLEAF_CONSOLE_DEBUG",
        "TWINLEAF_DEBUG_LOGS",
        "TWINLEAF_DEBUG",
    ]
    .iter()
    .any(|name| profile_env_enabled(name))
}

struct StreamIngestProfiler {
    enabled: bool,
    window_start: Instant,
    samples: u64,
    columns: u64,
    active_columns: u64,
    total_process_micros: u128,
    total_display_micros: u128,
    last_states: usize,
    last_active: usize,
}

impl StreamIngestProfiler {
    fn new() -> Self {
        Self {
            enabled: profile_env_enabled("TWINLEAF_STREAM_PROFILE")
                || profile_env_enabled("TWINLEAF_PLOT_PROFILE"),
            window_start: Instant::now(),
            samples: 0,
            columns: 0,
            active_columns: 0,
            total_process_micros: 0,
            total_display_micros: 0,
            last_states: 0,
            last_active: 0,
        }
    }

    fn record(
        &mut self,
        elapsed: Duration,
        display_elapsed: Duration,
        columns: usize,
        active_columns: usize,
        states: usize,
        active: usize,
    ) {
        if !self.enabled {
            return;
        }

        self.samples += 1;
        self.columns += columns as u64;
        self.active_columns += active_columns as u64;
        self.total_process_micros += elapsed.as_micros();
        self.total_display_micros += display_elapsed.as_micros();
        self.last_states = states;
        self.last_active = active;

        let window = self.window_start.elapsed();
        if window < Duration::from_secs(1) {
            return;
        }

        let seconds = window.as_secs_f64();
        let sample_rate = self.samples as f64 / seconds;
        let column_rate = self.columns as f64 / seconds;
        let avg_process_ms = self.total_process_micros as f64 / self.samples as f64 / 1_000.0;
        let avg_display_us = self.total_display_micros as f64 / self.columns.max(1) as f64;
        let avg_columns = self.columns as f64 / self.samples.max(1) as f64;
        eprintln!(
            "[tio-bridge] profile sample.ingest: {:.1} samples/s {:.1} columns/s avg_sample={:.3} ms avg_display={:.2} us avg_columns={:.1} active_hits={} states={} active={}",
            sample_rate,
            column_rate,
            avg_process_ms,
            avg_display_us,
            avg_columns,
            self.active_columns,
            self.last_states,
            self.last_active
        );

        self.window_start = Instant::now();
        self.samples = 0;
        self.columns = 0;
        self.active_columns = 0;
        self.total_process_micros = 0;
        self.total_display_micros = 0;
    }
}

struct StreamValueEmitProfiler {
    enabled: bool,
    window_start: Instant,
    count: u64,
    total_emit_micros: u128,
    total_values: usize,
    last_values: usize,
    last_states: usize,
}

impl StreamValueEmitProfiler {
    fn new() -> Self {
        Self {
            enabled: profile_env_enabled("TWINLEAF_STREAM_PROFILE")
                || profile_env_enabled("TWINLEAF_PLOT_PROFILE"),
            window_start: Instant::now(),
            count: 0,
            total_emit_micros: 0,
            total_values: 0,
            last_values: 0,
            last_states: 0,
        }
    }

    fn record(&mut self, elapsed: Duration, values: usize, states: usize) {
        if !self.enabled {
            return;
        }

        self.count += 1;
        self.total_emit_micros += elapsed.as_micros();
        self.total_values += values;
        self.last_values = values;
        self.last_states = states;

        let window = self.window_start.elapsed();
        if window < Duration::from_secs(1) {
            return;
        }

        let seconds = window.as_secs_f64();
        let rate = self.count as f64 / seconds;
        let avg_emit_ms = self.total_emit_micros as f64 / self.count as f64 / 1_000.0;
        let avg_values = self.total_values as f64 / self.count.max(1) as f64;
        eprintln!(
            "[tio-bridge] profile streamValues.emit: {:.1}/s avg_emit={:.3} ms avg_values={:.1} last_values={} states={}",
            rate, avg_emit_ms, avg_values, self.last_values, self.last_states
        );

        self.window_start = Instant::now();
        self.count = 0;
        self.total_emit_micros = 0;
        self.total_values = 0;
    }
}

#[derive(Default)]
struct SessionLoopProfile {
    status_elapsed: Duration,
    command_elapsed: Duration,
    raw_log_elapsed: Duration,
    drain_elapsed: Duration,
    process_elapsed: Duration,
    event_elapsed: Duration,
    plot_elapsed: Duration,
    stream_value_elapsed: Duration,
    flush_elapsed: Duration,
    busy_elapsed: Duration,
    status_events: usize,
    commands: usize,
    raw_packets: usize,
    samples: usize,
    device_events: usize,
    plot_emits: usize,
    stream_values: usize,
    flushes: usize,
}

struct SessionLoopProfiler {
    enabled: bool,
    window_start: Instant,
    iterations: u64,
    total_busy_micros: u128,
    max_busy_micros: u128,
    total_status_micros: u128,
    total_command_micros: u128,
    total_raw_log_micros: u128,
    total_drain_micros: u128,
    total_process_micros: u128,
    total_event_micros: u128,
    total_plot_micros: u128,
    total_stream_value_micros: u128,
    total_flush_micros: u128,
    status_events: usize,
    commands: usize,
    raw_packets: usize,
    samples: usize,
    device_events: usize,
    plot_emits: usize,
    stream_values: usize,
    flushes: usize,
    last_states: usize,
    last_active: usize,
    last_retained_points: usize,
}

impl SessionLoopProfiler {
    fn new() -> Self {
        Self {
            enabled: profile_env_enabled("TWINLEAF_LOOP_PROFILE")
                || profile_env_enabled("TWINLEAF_STREAM_PROFILE")
                || profile_env_enabled("TWINLEAF_PLOT_PROFILE"),
            window_start: Instant::now(),
            iterations: 0,
            total_busy_micros: 0,
            max_busy_micros: 0,
            total_status_micros: 0,
            total_command_micros: 0,
            total_raw_log_micros: 0,
            total_drain_micros: 0,
            total_process_micros: 0,
            total_event_micros: 0,
            total_plot_micros: 0,
            total_stream_value_micros: 0,
            total_flush_micros: 0,
            status_events: 0,
            commands: 0,
            raw_packets: 0,
            samples: 0,
            device_events: 0,
            plot_emits: 0,
            stream_values: 0,
            flushes: 0,
            last_states: 0,
            last_active: 0,
            last_retained_points: 0,
        }
    }

    fn record(
        &mut self,
        sample: SessionLoopProfile,
        states: usize,
        active: usize,
        retained_points: usize,
    ) {
        if !self.enabled {
            return;
        }

        self.iterations += 1;
        self.total_busy_micros += sample.busy_elapsed.as_micros();
        self.max_busy_micros = self.max_busy_micros.max(sample.busy_elapsed.as_micros());
        self.total_status_micros += sample.status_elapsed.as_micros();
        self.total_command_micros += sample.command_elapsed.as_micros();
        self.total_raw_log_micros += sample.raw_log_elapsed.as_micros();
        self.total_drain_micros += sample.drain_elapsed.as_micros();
        self.total_process_micros += sample.process_elapsed.as_micros();
        self.total_event_micros += sample.event_elapsed.as_micros();
        self.total_plot_micros += sample.plot_elapsed.as_micros();
        self.total_stream_value_micros += sample.stream_value_elapsed.as_micros();
        self.total_flush_micros += sample.flush_elapsed.as_micros();
        self.status_events += sample.status_events;
        self.commands += sample.commands;
        self.raw_packets += sample.raw_packets;
        self.samples += sample.samples;
        self.device_events += sample.device_events;
        self.plot_emits += sample.plot_emits;
        self.stream_values += sample.stream_values;
        self.flushes += sample.flushes;
        self.last_states = states;
        self.last_active = active;
        self.last_retained_points = retained_points;

        let window = self.window_start.elapsed();
        if window < Duration::from_secs(1) {
            return;
        }

        let seconds = window.as_secs_f64();
        let loops_per_second = self.iterations as f64 / seconds;
        let divisor = self.iterations.max(1) as f64;
        let avg_ms = |micros: u128| micros as f64 / divisor / 1_000.0;
        eprintln!(
            "[tio-bridge] profile session.loop: {:.1}/s busy={:.3} ms max_busy={:.3} ms status={:.3} command={:.3} raw={:.3} drain={:.3} process={:.3} events={:.3} plot={:.3} values={:.3} flush={:.3} samples/s={:.1} raw_packets/s={:.1} plots={} value_updates={} states={} active={} retained_points={}",
            loops_per_second,
            avg_ms(self.total_busy_micros),
            self.max_busy_micros as f64 / 1_000.0,
            avg_ms(self.total_status_micros),
            avg_ms(self.total_command_micros),
            avg_ms(self.total_raw_log_micros),
            avg_ms(self.total_drain_micros),
            avg_ms(self.total_process_micros),
            avg_ms(self.total_event_micros),
            avg_ms(self.total_plot_micros),
            avg_ms(self.total_stream_value_micros),
            avg_ms(self.total_flush_micros),
            self.samples as f64 / seconds,
            self.raw_packets as f64 / seconds,
            self.plot_emits,
            self.stream_values,
            self.last_states,
            self.last_active,
            self.last_retained_points
        );

        *self = Self {
            enabled: self.enabled,
            window_start: Instant::now(),
            ..Self::new()
        };
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableDevice {
    url: String,
    label: String,
    kind: String,
    detail: String,
    routes: Vec<AvailableRoute>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableRoute {
    route: String,
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceDto {
    url: String,
    route: String,
    meta: DeviceMetaDto,
    streams: Vec<StreamDto>,
    rpcs: Vec<RpcDto>,
    #[serde(skip_serializing)]
    full_metadata: Option<DeviceFullMetadata>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceMetaDto {
    serial_number: String,
    firmware_hash: String,
    n_streams: usize,
    session_id: u32,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamDto {
    stream_id: u8,
    name: String,
    n_columns: usize,
    sample_size: usize,
    effective_sampling_rate: f64,
    columns: Vec<ColumnDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ColumnDto {
    key: ColumnKeyDto,
    name: String,
    units: String,
    data_type: String,
    description: String,
    display_value: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RpcDto {
    route: String,
    name: String,
    size: usize,
    permissions: String,
    arg_type: String,
    readable: bool,
    writable: bool,
    persistent: bool,
    unknown: bool,
    value: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlotSeries {
    key: ColumnKeyDto,
    label: String,
    units: String,
    sample_rate: f64,
    points: Vec<Point>,
    /// Robust white-noise ASD estimate for FFT series, in `units/sqrt(Hz)`.
    noise_floor: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamValueDto {
    key: ColumnKeyDto,
    value: f64,
}

#[derive(Debug)]
struct FftSeriesInput {
    key: ColumnKeyDto,
    label: String,
    units: String,
    sample_rate: f64,
    points: Vec<Point>,
}

#[derive(Debug)]
struct FftRequest {
    generation: u64,
    viewport_end: Option<f64>,
    window_seconds: f64,
    detrend: DetrendMethod,
    target_points: usize,
    inputs: Vec<FftSeriesInput>,
}

#[derive(Debug, Clone)]
struct FftResult {
    generation: u64,
    viewport_end: Option<f64>,
    window_seconds: f64,
    series: Vec<PlotSeries>,
}

struct FftWorker {
    request_tx: Sender<FftRequest>,
    result_rx: Receiver<FftResult>,
    generation: u64,
    in_flight: bool,
    latest: Option<FftResult>,
    latest_emitted: bool,
    needs_clear_emit: bool,
    last_request_at: Option<Instant>,
}

impl FftWorker {
    fn new(emitter: &EventSink) -> Self {
        let (request_tx, request_rx) = channel::bounded::<FftRequest>(1);
        let (result_tx, result_rx) = channel::unbounded::<FftResult>();
        let worker_emitter = emitter.clone();

        thread::Builder::new()
            .name("fft-worker".into())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let started = Instant::now();
                    let result = calculate_fft_request(request);
                    worker_emitter.debug(format!(
                        "FFT worker completed generation {} with {} series in {:.1} ms",
                        result.generation,
                        result.series.len(),
                        started.elapsed().as_secs_f64() * 1_000.0
                    ));
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn FFT worker");

        Self {
            request_tx,
            result_rx,
            generation: 0,
            in_flight: false,
            latest: None,
            latest_emitted: true,
            needs_clear_emit: false,
            last_request_at: None,
        }
    }

    fn poll(&mut self) {
        while let Ok(result) = self.result_rx.try_recv() {
            self.in_flight = false;
            if result.generation == self.generation {
                self.latest = Some(result);
                self.latest_emitted = false;
                self.needs_clear_emit = false;
            }
        }
    }

    fn is_ready_for_request(&mut self) -> bool {
        self.poll();

        if self.in_flight {
            return false;
        }

        if let Some(last_request_at) = self.last_request_at {
            if last_request_at.elapsed() < Duration::from_millis(66) {
                return false;
            }
        }

        true
    }

    fn submit_request(&mut self, request: FftRequest, emitter: &EventSink) {
        if request.inputs.is_empty() {
            self.latest = Some(FftResult {
                generation: self.generation,
                viewport_end: request.viewport_end,
                window_seconds: request.window_seconds,
                series: Vec::new(),
            });
            self.latest_emitted = false;
            self.needs_clear_emit = false;
            return;
        }

        match self.request_tx.try_send(request) {
            Ok(()) => {
                self.in_flight = true;
                self.last_request_at = Some(Instant::now());
            }
            Err(channel::TrySendError::Full(_)) => {
                self.in_flight = true;
            }
            Err(channel::TrySendError::Disconnected(_)) => {
                emitter.error("FFT worker stopped unexpectedly");
            }
        }
    }

    fn take_pending_emit(
        &mut self,
        viewport_end: Option<f64>,
        window_seconds: f64,
    ) -> Option<FftResult> {
        self.poll();

        if self.needs_clear_emit {
            self.needs_clear_emit = false;
            return Some(FftResult {
                generation: self.generation,
                viewport_end,
                window_seconds,
                series: Vec::new(),
            });
        }

        let result = self
            .latest
            .as_ref()
            .filter(|result| result.generation == self.generation)?
            .clone();
        if !same_fft_window_seconds(result.window_seconds, window_seconds) {
            return None;
        }
        if self.latest_emitted {
            return None;
        }

        self.latest_emitted = true;
        Some(result)
    }
}

fn same_fft_window_seconds(lhs: f64, rhs: f64) -> bool {
    let scale = lhs.abs().max(rhs.abs()).max(1.0);
    (lhs - rhs).abs() <= scale * 1e-9
}

#[derive(Debug, Clone)]
struct ColumnState {
    label: String,
    units: String,
    sample_rate: f64,
    raw: VecDeque<Point>,
    fpcs_by_pane: HashMap<usize, StreamingFpcs>,
    display_value: Option<f64>,
    display_value_x: Option<f64>,
}

impl ColumnState {
    fn new(label: String, units: String, sample_rate: f64) -> Self {
        Self {
            label,
            units,
            sample_rate,
            raw: VecDeque::new(),
            fpcs_by_pane: HashMap::new(),
            display_value: None,
            display_value_x: None,
        }
    }

    #[cfg(test)]
    fn push_raw(&mut self, point: Point, max_window_seconds: f64) {
        let _ = self.push_raw_profiled(point, max_window_seconds, false);
    }

    fn push_raw_profiled(
        &mut self,
        point: Point,
        max_window_seconds: f64,
        profile_display: bool,
    ) -> Duration {
        self.raw.push_back(point);
        let display_start = profile_display.then(Instant::now);
        self.update_display_value(point);
        let display_elapsed = display_start
            .map(|start| start.elapsed())
            .unwrap_or(Duration::ZERO);
        while let Some(front) = self.raw.front() {
            if point.x - front.x > max_window_seconds {
                self.raw.pop_front();
            } else {
                break;
            }
        }
        // Hard cap so a long window combined with a fast sample rate can't
        // grow the per-stream buffer without bound. Trims the oldest points
        // first — same direction as the time-based prune above.
        while self.raw.len() > MAX_RAW_POINTS_PER_STREAM {
            self.raw.pop_front();
        }
        display_elapsed
    }

    fn recent_points(&self, window_seconds: f64) -> Vec<Point> {
        let Some(last) = self.raw.back() else {
            return Vec::new();
        };
        let start = last.x - window_seconds;
        self.points_between(start, last.x)
    }

    fn fft_window_points(&self, window_seconds: f64, viewport_end: Option<f64>) -> Vec<Point> {
        let sample_count = self.fft_window_sample_count(window_seconds);
        let end_index = viewport_end
            .map(|end| self.upper_bound_x(end))
            .unwrap_or(self.raw.len());
        self.sample_window_ending_at_index(end_index, sample_count)
    }

    fn fft_window_sample_count(&self, window_seconds: f64) -> usize {
        if !window_seconds.is_finite() || !self.sample_rate.is_finite() || self.sample_rate <= 0.0 {
            return 0;
        }

        (window_seconds.max(1e-6) * self.sample_rate)
            .ceil()
            .max(1.0) as usize
    }

    fn sample_window_ending_at_index(&self, end_index: usize, sample_count: usize) -> Vec<Point> {
        if self.raw.is_empty() || sample_count == 0 || end_index == 0 {
            return Vec::new();
        }

        let end_index = end_index.min(self.raw.len());
        let start_index = end_index.saturating_sub(sample_count);
        (start_index..end_index)
            .map(|index| self.raw[index])
            .collect()
    }

    fn points_between(&self, start: f64, end: f64) -> Vec<Point> {
        if self.raw.is_empty() || start > end {
            return Vec::new();
        }

        let start_index = self.lower_bound_x(start);
        let end_index = self.upper_bound_x(end);
        if start_index >= end_index {
            return Vec::new();
        }

        (start_index..end_index)
            .map(|index| self.raw[index])
            .collect()
    }

    fn point_source_between(&self, start: f64, end: f64) -> PlotPointSource<'_> {
        if self.raw.is_empty() || start > end {
            return PlotPointSource::DequeRange {
                points: &self.raw,
                start: 0,
                end: 0,
            };
        }

        let start_index = self.lower_bound_x(start);
        let end_index = self.upper_bound_x(end);
        PlotPointSource::DequeRange {
            points: &self.raw,
            start: start_index,
            end: end_index.max(start_index),
        }
    }

    fn lower_bound_x(&self, target: f64) -> usize {
        lower_bound_point_deque(&self.raw, target)
    }

    fn upper_bound_x(&self, target: f64) -> usize {
        upper_bound_point_deque(&self.raw, target)
    }

    fn ensure_fpcs_configured(&mut self, pane_id: usize, view: &ViewConfig) {
        let ratio = fpcs_ratio(self.sample_rate, view);
        let capacity = target_plot_points(view).max(2);
        if self
            .fpcs_by_pane
            .get(&pane_id)
            .is_some_and(|fpcs| fpcs.ratio == ratio && fpcs.capacity == capacity)
        {
            return;
        }
        self.rebuild_fpcs(pane_id, view);
    }

    /// Rebuild the pane's FPCS decimator from the retained raw buffer,
    /// discarding any existing ring. The raw buffer is filled for every column
    /// regardless of selection, so rebuilding here keeps the decimated trace
    /// continuous when a column re-enters a pane. Without it, the ring frozen
    /// while the column was unselected would be stitched onto fresh points,
    /// leaving a gap across the unselected interval.
    fn rebuild_fpcs(&mut self, pane_id: usize, view: &ViewConfig) {
        let ratio = fpcs_ratio(self.sample_rate, view);
        let capacity = target_plot_points(view).max(2);
        let mut fpcs = StreamingFpcs::new(ratio, capacity);
        for point in self.recent_points(view.window_seconds) {
            fpcs.process_point(point);
        }
        self.fpcs_by_pane.insert(pane_id, fpcs);
    }

    fn process_fpcs_point(&mut self, pane_id: usize, view: &ViewConfig, point: Point) {
        self.ensure_fpcs_configured(pane_id, view);
        if let Some(fpcs) = self.fpcs_by_pane.get_mut(&pane_id) {
            fpcs.process_point(point);
        }
    }

    fn fpcs_for_pane(&self, pane_id: usize) -> Option<&StreamingFpcs> {
        self.fpcs_by_pane.get(&pane_id)
    }

    fn retain_fpcs_panes(&mut self, pane_ids: &HashSet<usize>) {
        self.fpcs_by_pane
            .retain(|pane_id, _| pane_ids.contains(pane_id));
    }

    fn update_display_value(&mut self, point: Point) {
        update_smoothed_display_value(&mut self.display_value, &mut self.display_value_x, point);
    }

    fn display_value_between(&self, start: f64, end: f64) -> Option<f64> {
        if self.raw.is_empty() || start > end {
            return None;
        }

        let start_index = self.lower_bound_x(start);
        let end_index = self.upper_bound_x(end);
        if start_index >= end_index {
            return None;
        }

        let mut display_value = None;
        let mut display_value_x = None;
        for index in start_index..end_index {
            update_smoothed_display_value(
                &mut display_value,
                &mut display_value_x,
                self.raw[index],
            );
        }
        display_value
    }
}

fn update_smoothed_display_value(
    display_value: &mut Option<f64>,
    display_value_x: &mut Option<f64>,
    point: Point,
) {
    if !point.y.is_finite() {
        return;
    }

    let alpha = if let (Some(previous_x), Some(_)) = (*display_value_x, *display_value) {
        let dt = (point.x - previous_x).max(0.0);
        (1.0 - (-dt / 0.25).exp()).clamp(0.02, 1.0)
    } else {
        1.0
    };

    *display_value = Some(match *display_value {
        Some(previous) => previous + alpha * (point.y - previous),
        None => point.y,
    });
    *display_value_x = Some(point.x);
}

fn lower_bound_point_deque(points: &VecDeque<Point>, target: f64) -> usize {
    let mut low = 0;
    let mut high = points.len();
    while low < high {
        let mid = low + (high - low) / 2;
        if points[mid].x < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

fn upper_bound_point_deque(points: &VecDeque<Point>, target: f64) -> usize {
    let mut low = 0;
    let mut high = points.len();
    while low < high {
        let mid = low + (high - low) / 2;
        if points[mid].x <= target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FpcsLastRetained {
    None,
    Max,
    Min,
}

#[derive(Debug, Clone)]
struct StreamingFpcs {
    ratio: usize,
    capacity: usize,
    output: VecDeque<Point>,
    counter: usize,
    potential_point: Option<Point>,
    last_retained_flag: FpcsLastRetained,
    window_max_point: Option<Point>,
    window_min_point: Option<Point>,
}

impl StreamingFpcs {
    fn new(ratio: usize, capacity: usize) -> Self {
        Self {
            ratio: ratio.max(1),
            capacity: capacity.max(2),
            output: VecDeque::new(),
            counter: 0,
            potential_point: None,
            last_retained_flag: FpcsLastRetained::None,
            window_max_point: None,
            window_min_point: None,
        }
    }

    fn retain_point(&mut self, point: Point) {
        while self.output.len() >= self.capacity {
            self.output.pop_front();
        }
        self.output.push_back(point);
    }

    fn process_point(&mut self, point: Point) {
        if self.window_max_point.is_none() {
            self.retain_point(point);
            self.window_max_point = Some(point);
            self.window_min_point = Some(point);
            self.counter = 1;
            return;
        }

        let mut max_point = self.window_max_point.unwrap();
        let mut min_point = self.window_min_point.unwrap();

        self.counter += 1;

        if point.y >= max_point.y {
            max_point = point;
        } else if point.y < min_point.y {
            min_point = point;
        }

        if self.counter >= self.ratio {
            if min_point.x < max_point.x {
                if self.last_retained_flag == FpcsLastRetained::Min
                    && self.potential_point != Some(min_point)
                {
                    if let Some(potential) = self.potential_point {
                        self.retain_point(potential);
                    }
                }
                self.retain_point(min_point);
                self.potential_point = Some(max_point);
                min_point = max_point;
                self.last_retained_flag = FpcsLastRetained::Min;
            } else {
                if self.last_retained_flag == FpcsLastRetained::Max
                    && self.potential_point != Some(max_point)
                {
                    if let Some(potential) = self.potential_point {
                        self.retain_point(potential);
                    }
                }
                self.retain_point(max_point);
                self.potential_point = Some(min_point);
                max_point = min_point;
                self.last_retained_flag = FpcsLastRetained::Max;
            }
            self.counter = 0;
        }

        self.window_max_point = Some(max_point);
        self.window_min_point = Some(min_point);
    }

    fn points_since(&self, start: f64) -> Vec<Point> {
        self.output
            .iter()
            .copied()
            .filter(|point| point.x >= start)
            .collect()
    }

    fn point_source_since(&self, start: f64) -> PlotPointSource<'_> {
        let start_index = lower_bound_point_deque(&self.output, start);
        PlotPointSource::DequeRange {
            points: &self.output,
            start: start_index,
            end: self.output.len(),
        }
    }
}

struct PacketLogger {
    writer: Option<BufWriter<File>>,
    path: Option<String>,
    packets_written: u64,
    bytes_written: u64,
    serialize_errors: u64,
}

impl PacketLogger {
    fn new(log_path: Option<String>, emitter: &EventSink) -> Self {
        let mut logger = Self {
            writer: None,
            path: None,
            packets_written: 0,
            bytes_written: 0,
            serialize_errors: 0,
        };
        if let Some(path) = log_path {
            logger.open(path, emitter);
        }
        logger
    }

    fn set_enabled(
        &mut self,
        enabled: bool,
        log_path: Option<String>,
        devices: &[DeviceDto],
        emitter: &EventSink,
    ) {
        if enabled {
            let Some(path) = log_path else {
                emitter.error("No log file path provided for logging");
                return;
            };
            if self.open(path, emitter) {
                self.write_startup_metadata(devices, emitter);
            }
        } else {
            self.disable(emitter);
        }
    }

    fn open(&mut self, path: String, emitter: &EventSink) -> bool {
        self.flush();
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                emitter.status("logging", format!("Writing Twinleaf packets to {path}"));
                self.writer = Some(BufWriter::new(file));
                self.path = Some(path);
                self.packets_written = 0;
                self.bytes_written = 0;
                self.serialize_errors = 0;
                true
            }
            Err(err) => {
                emitter.error(format!("Failed to open log file: {err}"));
                false
            }
        }
    }

    /// Move the active log to `new_path` and continue appending there — no gap
    /// and no duplicate metadata (the moved file keeps its original header).
    /// No-op if logging isn't currently active.
    fn relocate(&mut self, new_path: String, emitter: &EventSink) -> bool {
        if !self.is_enabled() {
            return false;
        }
        self.flush();
        let old = self.path.take();
        self.writer = None; // close the current file so it can be moved
        let moved = match &old {
            Some(old_path) if *old_path != new_path => std::fs::rename(old_path, &new_path)
                .or_else(|_| {
                    // rename fails across filesystems; fall back to copy + remove.
                    std::fs::copy(old_path, &new_path).and_then(|_| std::fs::remove_file(old_path))
                })
                .is_ok(),
            _ => true,
        };
        if !moved {
            emitter.error(format!("Failed to move log to {new_path}"));
            // Keep recording at the original location rather than dropping data.
            if let Some(old_path) = &old {
                if let Ok(file) = OpenOptions::new().create(true).append(true).open(old_path) {
                    self.writer = Some(BufWriter::new(file));
                    self.path = old.clone();
                }
            }
            return false;
        }
        match OpenOptions::new().create(true).append(true).open(&new_path) {
            Ok(file) => {
                self.writer = Some(BufWriter::new(file));
                self.path = Some(new_path.clone());
                emitter.status("logging", format!("Writing Twinleaf packets to {new_path}"));
                true
            }
            Err(err) => {
                emitter.error(format!("Failed to open log file: {err}"));
                false
            }
        }
    }

    fn disable(&mut self, emitter: &EventSink) {
        if !self.is_enabled() && self.path.is_none() {
            return;
        }
        self.flush();
        self.writer = None;
        self.path = None;
        self.packets_written = 0;
        self.bytes_written = 0;
        self.serialize_errors = 0;
        emitter.status("streaming", "Streaming without recording");
    }

    fn is_enabled(&self) -> bool {
        self.writer.is_some()
    }

    fn write_packet(&mut self, packet: tio::Packet, emitter: &EventSink) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        match packet.serialize() {
            Ok(raw) => {
                if writer.write_all(&raw).is_ok() {
                    let _ = writer.flush();
                    self.packets_written += 1;
                    self.bytes_written += raw.len() as u64;
                }
            }
            Err(()) => {
                self.serialize_errors += 1;
                if self.serialize_errors <= 3 {
                    emitter.error(format!(
                        "Failed to serialize packet for log: route={}, payload={}",
                        packet.routing,
                        payload_name(&packet.payload)
                    ));
                }
            }
        }
    }

    fn write_startup_metadata(&mut self, devices: &[DeviceDto], emitter: &EventSink) {
        if !self.is_enabled() {
            return;
        }

        let mut metadata_packets = 0usize;
        for device in devices {
            let Some(metadata) = &device.full_metadata else {
                emitter.debug(format!(
                    "startup log metadata unavailable for route {}",
                    device.route
                ));
                continue;
            };
            let route = match DeviceRoute::from_str(&device.route) {
                Ok(route) => route,
                Err(err) => {
                    emitter.error(format!(
                        "Failed to parse startup metadata route {}: {err:?}",
                        device.route
                    ));
                    continue;
                }
            };

            metadata_packets += self.write_metadata_packets(&route, metadata, emitter);
        }

        if metadata_packets > 0 {
            self.flush();
            emitter.debug(format!(
                "wrote {metadata_packets} startup metadata packet(s) to log"
            ));
        }
    }

    fn write_metadata_packets(
        &mut self,
        route: &DeviceRoute,
        metadata: &DeviceFullMetadata,
        emitter: &EventSink,
    ) -> usize {
        let before = self.packets_written;
        self.write_packet(
            metadata.device.make_update_with_route(route.clone()),
            emitter,
        );

        let mut stream_ids: Vec<u8> = metadata.streams.keys().copied().collect();
        stream_ids.sort_unstable();
        for stream_id in stream_ids {
            let Some(stream) = metadata.streams.get(&stream_id) else {
                continue;
            };
            self.write_packet(stream.stream.make_update_with_route(route.clone()), emitter);
            self.write_packet(
                stream.segment.make_update_with_route(route.clone()),
                emitter,
            );

            let mut columns = stream.columns.clone();
            columns.sort_by_key(|column| column.index);
            for column in columns {
                self.write_packet(column.make_update_with_route(route.clone()), emitter);
            }
        }

        (self.packets_written - before) as usize
    }

    fn flush(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
    }

    fn emit_progress(
        &self,
        emitter: &EventSink,
        start_seconds: Option<f64>,
        end_seconds: Option<f64>,
        time_reference_start: Option<f64>,
    ) {
        if !self.is_enabled() {
            return;
        }
        let elapsed_seconds = start_seconds
            .zip(end_seconds)
            .map(|(start, end)| (end - start).max(0.0));
        let file_bytes = self
            .path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .unwrap_or(self.bytes_written);

        emitter.emit(&json!({
            "type": "logProgress",
            "path": self.path,
            "packets": self.packets_written,
            "bytes": self.bytes_written,
            "fileBytes": file_bytes,
            "startSeconds": start_seconds,
            "endSeconds": end_seconds,
            "elapsedSeconds": elapsed_seconds,
            "timeReferenceStart": time_reference_start,
            "serializeErrors": self.serialize_errors
        }));
    }
}

struct PlaybackSession {
    devices: Vec<DeviceDto>,
    column_states: HashMap<ColumnKeyDto, ColumnState>,
    active_columns: HashSet<ColumnKeyDto>,
    view: ViewConfig,
    plot_panes: Vec<PlotPaneConfig>,
    range: Option<(f64, f64)>,
    recording_start: Option<f64>,
    time_reference_start: Option<f64>,
    position: f64,
}

impl PlaybackSession {
    fn new(
        devices: Vec<DeviceDto>,
        column_states: HashMap<ColumnKeyDto, ColumnState>,
        recording_start: Option<f64>,
        time_reference_start: Option<f64>,
    ) -> Self {
        // A log can contain pre-sync samples timestamped near 0 (recorded before
        // the device clock was set). Take the range over points at/after the
        // synced time reference so the scrubber spans the real data — starting at
        // the first real sample, not the (data-less) projected time of sample 0.
        let range = data_range_since(&column_states, time_reference_start)
            .or_else(|| data_range(&column_states));
        let position = range.map(|(_, end)| end).unwrap_or(0.0);
        let view = ViewConfig::default();
        Self {
            devices,
            column_states,
            active_columns: HashSet::new(),
            plot_panes: default_plot_panes(&view),
            view,
            range,
            recording_start,
            time_reference_start,
            position,
        }
    }

    fn set_position(&mut self, position: f64) {
        self.position = self.clamp_position(position);
    }

    fn set_active_columns(&mut self, columns: Vec<ColumnKeyDto>) {
        update_default_pane_columns(&mut self.plot_panes, columns, &self.view);
        self.active_columns = active_columns_for_panes(&self.plot_panes);
    }

    fn set_view(&mut self, view: ViewConfig) {
        self.view = view;
        apply_view_to_all_panes(&mut self.plot_panes, &self.view);
        self.active_columns = active_columns_for_panes(&self.plot_panes);
        self.position = self.clamp_position(self.position);
    }

    fn set_plot_panes(&mut self, panes: Vec<PlotPaneConfig>) {
        self.plot_panes = panes;
        self.active_columns = active_columns_for_panes(&self.plot_panes);
        self.position = self.clamp_position(self.position);
    }

    fn clamp_position(&self, position: f64) -> f64 {
        let Some((start, end)) = self.range else {
            return 0.0;
        };
        let window = self
            .plot_panes
            .iter()
            .map(|pane| pane.view.window_seconds)
            .fold(self.view.window_seconds, f64::max)
            .max(1e-6);
        let min_position = (start + window).min(end);
        position.clamp(min_position, end)
    }

    fn emit_state(&self, emitter: &EventSink) {
        let (start, end) = self.range.unwrap_or((0.0, 0.0));
        emitter.emit(&json!({
            "type": "playback",
            "start": start,
            "end": end,
            "position": self.position,
            "recordingStart": self.recording_start,
            "timeReferenceStart": self.time_reference_start
        }));
    }

    fn emit_plot(&self, emitter: &EventSink) {
        let _ = emit_plot_panes_at(
            emitter,
            &self.column_states,
            &self.plot_panes,
            Some(self.position),
        );
    }

    fn emit_stream_values(&self, emitter: &EventSink) {
        let _ = emit_stream_values_for_view(
            emitter,
            &self.column_states,
            self.position,
            self.stream_value_window_seconds(),
        );
    }

    fn stream_value_window_seconds(&self) -> f64 {
        self.plot_panes
            .iter()
            .filter(|pane| pane.view.mode == PlotMode::Timeseries)
            .map(|pane| pane.view.window_seconds)
            .chain(std::iter::once(self.view.window_seconds))
            .filter(|seconds| seconds.is_finite())
            .fold(0.0, f64::max)
            .max(1e-6)
    }
}

fn list_available_devices(include_all: bool) -> Vec<AvailableDevice> {
    let mut devices = vec![AvailableDevice {
        url: "tcp://localhost".to_string(),
        label: "Local tio-proxy".to_string(),
        kind: "tcp".to_string(),
        detail: "tcp://localhost:7855".to_string(),
        routes: Vec::new(),
    }];

    append_mdns_devices(&mut devices);
    append_serial_devices(&mut devices, include_all);

    devices
}

/// Browse the local network for Twinleaf devices advertised over mDNS and add
/// any that aren't already in the list. No-op when the `mdns` feature is off.
fn append_mdns_devices(devices: &mut Vec<AvailableDevice>) {
    #[cfg(feature = "mdns")]
    {
        let existing: HashSet<String> = devices.iter().map(|d| d.url.clone()).collect();
        for found in discovery::enumerate_mdns(MDNS_DISCOVERY_TIMEOUT) {
            if existing.contains(&found.url) {
                continue;
            }
            let scheme = found.url.split("://").next().unwrap_or("network");
            devices.push(AvailableDevice {
                label: found.name.clone().unwrap_or_else(|| found.url.clone()),
                kind: "network".to_string(),
                detail: format!(
                    "{} · {found_url}",
                    scheme.to_uppercase(),
                    found_url = found.url
                ),
                url: found.url,
                routes: Vec::new(),
            });
        }
    }
    #[cfg(not(feature = "mdns"))]
    {
        let _ = devices;
    }
}

#[cfg(feature = "serial")]
fn append_serial_devices(devices: &mut Vec<AvailableDevice>, include_all: bool) {
    let discovered = discovery::enumerate_serial(include_all);
    #[cfg(target_os = "macos")]
    let discovered_urls: HashSet<String> =
        discovered.iter().map(|device| device.url.clone()).collect();
    let mut probe_candidates = Vec::new();
    let mut unknown_devices = Vec::new();

    for device in discovered {
        #[cfg(target_os = "macos")]
        if should_skip_macos_tty_device(&device.url, &discovered_urls) {
            continue;
        }

        match &device.interface {
            PortInterface::Unknown(_, _) => unknown_devices.push(device),
            PortInterface::FTDI | PortInterface::STM32 => probe_candidates.push(device),
            // Network devices come from mDNS, not serial enumeration.
            PortInterface::Network => {}
        }
    }

    let probes: Vec<ListedDevice> = thread::scope(|scope| {
        let handles: Vec<_> = probe_candidates
            .into_iter()
            .map(|device| scope.spawn(move || probe_list_device(device.url, device.interface)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("device list probe thread panicked"))
            .collect()
    });

    devices.extend(probes.into_iter().map(available_device_from_probe));

    if include_all {
        devices.extend(unknown_devices.into_iter().map(|device| {
            let detail = interface_detail(&device.interface);
            AvailableDevice {
                url: device.url,
                label: "USB serial".to_string(),
                kind: "USB serial".to_string(),
                detail,
                routes: Vec::new(),
            }
        }));
    }
}

#[cfg(feature = "serial")]
struct ListedDevice {
    url: String,
    interface: PortInterface,
    status: ListedStatus,
}

#[cfg(feature = "serial")]
enum ListedStatus {
    Unreachable(String),
    Silent,
    Alive(Vec<ListedRoute>),
}

#[cfg(feature = "serial")]
struct ListedRoute {
    route: DeviceRoute,
    name: Option<String>,
}

#[cfg(feature = "serial")]
fn probe_list_device(url: String, interface: PortInterface) -> ListedDevice {
    let ifc = proxy::Interface::new_proxy(&url, Some(DEVICE_LIST_CONNECT_TIMEOUT), None);
    let port = match ifc.subtree_full(DeviceRoute::root()) {
        Ok(port) => port,
        Err(error) => {
            return ListedDevice {
                url,
                interface,
                status: ListedStatus::Unreachable(error.to_string()),
            };
        }
    };

    let mut seen: HashSet<DeviceRoute> = HashSet::new();
    let deadline = Instant::now() + DEVICE_LIST_DISCOVERY_WINDOW;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match port.receiver().recv_timeout(remaining) {
            Ok(packet) => {
                seen.insert(packet.routing);
            }
            Err(_) => break,
        }
    }

    if seen.is_empty() {
        return ListedDevice {
            url,
            interface,
            status: ListedStatus::Silent,
        };
    }

    let mut routes: Vec<DeviceRoute> = seen.into_iter().collect();
    routes.sort();
    let routes = routes
        .into_iter()
        .map(|route| {
            let name = ifc
                .new_port(
                    Some(DEVICE_LIST_NAME_RPC_TIMEOUT),
                    route.clone(),
                    0,
                    false,
                    false,
                )
                .ok()
                .and_then(|port| port.rpc::<(), String>("dev.name", ()).ok());
            ListedRoute { route, name }
        })
        .collect();

    ListedDevice {
        url,
        interface,
        status: ListedStatus::Alive(routes),
    }
}

#[cfg(feature = "serial")]
fn available_device_from_probe(device: ListedDevice) -> AvailableDevice {
    let kind = serial_device_kind(&device.interface).to_string();
    let interface = interface_detail(&device.interface);
    match device.status {
        ListedStatus::Unreachable(reason) => AvailableDevice {
            url: device.url,
            label: kind.clone(),
            kind,
            detail: format!("{interface}; unreachable: {reason}"),
            routes: Vec::new(),
        },
        ListedStatus::Silent => AvailableDevice {
            url: device.url,
            label: kind.clone(),
            kind,
            detail: format!("{interface}; silent"),
            routes: Vec::new(),
        },
        ListedStatus::Alive(routes) => {
            let root_name = routes
                .iter()
                .find(|route| route.route.len() == 0)
                .and_then(|route| route.name.as_deref())
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let routes: Vec<AvailableRoute> = routes
                .into_iter()
                .map(|route| AvailableRoute {
                    route: route_string(&route.route),
                    name: route.name,
                })
                .collect();
            let attached_count = routes.iter().filter(|route| route.route != "/").count();
            let attached_detail = match attached_count {
                0 => "no attached sensors".to_string(),
                1 => "1 attached sensor".to_string(),
                count => format!("{count} attached sensors"),
            };
            AvailableDevice {
                url: device.url,
                label: root_name.unwrap_or_else(|| kind.clone()),
                kind,
                detail: format!("{interface}; {attached_detail}"),
                routes,
            }
        }
    }
}

#[cfg(feature = "serial")]
fn serial_device_kind(interface: &PortInterface) -> &'static str {
    match interface {
        PortInterface::FTDI => "Twinleaf FTDI",
        PortInterface::STM32 => "Twinleaf STM32",
        PortInterface::Unknown(_, _) => "USB serial",
        PortInterface::Network => "Network",
    }
}

#[cfg(feature = "serial")]
fn route_string(route: &DeviceRoute) -> String {
    let route = route.to_string();
    if route.is_empty() {
        "/".to_string()
    } else {
        route
    }
}

#[cfg(all(feature = "serial", target_os = "macos"))]
fn should_skip_macos_tty_device(url: &str, discovered_urls: &HashSet<String>) -> bool {
    let Some(callout_url) = url
        .strip_prefix("serial:///dev/tty.")
        .map(|suffix| format!("serial:///dev/cu.{suffix}"))
    else {
        return false;
    };
    discovered_urls.contains(&callout_url)
}

#[cfg(feature = "serial")]
fn interface_detail(interface: &PortInterface) -> String {
    match interface {
        PortInterface::FTDI => "VID 0403, PID 6015".to_string(),
        PortInterface::STM32 => "VID 0483, PID 5740".to_string(),
        PortInterface::Unknown(vid, pid) => format!("VID {vid:04x}, PID {pid:04x}"),
        PortInterface::Network => "Network".to_string(),
    }
}

#[cfg(not(feature = "serial"))]
fn append_serial_devices(_devices: &mut Vec<AvailableDevice>, _include_all: bool) {}

/// Serve several sensors as one device tree on a loopback TCP port: sensor k
/// is mounted at route /k. The session connects to the returned URL and the
/// combined tree behaves exactly like a hub. The mount thread runs until the
/// session's connection closes (or any mounted sensor's proxy thread dies).
fn start_multi_sensor_mount(urls: Vec<String>, emitter: EventSink) -> Result<String, String> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|err| format!("could not bind a loopback port: {err}"))?;
    let loopback_port = listener
        .local_addr()
        .map_err(|err| format!("could not read the loopback address: {err}"))?
        .port();

    thread::Builder::new()
        .name("twinleaf-multi-mount".into())
        .spawn(move || run_multi_sensor_mount(listener, urls, emitter))
        .map_err(|err| format!("could not spawn the mount thread: {err}"))?;

    Ok(format!("tcp://127.0.0.1:{loopback_port}"))
}

fn run_multi_sensor_mount(listener: std::net::TcpListener, urls: Vec<String>, emitter: EventSink) {
    struct MountLink {
        prefix: DeviceRoute,
        interface: proxy::Interface,
        status_rx: Receiver<proxy::Event>,
    }

    let mut links = Vec::with_capacity(urls.len());
    for (index, sensor_url) in urls.iter().enumerate() {
        let Ok(prefix) = DeviceRoute::from_str(&format!("/{index}")) else {
            emitter.debug(format!("multi-mount: invalid route /{index}"));
            return;
        };
        emitter.debug(format!("multi-mount: {sensor_url} at {prefix}"));
        let (status_tx, status_rx) = channel::bounded(100);
        let interface = proxy::Interface::new_proxy(
            sensor_url,
            Some(CONNECTION_STARTUP_TIMEOUT),
            Some(status_tx),
        );
        links.push(MountLink {
            prefix,
            interface,
            status_rx,
        });
    }

    // Open each mount's port right away (before blocking on accept): a proxy
    // interface with no ports can shut down on an early connection failure,
    // after which `new_port` is refused.
    let mut ports = Vec::with_capacity(links.len());
    for link in &links {
        match link.interface.new_port(
            Some(Duration::from_millis(2000)),
            DeviceRoute::root(),
            usize::MAX,
            true,
            true,
        ) {
            Ok(port) => ports.push(port),
            Err(err) => {
                emitter.debug(format!(
                    "multi-mount: could not open a port for {}: {err:?}",
                    link.prefix
                ));
                return;
            }
        }
    }

    // The only client is this process's own session proxy.
    let Ok((stream, _)) = listener.accept() else {
        emitter.debug("multi-mount: accept failed");
        return;
    };
    drop(listener);

    let (rx_send, client_rx) =
        tio::port::Port::rx_channel_custom(proxy::Interface::get_client_tx_channel_size());
    let client = match tio::port::Port::from_tcp_stream_custom(
        stream,
        tio::port::Port::rx_to_channel(rx_send),
        proxy::Interface::get_client_rx_channel_size(),
    ) {
        Ok(client) => client,
        Err(err) => {
            emitter.debug(format!(
                "multi-mount: could not wrap the session stream: {err:?}"
            ));
            return;
        }
    };

    // Select slots: 0 is the session's traffic; then each link contributes
    // its sensor packets (slot 1 + 2i) and its proxy status (slot 2 + 2i).
    let mut sel = channel::Select::new();
    sel.recv(&client_rx);
    for (link, port) in links.iter().zip(&ports) {
        sel.recv(port.receiver());
        sel.recv(&link.status_rx);
    }

    let mut dropped_packets: u64 = 0;
    loop {
        let oper = sel.select();
        let slot = oper.index();
        if slot == 0 {
            let Ok(Ok(mut pkt)) = oper.recv(&client_rx) else {
                emitter.debug("multi-mount: session disconnected; shutting down");
                break;
            };
            let mut destination = None;
            for (link, port) in links.iter().zip(&ports) {
                if let Ok(relative) = link.prefix.relative_route(&pkt.routing) {
                    destination = Some((relative, port));
                    break;
                }
            }
            let Some((relative, port)) = destination else {
                // Root-addressed heartbeats keep each sensor's link alive
                // (devices expire clients that go quiet); broadcast them to
                // every mount, exactly as each sensor would receive over a
                // direct connection. Other root-addressed packets have no
                // single destination and are dropped.
                if pkt.routing.len() == 0 {
                    if let Payload::Heartbeat(_) = pkt.payload {
                        for port in &ports {
                            let _ = port.try_send(pkt.clone());
                        }
                    }
                }
                continue;
            };
            pkt.routing = relative;
            if port.try_send(pkt).is_err() {
                emitter.debug("multi-mount: forwarding to a sensor failed");
                break;
            }
        } else {
            let link_index = (slot - 1) / 2;
            let link = &links[link_index];
            if (slot - 1) % 2 == 0 {
                let Ok(mut pkt) = oper.recv(ports[link_index].receiver()) else {
                    emitter.debug(format!("multi-mount: lost the sensor at {}", link.prefix));
                    break;
                };
                pkt.routing = link.prefix.absolute_route(&pkt.routing);
                if pkt.routing.len() > tio::proto::TIO_PACKET_MAX_ROUTING_SIZE {
                    continue;
                }
                match client.try_send(pkt) {
                    Ok(()) => {}
                    Err(tio::SendError::Full) => {
                        dropped_packets += 1;
                        if dropped_packets.is_power_of_two() {
                            emitter.debug(format!(
                                "multi-mount: dropped {dropped_packets} packet(s); session is slow"
                            ));
                        }
                    }
                    Err(_) => {
                        emitter.debug("multi-mount: session connection closed");
                        break;
                    }
                }
            } else {
                let Ok(event) = oper.recv(&link.status_rx) else {
                    emitter.debug(format!("multi-mount: proxy ended at {}", link.prefix));
                    break;
                };
                emitter.debug(format!("multi-mount {}: {event:?}", link.prefix));
            }
        }
    }
}

// MARK: - Health diagnostics
//
// Per-stream timing/rate diagnostics equivalent to `tio health`. The math
// (incremental OLS for drift and rate, a ring-buffer jitter window) is ported
// from twinleaf-tools, which is a binary crate the bridge can't depend on.

/// Incremental ordinary-least-squares slope, anchored at the first point to
/// keep the sums small.
#[derive(Default)]
struct OnlineSlope {
    n: u64,
    sum_x: f64,
    sum_y: f64,
    sum_xx: f64,
    sum_xy: f64,
    x0: f64,
    y0: f64,
}

impl OnlineSlope {
    fn push(&mut self, x: f64, y: f64) {
        if self.n == 0 {
            self.x0 = x;
            self.y0 = y;
        }
        let dx = x - self.x0;
        let dy = y - self.y0;
        self.n += 1;
        self.sum_x += dx;
        self.sum_y += dy;
        self.sum_xx += dx * dx;
        self.sum_xy += dx * dy;
    }

    fn slope(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let denom = self.n as f64 * self.sum_xx - self.sum_x * self.sum_x;
        if denom.abs() < f64::EPSILON {
            return None;
        }
        Some((self.n as f64 * self.sum_xy - self.sum_x * self.sum_y) / denom)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Fixed-capacity ring of inter-sample timing deltas (ms); reports their stddev.
struct JitterWindow {
    buf: Vec<f64>,
    idx: usize,
    filled: bool,
}

impl JitterWindow {
    fn new(seconds: u64, hz_guess: f64) -> Self {
        let cap = ((seconds as f64 * hz_guess).round() as usize).max(16);
        Self {
            buf: vec![0.0; cap],
            idx: 0,
            filled: false,
        }
    }

    fn push(&mut self, value: f64) {
        self.buf[self.idx] = value;
        self.idx = (self.idx + 1) % self.buf.len();
        if self.idx == 0 {
            self.filled = true;
        }
    }

    fn std_ms(&self) -> f64 {
        let n = if self.filled {
            self.buf.len()
        } else {
            self.idx
        };
        if n == 0 {
            return 0.0;
        }
        let mean = self.buf[..n].iter().sum::<f64>() / n as f64;
        let var = self.buf[..n]
            .iter()
            .map(|x| (x - mean).powi(2))
            .sum::<f64>()
            / n as f64;
        var.sqrt()
    }
}

#[derive(Default)]
struct StreamHealth {
    name: String,
    host_epoch: Option<Instant>,
    drift_slope: OnlineSlope,
    drift_s: f64,
    ppm: f64,
    last_host: Option<Instant>,
    last_data: Option<f64>,
    jitter_ms: f64,
    jitter_window: Option<JitterWindow>,
    last_n: Option<u32>,
    samples_dropped: u64,
    session_id: Option<u32>,
    rate_slope: OnlineSlope,
    received_count: u64,
    rate_smps: f64,
    last_seen: Option<Instant>,
}

impl StreamHealth {
    fn on_sample(&mut self, sample_n: u32, t_data: f64, now: Instant) {
        let epoch = *self.host_epoch.get_or_insert(now);
        let host_time = now.duration_since(epoch).as_secs_f64();

        let window = self
            .jitter_window
            .get_or_insert_with(|| JitterWindow::new(HEALTH_JITTER_WINDOW_SECONDS, 100.0));
        if let (Some(last_host), Some(last_data)) = (self.last_host, self.last_data) {
            let dh = now.duration_since(last_host).as_secs_f64();
            let dd = t_data - last_data;
            window.push((dd - dh) * 1000.0);
            self.jitter_ms = window.std_ms();
        }
        self.last_host = Some(now);
        self.last_data = Some(t_data);

        self.drift_slope.push(host_time, t_data);
        if self.drift_slope.n >= HEALTH_MIN_DRIFT_SAMPLES {
            if let Some(beta) = self.drift_slope.slope() {
                let host_elapsed = host_time - self.drift_slope.x0;
                self.drift_s = (beta - 1.0) * host_elapsed;
                self.ppm = (beta - 1.0) * 1e6;
            }
        }

        self.received_count += 1;
        self.rate_slope.push(host_time, self.received_count as f64);
        if let Some(slope) = self.rate_slope.slope() {
            self.rate_smps = slope;
        }

        self.last_n = Some(sample_n);
    }

    fn reset_timing(&mut self) {
        self.drift_slope.reset();
        self.drift_s = 0.0;
        self.ppm = 0.0;
        self.last_host = None;
        self.last_data = None;
        self.jitter_ms = 0.0;
        self.jitter_window = None;
        self.last_n = None;
    }

    fn reset_for_new_session(&mut self, session_id: u32) {
        self.reset_timing();
        self.samples_dropped = 0;
        self.session_id = Some(session_id);
    }

    fn stale_threshold(&self) -> Duration {
        if self.rate_slope.n >= 2 && self.rate_smps > 0.0 {
            Duration::from_secs_f64(2.0 / self.rate_smps).max(HEALTH_STALE_FLOOR)
        } else {
            HEALTH_STALE_FLOOR
        }
    }

    fn is_stale(&self, now: Instant) -> bool {
        self.last_seen
            .map(|seen| now.duration_since(seen) > self.stale_threshold())
            .unwrap_or(true)
    }
}

#[derive(Default)]
struct HealthMonitor {
    streams: HashMap<(DeviceRoute, u8), StreamHealth>,
}

impl HealthMonitor {
    fn observe(&mut self, sample: &Sample, route: &DeviceRoute, now: Instant) {
        let key = (route.clone(), sample.stream.stream_id);
        let stats = self.streams.entry(key).or_insert_with(|| StreamHealth {
            name: sample.stream.name.clone(),
            session_id: Some(sample.device.session_id),
            ..Default::default()
        });
        stats.name = sample.stream.name.clone();

        if let Some(boundary) = &sample.boundary {
            match &boundary.reason {
                BoundaryReason::SessionChanged { new, .. } => stats.reset_for_new_session(*new),
                BoundaryReason::SamplesLost { expected, received } => {
                    stats.samples_dropped += u64::from(received.wrapping_sub(*expected));
                }
                _ => {}
            }
        }

        if stats.last_n.map(|n| sample.n != n).unwrap_or(true) {
            stats.last_seen = Some(now);
        }
        stats.on_sample(sample.n, sample.timestamp_end(), now);
    }

    /// Drop all streams for a route (device reset / disconnect).
    fn reset_route(&mut self, route: &DeviceRoute) {
        self.streams.retain(|(r, _), _| r != route);
    }

    fn snapshot(&self, now: Instant) -> Value {
        let mut entries: Vec<(&(DeviceRoute, u8), &StreamHealth)> = self.streams.iter().collect();
        entries.sort_by(|a, b| (a.0 .0.to_string(), a.0 .1).cmp(&(b.0 .0.to_string(), b.0 .1)));
        let streams: Vec<Value> = entries
            .into_iter()
            .map(|((route, stream_id), stats)| {
                json!({
                    "route": route.to_string(),
                    "streamId": stream_id,
                    "name": stats.name,
                    "rateHz": finite_or_null(stats.rate_smps),
                    "ppm": finite_or_null(stats.ppm),
                    "driftSeconds": finite_or_null(stats.drift_s),
                    "jitterMs": finite_or_null(stats.jitter_ms),
                    "received": stats.received_count,
                    "dropped": stats.samples_dropped,
                    "sessionId": stats.session_id,
                    "stale": stats.is_stale(now),
                })
            })
            .collect();
        json!({ "type": "health", "streams": streams })
    }
}

fn finite_or_null(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

fn run_session(
    url: String,
    route: Option<String>,
    log_path: Option<String>,
    command_rx: Receiver<SessionCommand>,
    emitter: EventSink,
) {
    let root_route = match route
        .as_deref()
        .unwrap_or("/")
        .parse::<String>()
        .ok()
        .and_then(|s| DeviceRoute::from_str(&s).ok())
    {
        Some(route) => route,
        None => {
            emitter.error("Invalid route");
            return;
        }
    };

    emitter.status("connecting", format!("Connecting to {url}"));
    emitter.debug(format!("session starting: url={url}, route={root_route}"));

    // Multiple whitespace-separated URLs are mounted at /0, /1, ... behind an
    // in-process loopback proxy, so the rest of the session sees one combined
    // device tree exactly as it would behind a hub.
    let sensor_urls: Vec<String> = url.split_whitespace().map(str::to_string).collect();
    let connect_url = if sensor_urls.len() > 1 {
        match start_multi_sensor_mount(sensor_urls, emitter.clone()) {
            Ok(loopback_url) => loopback_url,
            Err(err) => {
                emitter.error(format!("Could not start the multi-sensor mount: {err}"));
                return;
            }
        }
    } else {
        url.clone()
    };

    let (proxy_status_tx, proxy_status_rx) = channel::unbounded();
    let (status_tx, status_rx) = channel::unbounded();
    spawn_proxy_status_forwarder(url.clone(), proxy_status_rx, status_tx, emitter.clone());
    let proxy = Arc::new(proxy::Interface::new_proxy(
        &connect_url,
        Some(CONNECTION_STARTUP_TIMEOUT),
        Some(proxy_status_tx),
    ));

    let mut pending_startup_commands = VecDeque::new();
    let retry_transient_connect_failures = is_retryable_connect_url(&connect_url);
    if !wait_for_proxy_connection(
        &status_rx,
        &command_rx,
        &mut pending_startup_commands,
        &emitter,
        CONNECTION_STARTUP_TIMEOUT,
        retry_transient_connect_failures,
    ) {
        return;
    }

    let mut discovery = match discover_devices_until_available(
        &proxy,
        &url,
        &root_route,
        &command_rx,
        &mut pending_startup_commands,
        &emitter,
        CONNECTION_STARTUP_TIMEOUT,
    ) {
        Some(discovery) => discovery,
        None => return,
    };

    emitter.status(
        "metadata",
        format!("Loaded metadata for {} device(s)", discovery.devices.len()),
    );
    emit_metadata_devices(&emitter, &discovery.devices);

    let mut column_states = build_column_states(&discovery.devices);
    let mut rpc_index = build_rpc_index(&discovery.devices);
    let mut active_columns: HashSet<ColumnKeyDto> = HashSet::new();
    let mut view = ViewConfig::default();
    let mut plot_panes = default_plot_panes(&view);
    let mut fft_workers: HashMap<usize, FftWorker> = HashMap::new();
    let mut logger = PacketLogger::new(log_path, &emitter);
    logger.write_startup_metadata(&discovery.devices, &emitter);
    let mut plot_profiler = PlotCadenceProfiler::new();
    let mut ingest_profiler = StreamIngestProfiler::new();
    let mut stream_value_profiler = StreamValueEmitProfiler::new();
    let mut loop_profiler = SessionLoopProfiler::new();
    let packet_monitor_port = match proxy.subtree_full(root_route.clone()) {
        Ok(port) => Some(port),
        Err(err) => {
            emitter.error(format!("Failed to open packet monitor port: {err:?}"));
            None
        }
    };
    let mut device_tree = match DeviceTree::open(&proxy, root_route.clone()) {
        Ok(tree) => tree,
        Err(err) => {
            emitter.error(format!("Failed to open device tree: {err:?}"));
            return;
        }
    };

    emitter.status("streaming", format!("Streaming from {url}"));
    // Lazily check for available firmware upgrades on a background thread so
    // neither the network round-trip nor the per-device dev.desc RPC blocks the
    // streaming loop.
    spawn_upgrade_check(
        Arc::clone(&proxy),
        discovery.devices.clone(),
        emitter.clone(),
    );
    let mut last_plot_emit = Instant::now();
    let mut last_stream_value_emit = Instant::now();
    let mut health = HealthMonitor::default();
    let mut last_health_emit = Instant::now();
    let mut last_log_flush = Instant::now();
    let mut last_log_progress_packets = 0;
    let mut log_data_start: Option<f64> = None;
    let mut log_data_end: Option<f64> = None;
    let mut log_time_reference_start: Option<f64> = None;
    let mut latest_stream_timestamp_by_route: HashMap<DeviceRoute, f64> = HashMap::new();
    let mut last_rpc_metadata_recovery = Instant::now();
    let mut last_sample_number_by_stream: HashMap<(DeviceRoute, u8), u32> = HashMap::new();
    let mut last_reset_refresh_by_route: HashMap<DeviceRoute, Instant> = HashMap::new();

    loop {
        let loop_start = Instant::now();
        let mut loop_profile = SessionLoopProfile::default();

        let status_start = Instant::now();
        for status in status_rx.try_iter() {
            loop_profile.status_events += 1;
            emitter.emit(&json!({
                "type": "proxyEvent",
                "event": format!("{status:?}")
            }));
        }
        loop_profile.status_elapsed = status_start.elapsed();

        let command_start = Instant::now();
        while let Some(command) = next_session_command(&command_rx, &mut pending_startup_commands) {
            loop_profile.commands += 1;
            match command {
                SessionCommand::Stop => {
                    logger.flush();
                    emitter.status("disconnected", "Stopped streaming");
                    return;
                }
                SessionCommand::SetActiveColumns(columns) => {
                    update_default_pane_columns(&mut plot_panes, columns, &view);
                    active_columns = active_columns_for_panes(&plot_panes);
                    fft_workers.clear();
                    hydrate_fpcs(&mut column_states, &plot_panes, &emitter);
                    emit_active_columns(&emitter, &active_columns);
                }
                SessionCommand::SetLogging { enabled, log_path } => {
                    logger.set_enabled(enabled, log_path, &discovery.devices, &emitter);
                    if logger.is_enabled() {
                        logger.emit_progress(
                            &emitter,
                            log_data_start,
                            log_data_end,
                            log_time_reference_start,
                        );
                        last_log_progress_packets = logger.packets_written;
                    } else {
                        last_log_progress_packets = 0;
                    }
                }
                SessionCommand::RelocateLog { path } => {
                    if logger.relocate(path, &emitter) {
                        logger.emit_progress(
                            &emitter,
                            log_data_start,
                            log_data_end,
                            log_time_reference_start,
                        );
                        last_log_progress_packets = logger.packets_written;
                    }
                }
                SessionCommand::SetView(new_view) => {
                    view = new_view;
                    apply_view_to_all_panes(&mut plot_panes, &view);
                    active_columns = active_columns_for_panes(&plot_panes);
                    fft_workers.clear();
                    hydrate_fpcs(&mut column_states, &plot_panes, &emitter);
                }
                SessionCommand::SetPlotPanes(panes) => {
                    plot_panes = panes;
                    active_columns = active_columns_for_panes(&plot_panes);
                    fft_workers.clear();
                    hydrate_fpcs(&mut column_states, &plot_panes, &emitter);
                    emit_active_columns(&emitter, &active_columns);
                }
                SessionCommand::CallRpc {
                    request_id,
                    route,
                    name,
                    arg,
                } => dispatch_rpc(&proxy, &rpc_index, request_id, route, name, arg, &emitter),
                SessionCommand::CopyViewData {
                    request_id,
                    pane_id,
                    viewport_end,
                } => {
                    let pane = plot_panes
                        .iter()
                        .find(|pane| Some(pane.id) == pane_id)
                        .or_else(|| plot_panes.first());
                    let (columns, view) = pane
                        .map(|pane| (pane_columns_set(pane), pane.view.clone()))
                        .unwrap_or_else(|| (active_columns.clone(), view.clone()));
                    let text = build_view_data_tsv(&column_states, &columns, &view, viewport_end);
                    emit_view_data(&emitter, request_id, Ok(text));
                }
                SessionCommand::CheckUpgrade => {
                    spawn_upgrade_check(
                        Arc::clone(&proxy),
                        discovery.devices.clone(),
                        emitter.clone(),
                    );
                }
                SessionCommand::PerformUpgrade { route } => {
                    spawn_upgrade_flash(Arc::clone(&proxy), route, emitter.clone());
                }
            }
        }
        loop_profile.command_elapsed = command_start.elapsed();

        if !discovery.incomplete_rpc_routes.is_empty()
            && last_rpc_metadata_recovery.elapsed() >= RPC_METADATA_RECOVERY_INTERVAL
        {
            if recover_incomplete_rpc_metadata(&proxy, &mut discovery, &emitter) {
                rpc_index = build_rpc_index(&discovery.devices);
                emit_metadata_devices(&emitter, &discovery.devices);
            }
            last_rpc_metadata_recovery = Instant::now();
        }

        let drain_start = Instant::now();
        let drain_result = panic::catch_unwind(AssertUnwindSafe(|| device_tree.drain()));
        loop_profile.drain_elapsed = drain_start.elapsed();
        match drain_result {
            Ok(Ok(samples)) => {
                loop_profile.samples = samples.len();
                let process_start = Instant::now();
                let sample_now = Instant::now();
                let mut reset_routes: HashSet<DeviceRoute> = HashSet::new();
                for (sample, sample_route) in samples {
                    health.observe(&sample, &sample_route, sample_now);
                    // A sample counter that goes backward means the device
                    // rebooted (its counter restarted). Note the route so the
                    // stale history gets flushed after this batch.
                    let stream_key = (sample_route.clone(), sample.stream.stream_id);
                    if let Some(&last_n) = last_sample_number_by_stream.get(&stream_key) {
                        if sample.n < last_n {
                            reset_routes.insert(sample_route.clone());
                        }
                    }
                    last_sample_number_by_stream.insert(stream_key, sample.n);

                    let sample_start = sample.timestamp_begin();
                    let sample_end = sample.timestamp_end();
                    if sample_start.is_finite() {
                        log_data_start = Some(
                            log_data_start
                                .map(|start| start.min(sample_start))
                                .unwrap_or(sample_start),
                        );
                    }
                    if sample_end.is_finite() {
                        log_data_end = Some(
                            log_data_end
                                .map(|end| end.max(sample_end))
                                .unwrap_or(sample_end),
                        );
                        latest_stream_timestamp_by_route.insert(sample_route.clone(), sample_end);
                    }
                    log_time_reference_start = earliest_time(
                        log_time_reference_start,
                        sample_time_reference_start(&sample),
                    );
                    process_sample(
                        &sample,
                        &sample_route,
                        &mut column_states,
                        &active_columns,
                        &plot_panes,
                        live_retention_seconds_for_panes(&plot_panes, &view),
                        Some(&mut ingest_profiler),
                    );
                }
                loop_profile.process_elapsed = process_start.elapsed();

                for route in reset_routes {
                    emitter.debug(format!(
                        "sample number went backward on {route}; treating as device reset"
                    ));
                    flush_route_history(&route.to_string(), &mut column_states);
                    fft_workers.clear();
                    health.reset_route(&route);
                    // Drop baselines for the route's other streams so their
                    // first post-reset sample doesn't re-trigger.
                    last_sample_number_by_stream.retain(|(r, _), _| r != &route);

                    let refresh_due = last_reset_refresh_by_route
                        .get(&route)
                        .is_none_or(|at| at.elapsed() >= DEVICE_RESET_REFRESH_COOLDOWN);
                    if refresh_due {
                        last_reset_refresh_by_route.insert(route.clone(), Instant::now());
                        emitter.status(
                            "metadata",
                            format!("Device on {route} restarted; reloading settings"),
                        );
                        if refresh_route_metadata(
                            &proxy,
                            &url,
                            &route,
                            &root_route,
                            &emitter,
                            &mut discovery,
                            &mut column_states,
                            &mut rpc_index,
                        ) {
                            emit_metadata_devices(&emitter, &discovery.devices);
                        }
                        emitter.status("streaming", format!("Streaming from {url}"));
                    }
                }
            }
            Ok(Err(err)) => {
                emitter.error(format!("Twinleaf stream error: {err:?}"));
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) => {
                emitter.error(format!(
                    "Twinleaf parser panic while draining stream data: {}",
                    panic_message(err)
                ));
                thread::sleep(Duration::from_millis(250));
            }
        }

        if let Some(port) = &packet_monitor_port {
            let raw_log_start = Instant::now();
            loop_profile.raw_packets = drain_packet_monitor_port(
                port,
                &root_route,
                &mut logger,
                &latest_stream_timestamp_by_route,
                &emitter,
            );
            loop_profile.raw_log_elapsed = raw_log_start.elapsed();
        }

        let event_start = Instant::now();
        let device_events = device_tree.drain_events();
        loop_profile.device_events = device_events.len();
        let mut new_routes: Vec<DeviceRoute> = Vec::new();
        for event in device_events {
            match event {
                TreeEvent::Device { route, event } => {
                    if let DeviceEvent::RpcInvalidated(method) = &event {
                        emit_rpc_invalidated(&emitter, &route, method);
                    }
                    emitter.emit(&json!({
                        "type": "deviceEvent",
                        "route": route.to_string(),
                        "event": format!("{event:?}")
                    }));
                }
                TreeEvent::RouteDiscovered(route) => {
                    new_routes.push(route);
                }
            }
        }
        let mut devices_changed = false;
        for route in new_routes {
            if add_late_discovered_route(
                &proxy,
                &url,
                &route,
                &root_route,
                &emitter,
                &mut discovery,
                &mut column_states,
                &mut rpc_index,
            ) {
                devices_changed = true;
            }
        }
        if devices_changed {
            emit_metadata_devices(&emitter, &discovery.devices);
        }
        loop_profile.event_elapsed = event_start.elapsed();

        if last_plot_emit.elapsed() >= Duration::from_millis(33) {
            let emit_start = Instant::now();
            let stats = emit_live_plot(
                &emitter,
                &column_states,
                &active_columns,
                &plot_panes,
                &mut fft_workers,
            );
            loop_profile.plot_elapsed = emit_start.elapsed();
            if let Some(stats) = stats {
                plot_profiler.record(emit_start.elapsed(), stats);
                loop_profile.plot_emits = 1;
            }
            last_plot_emit = Instant::now();
        }

        if last_stream_value_emit.elapsed() >= Duration::from_millis(100) {
            let emit_start = Instant::now();
            let value_count = emit_stream_values(&emitter, &column_states);
            loop_profile.stream_values = value_count;
            loop_profile.stream_value_elapsed = emit_start.elapsed();
            stream_value_profiler.record(emit_start.elapsed(), value_count, column_states.len());
            last_stream_value_emit = Instant::now();
        }

        if last_health_emit.elapsed() >= HEALTH_EMIT_INTERVAL {
            emitter.emit(&health.snapshot(Instant::now()));
            last_health_emit = Instant::now();
        }

        if last_log_flush.elapsed() >= Duration::from_secs(1) {
            let flush_start = Instant::now();
            logger.flush();
            if logger.packets_written != last_log_progress_packets {
                logger.emit_progress(
                    &emitter,
                    log_data_start,
                    log_data_end,
                    log_time_reference_start,
                );
                last_log_progress_packets = logger.packets_written;
            }
            loop_profile.flushes = 1;
            loop_profile.flush_elapsed = flush_start.elapsed();
            last_log_flush = Instant::now();
        }

        loop_profile.busy_elapsed = loop_start.elapsed();
        loop_profiler.record(
            loop_profile,
            column_states.len(),
            active_columns.len(),
            retained_point_count(&column_states),
        );

        thread::sleep(Duration::from_millis(10));
    }
}

fn emit_rpc_invalidated(emitter: &EventSink, route: &DeviceRoute, method: &RpcMethod) {
    match method {
        RpcMethod::Name(name) => emitter.emit(&json!({
            "type": "rpcInvalidated",
            "route": route.to_string(),
            "name": name
        })),
        RpcMethod::Id(id) => emitter.emit(&json!({
            "type": "rpcInvalidated",
            "route": route.to_string(),
            "rpcId": id
        })),
    }
}

fn emit_metadata_devices(emitter: &EventSink, devices: &[DeviceDto]) {
    emitter.emit(&json!({
        "type": "metadata",
        "devices": devices
    }));
}

/// Drop all buffered plot history for a route. Used when the device resets:
/// pre-reset points would otherwise bridge the discontinuity on screen. The
/// FPCS state rebuilds lazily from the (now empty) raw buffer on the next
/// sample.
fn flush_route_history(route: &str, column_states: &mut HashMap<ColumnKeyDto, ColumnState>) {
    for (key, state) in column_states.iter_mut() {
        if key.route == route {
            state.raw.clear();
            state.fpcs_by_pane.clear();
            state.display_value = None;
            state.display_value_x = None;
        }
    }
}

/// Re-fetch stream metadata and the RPC list for a route whose device
/// restarted, replacing its entry in the running session state. Returns true
/// when the refreshed device was applied (the caller then re-emits `metadata`,
/// which also prompts the app to reload all readable RPC values).
fn refresh_route_metadata(
    proxy: &Arc<proxy::Interface>,
    url: &str,
    route: &DeviceRoute,
    root_route: &DeviceRoute,
    emitter: &EventSink,
    discovery: &mut DeviceDiscoveryResult,
    column_states: &mut HashMap<ColumnKeyDto, ColumnState>,
    rpc_index: &mut HashMap<(String, String), RpcDto>,
) -> bool {
    let fetched = match panic::catch_unwind(AssertUnwindSafe(|| {
        fetch_device(proxy, url, route, emitter, false)
    })) {
        Ok(Ok(fetched)) => fetched,
        Ok(Err(err)) => {
            emitter.debug(format!("post-reset refresh of {route} failed: {err}"));
            return false;
        }
        Err(err) => {
            emitter.debug(format!(
                "post-reset refresh of {route} panicked: {}",
                panic_message(err)
            ));
            return false;
        }
    };

    let route_str = route.to_string();
    if fetched.rpc_metadata_complete {
        discovery.incomplete_rpc_routes.remove(route);
    } else {
        discovery.incomplete_rpc_routes.insert(route.clone());
    }

    for stream in &fetched.device.streams {
        for column in &stream.columns {
            let label = format!(
                "{} {}.{}",
                fetched.device.meta.name, stream.name, column.name
            );
            column_states.entry(column.key.clone()).or_insert_with(|| {
                ColumnState::new(label, column.units.clone(), stream.effective_sampling_rate)
            });
        }
    }

    rpc_index.retain(|(r, _), _| r != &route_str);
    for rpc in &fetched.device.rpcs {
        rpc_index.insert(
            (fetched.device.route.clone(), rpc.name.clone()),
            rpc.clone(),
        );
    }

    if let Some(existing) = discovery
        .devices
        .iter_mut()
        .find(|device| device.route == route_str)
    {
        *existing = fetched.device;
    } else {
        discovery.devices.push(fetched.device);
        sort_devices_leaf_first(&mut discovery.devices, Some(root_route));
    }
    true
}

#[cfg(not(feature = "firmware"))]
fn spawn_upgrade_check(
    _proxy: Arc<proxy::Interface>,
    _devices: Vec<DeviceDto>,
    emitter: EventSink,
) {
    emitter.debug("firmware upgrade support not compiled in");
}

#[cfg(not(feature = "firmware"))]
fn spawn_upgrade_flash(_proxy: Arc<proxy::Interface>, _route: String, emitter: EventSink) {
    emitter.debug("firmware upgrade support not compiled in");
}

#[cfg(feature = "firmware")]
fn installed_version_text(installed: &firmware::InstalledFirmware) -> String {
    match (&installed.build_date, &installed.hash) {
        (Some(date), Some(hash)) => format!("{date} ({hash})"),
        (Some(date), None) => date.to_string(),
        (None, Some(hash)) => hash.clone(),
        (None, None) => installed.description.clone(),
    }
}

/// Background firmware-availability check. For each discovered device route,
/// queries the installed firmware (`dev.desc`) and compares it against the
/// published GitHub catalog. Emits a single `upgradeStatus` event listing the
/// devices that have a strictly-newer release available. Runs on its own thread
/// (network + RPC) so the streaming loop is never blocked.
#[cfg(feature = "firmware")]
fn spawn_upgrade_check(proxy: Arc<proxy::Interface>, devices: Vec<DeviceDto>, emitter: EventSink) {
    let spawn_error_emitter = emitter.clone();
    let spawned = thread::Builder::new()
        .name("twinleaf-upgrade-check".into())
        .spawn(move || {
            let catalog = firmware::github::GithubCatalog::twinleaf();
            let mut available = Vec::new();

            for device in &devices {
                let Ok(route) = DeviceRoute::from_str(&device.route) else {
                    continue;
                };
                let Ok(port) = proxy.device_rpc(route) else {
                    continue;
                };
                let installed = match firmware::query_installed(&port) {
                    Ok(installed) => installed,
                    Err(err) => {
                        emitter.debug(format!(
                            "upgrade check: dev.desc failed for {}: {err}",
                            device.route
                        ));
                        continue;
                    }
                };
                match firmware::check_for_update(installed.clone(), &catalog) {
                    Ok(report) if report.status == UpdateStatus::UpdateAvailable => {
                        if let Some(latest) = report.latest {
                            available.push(json!({
                                "route": device.route,
                                "deviceName": if device.meta.name.is_empty() {
                                    device.route.clone()
                                } else {
                                    device.meta.name.clone()
                                },
                                "currentVersion": installed_version_text(&installed),
                                "newVersion": latest.date.to_string(),
                                "newHash": latest.short_hash,
                                "filename": latest.filename,
                            }));
                        }
                    }
                    Ok(_) => {}
                    Err(err) => emitter.debug(format!(
                        "upgrade check: comparison failed for {}: {err}",
                        device.route
                    )),
                }
            }

            emitter.emit(&json!({
                "type": "upgradeStatus",
                "available": available,
            }));
        });
    if let Err(err) = spawned {
        spawn_error_emitter.debug(format!("failed to spawn upgrade-check thread: {err}"));
    }
}

/// Background firmware flash for a single device route. Re-derives the latest
/// release, downloads it (cached), and flashes — emitting `upgradeProgress`
/// events for every phase so the UI can show detailed progress. Runs on its own
/// thread because `firmware::flash` blocks for the full upload + settle period.
#[cfg(feature = "firmware")]
fn spawn_upgrade_flash(proxy: Arc<proxy::Interface>, route: String, emitter: EventSink) {
    let spawn_error_emitter = emitter.clone();
    let spawned = thread::Builder::new()
        .name("twinleaf-upgrade-flash".into())
        .spawn(move || {
            let progress = |phase: &str, body: Value| {
                let mut event = json!({
                    "type": "upgradeProgress",
                    "route": route,
                    "phase": phase,
                });
                if let (Value::Object(map), Value::Object(extra)) = (&mut event, body) {
                    map.extend(extra);
                }
                emitter.emit(&event);
            };

            match run_upgrade_flash(&proxy, &route, &progress) {
                Ok(()) => progress(
                    "complete",
                    json!({ "message": "Firmware updated. Power-cycle the device." }),
                ),
                Err(message) => progress("error", json!({ "message": message })),
            }
        });
    if let Err(err) = spawned {
        spawn_error_emitter.debug(format!("failed to spawn upgrade-flash thread: {err}"));
    }
}

#[cfg(feature = "firmware")]
fn run_upgrade_flash(
    proxy: &Arc<proxy::Interface>,
    route: &str,
    progress: &dyn Fn(&str, Value),
) -> Result<(), String> {
    let device_route =
        DeviceRoute::from_str(route).map_err(|_| format!("invalid device route: {route}"))?;
    let port = proxy
        .device_rpc(device_route)
        .map_err(|err| format!("could not open device: {err:?}"))?;

    let installed = firmware::query_installed(&port)
        .map_err(|err| format!("could not read firmware: {err}"))?;
    let catalog = firmware::github::GithubCatalog::twinleaf();
    let report = firmware::check_for_update(installed, &catalog)
        .map_err(|err| format!("could not check for update: {err}"))?;
    let release = report
        .latest
        .ok_or_else(|| "no published firmware available for this device".to_string())?;

    progress(
        "downloading",
        json!({ "newVersion": release.date.to_string(), "filename": release.filename }),
    );
    let firmware_data = match firmware::default_cache_dir() {
        Some(cache_root) => firmware::download_cached(&catalog, &release, &cache_root),
        None => catalog.download(&release),
    }
    .map_err(|err| format!("download failed: {err}"))?;

    firmware::flash(&port, &firmware_data, |event| match event {
        FlashEvent::Stopping => progress("stopping", json!({ "message": "Stopping device" })),
        FlashEvent::Stopped(outcome) => {
            let message = match outcome {
                StopOutcome::Stopped => "Device stopped",
                StopOutcome::AlreadyStopped => "Device already stopped",
                StopOutcome::Unsupported => "Device has no stop command; continuing",
            };
            progress("stopped", json!({ "message": message }));
        }
        FlashEvent::Uploading { chunk, total } => progress(
            "uploading",
            json!({
                "chunk": chunk,
                "total": total,
                "fraction": if total > 0 { chunk as f64 / total as f64 } else { 0.0 },
                "message": format!("Uploading firmware {chunk}/{total}"),
            }),
        ),
        FlashEvent::Committing => {
            progress("committing", json!({ "message": "Committing upgrade" }))
        }
        FlashEvent::Finalizing => progress(
            "finalizing",
            json!({ "message": "Finalizing — keeping the device powered" }),
        ),
        FlashEvent::Complete => {}
    })
    .map_err(|err| format!("flash failed: {err}"))?;

    progress(
        "complete",
        json!({ "message": "Upgrade complete", "fraction": 1.0 }),
    );
    Ok(())
}

/// Fold a route that the `DeviceTree` flagged as newly seen mid-session into
/// the running state — `discovery.devices` (so the sidebar updates),
/// `column_states` (so streams get plotted), and `rpc_index` (so RPCs route
/// correctly). Returns true if a new device was actually added.
///
/// Skips routes we already know about (the root route and the initially-
/// discovered children re-fire `RouteDiscovered` as packets flow). Failures
/// to fetch metadata are logged at debug only — a late-arriving device that
/// can't be reached shouldn't spam the user with errors.
fn add_late_discovered_route(
    proxy: &Arc<proxy::Interface>,
    url: &str,
    route: &DeviceRoute,
    root_route: &DeviceRoute,
    emitter: &EventSink,
    discovery: &mut DeviceDiscoveryResult,
    column_states: &mut HashMap<ColumnKeyDto, ColumnState>,
    rpc_index: &mut HashMap<(String, String), RpcDto>,
) -> bool {
    let route_str = route.to_string();
    if discovery.devices.iter().any(|d| d.route == route_str) {
        return false;
    }

    emitter.debug(format!(
        "route {route} appeared mid-session; fetching metadata"
    ));
    emitter.status(
        "metadata",
        format!("Discovering new device on route {route}"),
    );
    let fetched = match panic::catch_unwind(AssertUnwindSafe(|| {
        fetch_device(proxy, url, route, emitter, false)
    })) {
        Ok(Ok(fetched)) => fetched,
        Ok(Err(err)) => {
            emitter.debug(format!("late discovery of {route} skipped: {err}"));
            return false;
        }
        Err(err) => {
            emitter.debug(format!(
                "late discovery of {route} panicked: {}",
                panic_message(err)
            ));
            return false;
        }
    };

    if !fetched.rpc_metadata_complete {
        discovery.incomplete_rpc_routes.insert(route.clone());
    }

    for stream in &fetched.device.streams {
        for column in &stream.columns {
            let label = format!(
                "{} {}.{}",
                fetched.device.meta.name, stream.name, column.name
            );
            column_states.entry(column.key.clone()).or_insert_with(|| {
                ColumnState::new(label, column.units.clone(), stream.effective_sampling_rate)
            });
        }
    }
    for rpc in &fetched.device.rpcs {
        rpc_index.insert(
            (fetched.device.route.clone(), rpc.name.clone()),
            rpc.clone(),
        );
    }
    discovery.devices.push(fetched.device);
    sort_devices_leaf_first(&mut discovery.devices, Some(root_route));
    true
}

fn recover_incomplete_rpc_metadata(
    proxy: &Arc<proxy::Interface>,
    discovery: &mut DeviceDiscoveryResult,
    emitter: &EventSink,
) -> bool {
    let routes: Vec<DeviceRoute> = discovery.incomplete_rpc_routes.iter().cloned().collect();
    let mut metadata_changed = false;

    for route in routes {
        let route_text = route.to_string();
        let Some(device_index) = discovery
            .devices
            .iter()
            .position(|device| device.route == route_text)
        else {
            discovery.incomplete_rpc_routes.remove(&route);
            continue;
        };

        let Ok(port) = proxy.device_rpc(route.clone()) else {
            emitter.debug(format!(
                "RPC metadata recovery could not open RPC port for route {route}"
            ));
            continue;
        };

        let RpcFetchResult { rpcs, complete } = fetch_rpcs(&port, &route, emitter);
        if !rpc_lists_equivalent(&discovery.devices[device_index].rpcs, &rpcs) {
            discovery.devices[device_index].rpcs = rpcs;
            metadata_changed = true;
        }

        if complete {
            discovery.incomplete_rpc_routes.remove(&route);
            emitter.debug(format!("RPC metadata recovery complete for route {route}"));
        }
    }

    metadata_changed
}

fn rpc_lists_equivalent(lhs: &[RpcDto], rhs: &[RpcDto]) -> bool {
    lhs.len() == rhs.len()
        && lhs.iter().zip(rhs.iter()).all(|(left, right)| {
            left.name == right.name
                && left.size == right.size
                && left.permissions == right.permissions
                && left.arg_type == right.arg_type
                && left.readable == right.readable
                && left.writable == right.writable
                && left.persistent == right.persistent
                && left.unknown == right.unknown
        })
}

fn drain_packet_monitor_port(
    port: &proxy::Port,
    root_route: &DeviceRoute,
    logger: &mut PacketLogger,
    latest_stream_timestamp_by_route: &HashMap<DeviceRoute, f64>,
    emitter: &EventSink,
) -> usize {
    let mut packet_count = 0;
    loop {
        match port.try_recv() {
            Ok(mut packet) => {
                packet.routing = root_route.absolute_route(&packet.routing);
                emit_tio_log_message(
                    &packet,
                    latest_stream_timestamp_by_route
                        .get(&packet.routing)
                        .copied(),
                    emitter,
                );
                logger.write_packet(packet, emitter);
                packet_count += 1;
            }
            Err(proxy::RecvError::WouldBlock) => break,
            Err(err) => {
                emitter.error(format!("Raw logging port failed: {err:?}"));
                break;
            }
        }
    }
    packet_count
}

fn emit_tio_log_message(packet: &tio::Packet, timestamp_seconds: Option<f64>, emitter: &EventSink) {
    let Payload::LogMessage(log) = &packet.payload else {
        return;
    };
    let message = log.message.trim_matches(char::from(0)).trim().to_string();
    if message.is_empty() {
        return;
    }
    let timestamp_seconds = timestamp_seconds
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .unwrap_or_else(|| f64::from(log.data) / 1_000.0);
    emitter.emit(&json!({
        "type": "logMessage",
        "route": packet.routing.to_string(),
        "timestampSeconds": timestamp_seconds,
        "message": message
    }));
}

fn spawn_proxy_status_forwarder(
    url: String,
    proxy_status_rx: Receiver<proxy::Event>,
    session_status_tx: Sender<proxy::Event>,
    emitter: EventSink,
) {
    thread::Builder::new()
        .name("proxy-status-forwarder".into())
        .spawn(move || {
            for event in proxy_status_rx {
                emitter.debug(format!("proxy event from {url}: {event:?}"));
                let _ = session_status_tx.send(event);
            }
            emitter.debug(format!("proxy status channel ended for {url}"));
        })
        .expect("failed to spawn proxy status forwarder");
}

fn is_retryable_connect_url(url: &str) -> bool {
    url.starts_with("udp://") || url.starts_with("tcp://")
}

fn next_session_command(
    command_rx: &Receiver<SessionCommand>,
    pending_commands: &mut VecDeque<SessionCommand>,
) -> Option<SessionCommand> {
    pending_commands
        .pop_front()
        .or_else(|| command_rx.try_recv().ok())
}

fn collect_startup_commands(
    command_rx: &Receiver<SessionCommand>,
    pending_commands: &mut VecDeque<SessionCommand>,
    emitter: &EventSink,
) -> bool {
    for command in command_rx.try_iter() {
        match command {
            SessionCommand::Stop => {
                emitter.debug("stop requested while waiting for startup");
                emitter.status("disconnected", "Stopped before connection completed");
                return false;
            }
            other => pending_commands.push_back(other),
        }
    }
    true
}

fn sleep_with_startup_cancel(
    duration: Duration,
    command_rx: &Receiver<SessionCommand>,
    pending_commands: &mut VecDeque<SessionCommand>,
    emitter: &EventSink,
) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if !collect_startup_commands(command_rx, pending_commands, emitter) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
    collect_startup_commands(command_rx, pending_commands, emitter)
}

fn wait_for_proxy_connection(
    status_rx: &Receiver<proxy::Event>,
    command_rx: &Receiver<SessionCommand>,
    pending_commands: &mut VecDeque<SessionCommand>,
    emitter: &EventSink,
    timeout: Duration,
    retry_transient_failures: bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    emitter.debug(format!(
        "waiting up to {:.1}s for proxy connection",
        timeout.as_secs_f64()
    ));
    let mut last_wait_log = Instant::now();

    while Instant::now() < deadline {
        if !collect_startup_commands(command_rx, pending_commands, emitter) {
            return false;
        }

        match status_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(proxy::Event::SensorConnected) | Ok(proxy::Event::SensorReconnected) => {
                emitter.debug("proxy reported sensor connected");
                emitter.status("connected", "Sensor connection established");
                return true;
            }
            Ok(proxy::Event::FailedToConnect) => {
                emitter.debug("proxy reported FailedToConnect");
                if !retry_transient_failures {
                    emitter.error("Twinleaf proxy failed to connect");
                    return false;
                }
                emitter.emit(&json!({
                    "type": "proxyEvent",
                    "event": "FailedToConnect"
                }));
            }
            Ok(proxy::Event::FailedToReconnect) => {
                emitter.debug("proxy reported FailedToReconnect");
                if !retry_transient_failures {
                    emitter.error("Twinleaf proxy failed to reconnect");
                    return false;
                }
                emitter.emit(&json!({
                    "type": "proxyEvent",
                    "event": "FailedToReconnect"
                }));
            }
            Ok(proxy::Event::FatalError(err)) => {
                emitter.debug(format!("proxy fatal error: {err:?}"));
                emitter.error(format!("Twinleaf proxy fatal error: {err:?}"));
                return false;
            }
            Ok(event) => {
                emitter.debug(format!("proxy event while connecting: {event:?}"));
                emitter.emit(&json!({
                    "type": "proxyEvent",
                    "event": format!("{event:?}")
                }));
            }
            Err(channel::RecvTimeoutError::Timeout) => {
                if last_wait_log.elapsed() >= Duration::from_secs(1) {
                    emitter.debug("still waiting for proxy connection...");
                    last_wait_log = Instant::now();
                }
            }
            Err(channel::RecvTimeoutError::Disconnected) => {
                emitter.debug("proxy status channel disconnected while connecting");
                emitter.error("Twinleaf proxy status channel disconnected");
                return false;
            }
        }
    }

    emitter.debug("timed out waiting for proxy connection");
    emitter.error("Timed out waiting for Twinleaf proxy connection");
    false
}

#[derive(Debug)]
struct DeviceDiscoveryResult {
    devices: Vec<DeviceDto>,
    incomplete_rpc_routes: HashSet<DeviceRoute>,
}

#[derive(Debug)]
struct FetchedDevice {
    device: DeviceDto,
    rpc_metadata_complete: bool,
}

#[derive(Debug)]
struct RpcFetchResult {
    rpcs: Vec<RpcDto>,
    complete: bool,
}

fn discover_devices_until_available(
    proxy: &Arc<proxy::Interface>,
    url: &str,
    root_route: &DeviceRoute,
    command_rx: &Receiver<SessionCommand>,
    pending_commands: &mut VecDeque<SessionCommand>,
    emitter: &EventSink,
    timeout: Duration,
) -> Option<DeviceDiscoveryResult> {
    let deadline = Instant::now() + timeout;
    let mut attempt = 1usize;

    while Instant::now() < deadline {
        if !collect_startup_commands(command_rx, pending_commands, emitter) {
            return None;
        }

        emitter.status(
            "discovering",
            format!("Discovering devices on {url} (attempt {attempt})"),
        );
        let discovery = discover_devices(proxy, url, root_route, emitter, false);
        if !discovery.devices.is_empty() {
            return Some(discovery);
        }

        emitter.debug(format!(
            "device metadata not available yet on attempt {attempt}; retrying"
        ));
        attempt += 1;
        if !sleep_with_startup_cancel(
            CONNECTION_METADATA_RETRY_DELAY,
            command_rx,
            pending_commands,
            emitter,
        ) {
            return None;
        }
    }

    emitter.error("Timed out waiting for Twinleaf device metadata");
    None
}

fn discover_devices(
    proxy: &Arc<proxy::Interface>,
    url: &str,
    root_route: &DeviceRoute,
    emitter: &EventSink,
    report_errors: bool,
) -> DeviceDiscoveryResult {
    let mut routes = HashSet::new();

    if let Ok(discovery_port) = proxy.tree_full() {
        emitter.debug("opened tree_full discovery port");
        let mut deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(packet) = discovery_port
                .receiver()
                .recv_timeout(Duration::from_millis(100))
            {
                let route = packet.routing.clone();
                let payload = payload_name(&packet.payload);
                emitter.debug(format!(
                    "discovery packet: route={route}, payload={payload}"
                ));
                if routes.insert(route.clone()) {
                    emitter.debug(format!("discovered route {route} from {payload} packet"));
                    deadline = Instant::now() + Duration::from_secs(2);
                }
            }
        }
    } else {
        emitter.debug("failed to open tree_full discovery port");
    }

    if routes.is_empty() {
        emitter.debug(format!(
            "no packet-discovered routes; falling back to requested route {root_route}"
        ));
        routes.insert(root_route.clone());
    }

    let mut devices = Vec::new();
    let mut incomplete_rpc_routes = HashSet::new();
    for route in routes {
        emitter.status("metadata", format!("Fetching metadata for route {route}"));
        emitter.debug(format!("fetching device metadata/RPCs for route {route}"));
        match panic::catch_unwind(AssertUnwindSafe(|| {
            fetch_device(proxy, url, &route, emitter, report_errors)
        })) {
            Ok(Ok(fetched)) => {
                if !fetched.rpc_metadata_complete {
                    incomplete_rpc_routes.insert(route.clone());
                }
                devices.push(fetched.device);
            }
            Ok(Err(err)) => emit_metadata_issue(
                emitter,
                report_errors,
                format!("Failed to fetch metadata for route {}: {err}", route),
            ),
            Err(err) => emit_metadata_issue(
                emitter,
                report_errors,
                format!(
                    "Twinleaf parser panic while fetching metadata for route {}: {}",
                    route,
                    panic_message(err)
                ),
            ),
        }
    }
    sort_devices_leaf_first(&mut devices, Some(root_route));
    DeviceDiscoveryResult {
        devices,
        incomplete_rpc_routes,
    }
}

fn emit_metadata_issue(emitter: &EventSink, report_errors: bool, message: String) {
    if report_errors {
        emitter.error(message);
    } else {
        emitter.debug(message);
    }
}

fn sort_devices_leaf_first(devices: &mut [DeviceDto], root_route: Option<&DeviceRoute>) {
    let root_route = root_route.map(|route| route.to_string());
    sort_devices_leaf_first_with_root(devices, root_route.as_deref());
}

fn sort_devices_leaf_first_with_root(devices: &mut [DeviceDto], root_route: Option<&str>) {
    let routes: Vec<_> = devices.iter().map(|device| device.route.clone()).collect();
    devices.sort_by(|a, b| {
        let a_is_hub = is_hub_route(&a.route, root_route);
        let b_is_hub = is_hub_route(&b.route, root_route);
        a_is_hub
            .cmp(&b_is_hub)
            .then_with(|| {
                is_parent_route(&a.route, &routes).cmp(&is_parent_route(&b.route, &routes))
            })
            .then_with(|| route_depth(&b.route).cmp(&route_depth(&a.route)))
            .then_with(|| a.route.cmp(&b.route))
    });
}

fn is_hub_route(route: &str, root_route: Option<&str>) -> bool {
    route == "/" || root_route.is_some_and(|root_route| route == root_route)
}

fn is_parent_route(route: &str, routes: &[String]) -> bool {
    if route == "/" {
        return routes.iter().any(|other| other != route);
    }

    let route = route.trim_end_matches('/');
    if route.is_empty() {
        return false;
    }

    let prefix = format!("{route}/");
    routes
        .iter()
        .any(|other| other != route && other.starts_with(&prefix))
}

fn route_depth(route: &str) -> usize {
    route
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .count()
}

fn payload_name(payload: &Payload) -> &'static str {
    match payload {
        Payload::LogMessage(_) => "LogMessage",
        Payload::RpcRequest(_) => "RpcRequest",
        Payload::RpcReply(_) => "RpcReply",
        Payload::RpcError(_) => "RpcError",
        Payload::LegacyTimebaseUpdate(_) => "LegacyTimebaseUpdate",
        Payload::LegacySourceUpdate(_) => "LegacySourceUpdate",
        Payload::LegacyStreamUpdate(_) => "LegacyStreamUpdate",
        Payload::Heartbeat(_) => "Heartbeat",
        Payload::Metadata(_) => "Metadata",
        Payload::Settings(_) => "Settings",
        Payload::LegacyStreamData(_) => "LegacyStreamData",
        Payload::StreamData(_) => "StreamData",
        Payload::ProxyStatus(_) => "ProxyStatus",
        Payload::RpcUpdate(_) => "RpcUpdate",
        Payload::Unknown(_) => "Unknown",
    }
}

fn panic_message(err: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = err.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = err.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else {
        "unknown panic payload".to_string()
    }
}

fn fetch_device(
    proxy: &Arc<proxy::Interface>,
    url: &str,
    route: &DeviceRoute,
    emitter: &EventSink,
    report_errors: bool,
) -> Result<FetchedDevice, String> {
    let rpc_fetch = match proxy.device_rpc(route.clone()) {
        Ok(port) => fetch_rpcs(&port, route, emitter),
        Err(err) => {
            emit_metadata_issue(
                emitter,
                report_errors,
                format!("Failed to open RPC port for route {route}: {err:?}"),
            );
            RpcFetchResult {
                rpcs: Vec::new(),
                complete: false,
            }
        }
    };

    let (meta, streams, full_metadata) =
        match panic::catch_unwind(AssertUnwindSafe(|| fetch_stream_metadata(proxy, route))) {
            Ok(Ok(metadata)) => {
                let (meta, streams) = metadata_to_stream_dtos(route, &metadata);
                (meta, streams, Some(metadata))
            }
            Ok(Err(err)) => {
                emit_metadata_issue(
                    emitter,
                    report_errors,
                    format!("Failed to fetch stream metadata for route {route}: {err}"),
                );
                (fallback_device_meta(route), Vec::new(), None)
            }
            Err(err) => {
                emit_metadata_issue(
                    emitter,
                    report_errors,
                    format!(
                    "Twinleaf parser panic while fetching stream metadata for route {route}: {}",
                    panic_message(err)
                ),
                );
                (fallback_device_meta(route), Vec::new(), None)
            }
        };

    if streams.is_empty() && rpc_fetch.rpcs.is_empty() {
        return Err("No stream metadata or RPC metadata returned".to_string());
    }

    Ok(FetchedDevice {
        device: DeviceDto {
            url: url.to_string(),
            route: route.to_string(),
            meta,
            streams,
            rpcs: rpc_fetch.rpcs,
            full_metadata,
        },
        rpc_metadata_complete: rpc_fetch.complete,
    })
}

fn fallback_device_meta(route: &DeviceRoute) -> DeviceMetaDto {
    DeviceMetaDto {
        serial_number: String::new(),
        firmware_hash: String::new(),
        n_streams: 0,
        session_id: 0,
        name: route.to_string(),
    }
}

fn fetch_stream_metadata(
    proxy: &Arc<proxy::Interface>,
    route: &DeviceRoute,
) -> Result<DeviceFullMetadata, String> {
    let mut data_device = Device::open(proxy, route.clone()).map_err(|err| format!("{err:?}"))?;
    data_device.get_metadata().map_err(|err| format!("{err:?}"))
}

fn metadata_to_device_dto(
    url: String,
    route: &DeviceRoute,
    metadata: &DeviceFullMetadata,
    rpcs: Vec<RpcDto>,
) -> DeviceDto {
    let (meta, streams) = metadata_to_stream_dtos(route, metadata);
    DeviceDto {
        url,
        route: route.to_string(),
        meta,
        streams,
        rpcs,
        full_metadata: Some(metadata.clone()),
    }
}

fn metadata_to_stream_dtos(
    route: &DeviceRoute,
    metadata: &DeviceFullMetadata,
) -> (DeviceMetaDto, Vec<StreamDto>) {
    let meta = DeviceMetaDto {
        serial_number: metadata.device.serial_number.clone(),
        firmware_hash: metadata.device.firmware_hash.clone(),
        n_streams: metadata.device.n_streams,
        session_id: metadata.device.session_id,
        name: metadata.device.name.clone(),
    };

    let mut streams = Vec::new();
    let mut stream_ids: Vec<u8> = metadata.streams.keys().copied().collect();
    stream_ids.sort_unstable();

    for stream_id in stream_ids {
        let stream = metadata.streams.get(&stream_id).unwrap();
        let decimation = if stream.segment.decimation == 0 {
            1.0
        } else {
            stream.segment.decimation as f64
        };
        let effective_sampling_rate = stream.segment.sampling_rate as f64 / decimation;

        let mut columns: Vec<ColumnDto> = stream
            .columns
            .iter()
            .map(|column| ColumnDto {
                key: ColumnKeyDto {
                    route: route.to_string(),
                    stream_id: stream.stream.stream_id,
                    column_index: column.index,
                },
                name: column.name.clone(),
                units: column.units.clone(),
                data_type: format!("{:?}", column.data_type),
                description: column.description.clone(),
                display_value: None,
            })
            .collect();
        columns.sort_by_key(|column| column.key.column_index);

        streams.push(StreamDto {
            stream_id: stream.stream.stream_id,
            name: stream.stream.name.clone(),
            n_columns: stream.stream.n_columns,
            sample_size: stream.stream.sample_size,
            effective_sampling_rate,
            columns,
        });
    }

    (meta, streams)
}

fn fetch_rpcs(port: &proxy::Port, route: &DeviceRoute, emitter: &EventSink) -> RpcFetchResult {
    emitter.debug(format!("fetching RPC list for route {route}"));
    let mut rpcs = Vec::new();
    let count = match retry_rpc_metadata_request(route, "rpc.listinfo count", emitter, || {
        port.get::<u16>("rpc.listinfo")
            .map_err(RpcMetadataRequestError::Rpc)
    }) {
        Ok(count) => count,
        Err(err) => {
            if matches!(err, RpcMetadataRequestError::Panic(_)) {
                emitter.error(format!(
                    "Twinleaf parser panic while counting RPCs for route {route}: {err}"
                ));
            } else {
                emitter.debug(format!(
                    "rpc.listinfo count failed for route {route}: {err}"
                ));
            }
            return RpcFetchResult {
                rpcs,
                complete: false,
            };
        }
    };

    emitter.debug(format!("route {route} reports {count} RPC(s)"));
    let mut missing_rpcs = 0usize;
    for rpc_id in 0..count {
        let label = format!("rpc.listinfo({rpc_id})");
        let rpc_info = retry_rpc_metadata_request(route, &label, emitter, || {
            port.raw_rpc("rpc.listinfo", &rpc_id.to_le_bytes())
                .map_err(RpcMetadataRequestError::Rpc)
                .and_then(|reply| {
                    decode_rpc_listinfo_reply(&reply).map_err(RpcMetadataRequestError::Decode)
                })
        });

        let (meta_bits, name) = match rpc_info {
            Ok(info) => info,
            Err(RpcMetadataRequestError::Panic(err)) => {
                emitter.error(format!(
                    "Twinleaf parser panic while reading RPC {rpc_id} for route {route}: {err}"
                ));
                continue;
            }
            Err(err) => {
                emitter.debug(format!(
                    "rpc.listinfo({rpc_id}) failed for route {route}: {err}"
                ));
                missing_rpcs += 1;
                continue;
            }
        };

        let descriptor = RpcDescriptor::from_meta(meta_bits, name);
        let arg_type = rpc_arg_type(&descriptor);
        let size = rpc_size(&descriptor);

        rpcs.push(RpcDto {
            route: route.to_string(),
            name: descriptor.full_name.clone(),
            size,
            permissions: rpc_permissions(&descriptor),
            arg_type,
            readable: rpc_readable(&descriptor),
            writable: descriptor.meta.flags().contains(RpcMetaFlags::WRITABLE),
            persistent: descriptor.meta.is_persistent(),
            unknown: rpc_unknown(&descriptor),
            value: None,
        });
    }

    rpcs.sort_by(|a, b| a.name.cmp(&b.name));
    let complete = missing_rpcs == 0 && rpcs.len() == usize::from(count);
    emitter.debug(format!(
        "loaded {} of {count} RPC(s) for route {route}",
        rpcs.len()
    ));
    RpcFetchResult { rpcs, complete }
}

#[derive(Debug)]
enum RpcMetadataRequestError {
    Rpc(proxy::RpcError),
    Decode(String),
    Panic(String),
}

impl std::fmt::Display for RpcMetadataRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(err) => write!(formatter, "{err:?}"),
            Self::Decode(err) | Self::Panic(err) => formatter.write_str(err),
        }
    }
}

impl RpcMetadataRequestError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Rpc(proxy::RpcError::RecvFailed(_)) => true,
            Self::Rpc(proxy::RpcError::ExecError(payload)) => {
                matches!(payload.error, tio::proto::RpcErrorCode::Timeout)
            }
            Self::Rpc(_) | Self::Decode(_) | Self::Panic(_) => false,
        }
    }
}

fn retry_rpc_metadata_request<T, F>(
    route: &DeviceRoute,
    label: &str,
    emitter: &EventSink,
    mut request: F,
) -> Result<T, RpcMetadataRequestError>
where
    F: FnMut() -> Result<T, RpcMetadataRequestError>,
{
    for attempt in 1..=RPC_METADATA_RETRY_ATTEMPTS {
        let result = panic::catch_unwind(AssertUnwindSafe(|| request()));
        match result {
            Ok(Ok(value)) => {
                if attempt > 1 {
                    emitter.debug(format!(
                        "{label} for route {route} succeeded on attempt {attempt}"
                    ));
                }
                return Ok(value);
            }
            Ok(Err(err)) => {
                if attempt == RPC_METADATA_RETRY_ATTEMPTS || !err.is_retryable() {
                    return Err(err);
                }
                emitter.debug(format!(
                    "{label} for route {route} failed on attempt {attempt}/{RPC_METADATA_RETRY_ATTEMPTS}: {err}; retrying"
                ));
            }
            Err(err) => return Err(RpcMetadataRequestError::Panic(panic_message(err))),
        }
        thread::sleep(RPC_METADATA_RETRY_DELAY);
    }

    unreachable!("retry loop must either return a value or the last error")
}

fn decode_rpc_listinfo_reply(reply: &[u8]) -> Result<(u16, String), String> {
    if reply.len() < 2 {
        return Err(format!(
            "rpc.listinfo reply too short: {} byte(s)",
            reply.len()
        ));
    }

    let meta_bits = u16::from_le_bytes([reply[0], reply[1]]);
    let name_bytes = reply[2..].split(|byte| *byte == 0).next().unwrap_or(&[]);
    if name_bytes.is_empty() {
        return Err("rpc.listinfo reply did not include a name".to_string());
    }

    let name = String::from_utf8_lossy(name_bytes).trim().to_string();
    if name.is_empty() {
        return Err("rpc.listinfo reply name was blank".to_string());
    }

    Ok((meta_bits, name))
}

fn load_log_file(path: &str, emitter: &EventSink) -> Result<PlaybackSession, String> {
    emitter.status("loading", format!("Loading log file {path}"));
    let file_data = std::fs::read(path).map_err(|err| err.to_string())?;
    if file_data.is_empty() {
        return Err("Log file is empty".to_string());
    }

    let mut rest: &[u8] = &file_data;
    let mut parsers: HashMap<DeviceRoute, DeviceDataParser> = HashMap::new();
    let mut routes_seen: HashSet<DeviceRoute> = HashSet::new();
    let mut column_states: HashMap<ColumnKeyDto, ColumnState> = HashMap::new();
    let active_columns = HashSet::new();
    let view = ViewConfig::default();
    let plot_panes = default_plot_panes(&view);
    let mut packets = 0usize;
    let mut samples = 0usize;
    let mut recording_start: Option<f64> = None;
    let mut time_reference_start: Option<f64> = None;
    let mut latest_stream_timestamp_by_route: HashMap<DeviceRoute, f64> = HashMap::new();

    while !rest.is_empty() {
        let (packet, len) = match tio::Packet::deserialize(rest) {
            Ok(result) => result,
            Err(err) => {
                emitter.debug(format!(
                    "stopped log parsing at byte {} of {}: {err:?}",
                    file_data.len() - rest.len(),
                    file_data.len()
                ));
                break;
            }
        };

        if len == 0 {
            break;
        }
        rest = &rest[len..];
        packets += 1;

        let route = packet.routing.clone();
        emit_tio_log_message(
            &packet,
            latest_stream_timestamp_by_route.get(&route).copied(),
            emitter,
        );
        routes_seen.insert(route.clone());
        let parser = parsers
            .entry(route.clone())
            .or_insert_with(|| DeviceDataParser::new(false));

        let parsed_samples =
            match panic::catch_unwind(AssertUnwindSafe(|| parser.process_packet(&packet))) {
                Ok(samples) => samples,
                Err(err) => {
                    emitter.error(format!(
                        "Twinleaf parser panic while reading log route {}: {}",
                        route,
                        panic_message(err)
                    ));
                    Vec::new()
                }
            };

        samples += parsed_samples.len();
        for sample in parsed_samples {
            let sample_start = sample.timestamp_begin();
            if sample_start.is_finite() {
                recording_start = Some(
                    recording_start
                        .map(|start| start.min(sample_start))
                        .unwrap_or(sample_start),
                );
            }
            let sample_end = sample.timestamp_end();
            if sample_end.is_finite() {
                latest_stream_timestamp_by_route.insert(route.clone(), sample_end);
            }
            time_reference_start =
                earliest_time(time_reference_start, sample_time_reference_start(&sample));
            process_sample(
                &sample,
                &route,
                &mut column_states,
                &active_columns,
                &plot_panes,
                f64::INFINITY,
                None,
            );
        }
    }

    let mut devices = Vec::new();
    let url = format!("file://{path}");
    for (route, parser) in parsers.iter_mut() {
        match panic::catch_unwind(AssertUnwindSafe(|| parser.get_metadata())) {
            Ok(Ok(metadata)) => {
                devices.push(metadata_to_device_dto(
                    url.clone(),
                    route,
                    &metadata,
                    Vec::new(),
                ));
            }
            Ok(Err(_)) => {
                if routes_seen.contains(route) {
                    emitter.debug(format!(
                        "log route {route} did not include complete metadata"
                    ));
                }
            }
            Err(err) => emitter.error(format!(
                "Twinleaf parser panic while reading log metadata for route {}: {}",
                route,
                panic_message(err)
            )),
        }
    }
    sort_devices_leaf_first(&mut devices, None);

    if devices.is_empty() {
        return Err("Log did not include complete stream metadata".to_string());
    }

    if data_range(&column_states).is_none() {
        return Err("Log did not include parseable sample data".to_string());
    }

    emitter.debug(format!(
        "loaded log file {path}: packets={packets}, samples={samples}, devices={}",
        devices.len()
    ));

    Ok(PlaybackSession::new(
        devices,
        column_states,
        recording_start,
        time_reference_start,
    ))
}

#[derive(Debug, Clone, Copy)]
struct ExportSummary {
    rows: usize,
    bytes: u64,
}

fn export_log_file(
    source_path: &str,
    output_path: &str,
    format: ExportFormat,
    emitter: &EventSink,
) -> Result<ExportSummary, String> {
    let output_path = Path::new(output_path);
    let temp_path = temporary_export_path(output_path);

    let result = match format {
        ExportFormat::Csv => export_log_csv(source_path, &temp_path, emitter),
        ExportFormat::Hdf5 => export_log_hdf5(source_path, &temp_path, emitter),
    };

    match result {
        Ok(mut summary) => {
            replace_export_file(&temp_path, output_path)?;
            summary.bytes = std::fs::metadata(output_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            Ok(summary)
        }
        Err(err) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(err)
        }
    }
}

fn export_log_csv(
    source_path: &str,
    output_path: &Path,
    emitter: &EventSink,
) -> Result<ExportSummary, String> {
    let file_data = std::fs::read(source_path).map_err(|err| err.to_string())?;
    if file_data.is_empty() {
        return Err("Log file is empty".to_string());
    }

    let file = File::create(output_path).map_err(|err| err.to_string())?;
    let mut writer = BufWriter::new(file);
    write_csv_record(
        &mut writer,
        &[
            "route",
            "device",
            "session_id",
            "stream_id",
            "stream",
            "segment_id",
            "sample_number",
            "time",
            "column_index",
            "column",
            "units",
            "data_type",
            "value",
        ],
    )
    .map_err(|err| err.to_string())?;

    let mut rest: &[u8] = &file_data;
    let mut parsers: HashMap<DeviceRoute, DeviceDataParser> = HashMap::new();
    let mut packets = 0usize;
    let mut samples = 0usize;
    let mut rows = 0usize;

    while !rest.is_empty() {
        let (packet, len) = match tio::Packet::deserialize(rest) {
            Ok(result) => result,
            Err(err) => {
                emitter.debug(format!(
                    "stopped CSV export parsing at byte {} of {}: {err:?}",
                    file_data.len() - rest.len(),
                    file_data.len()
                ));
                break;
            }
        };

        if len == 0 {
            break;
        }
        rest = &rest[len..];
        packets += 1;

        let route = packet.routing.clone();
        let parser = parsers
            .entry(route.clone())
            .or_insert_with(|| DeviceDataParser::new(false));
        let parsed_samples =
            match panic::catch_unwind(AssertUnwindSafe(|| parser.process_packet(&packet))) {
                Ok(samples) => samples,
                Err(err) => {
                    emitter.error(format!(
                        "Twinleaf parser panic while exporting CSV route {}: {}",
                        route,
                        panic_message(err)
                    ));
                    Vec::new()
                }
            };

        samples += parsed_samples.len();
        for sample in parsed_samples {
            rows += write_sample_csv_records(&mut writer, &route, &sample)
                .map_err(|err| err.to_string())?;
        }
    }

    writer.flush().map_err(|err| err.to_string())?;
    if samples == 0 {
        return Err("Log did not include parseable sample data".to_string());
    }

    emitter.debug(format!(
        "exported CSV from {source_path}: packets={packets}, samples={samples}, rows={rows}"
    ));
    Ok(ExportSummary { rows, bytes: 0 })
}

fn write_sample_csv_records<W: Write>(
    writer: &mut W,
    route: &DeviceRoute,
    sample: &Sample,
) -> io::Result<usize> {
    let route = route.to_string();
    let session_id = sample.device.session_id.to_string();
    let stream_id = sample.stream.stream_id.to_string();
    let segment_id = sample.segment.segment_id.to_string();
    let sample_number = sample.n.to_string();
    let timestamp = format!("{:.17}", sample.timestamp_end());
    let mut rows = 0usize;

    for column in &sample.columns {
        let fields = vec![
            route.clone(),
            sample.device.name.clone(),
            session_id.clone(),
            stream_id.clone(),
            sample.stream.name.clone(),
            segment_id.clone(),
            sample_number.clone(),
            timestamp.clone(),
            column.desc.index.to_string(),
            column.desc.name.clone(),
            column.desc.units.clone(),
            format!("{:?}", column.desc.data_type),
            column.value.to_string(),
        ];
        write_csv_record(writer, &fields)?;
        rows += 1;
    }

    Ok(rows)
}

#[cfg(feature = "hdf5")]
fn export_log_hdf5(
    source_path: &str,
    output_path: &Path,
    emitter: &EventSink,
) -> Result<ExportSummary, String> {
    use twinleaf::data::export::{Hdf5Appender, RunSplitLevel, SplitPolicy};
    use twinleaf::tio::proto::identifiers::StreamKey;

    let file_data = std::fs::read(source_path).map_err(|err| err.to_string())?;
    if file_data.is_empty() {
        return Err("Log file is empty".to_string());
    }

    let mut writer = Hdf5Appender::with_options(
        output_path,
        true,
        false,
        None,
        65_536,
        SplitPolicy::default(),
        RunSplitLevel::default(),
    )
    .map_err(|err| format!("Failed to create HDF5 export: {err:?}"))?;

    let mut rest: &[u8] = &file_data;
    let mut parsers: HashMap<DeviceRoute, DeviceDataParser> = HashMap::new();
    let mut packets = 0usize;
    let mut samples = 0usize;

    while !rest.is_empty() {
        let (packet, len) = match tio::Packet::deserialize(rest) {
            Ok(result) => result,
            Err(err) => {
                emitter.debug(format!(
                    "stopped HDF5 export parsing at byte {} of {}: {err:?}",
                    file_data.len() - rest.len(),
                    file_data.len()
                ));
                break;
            }
        };

        if len == 0 {
            break;
        }
        rest = &rest[len..];
        packets += 1;

        let route = packet.routing.clone();
        let parser = parsers
            .entry(route.clone())
            .or_insert_with(|| DeviceDataParser::new(false));
        let parsed_samples =
            match panic::catch_unwind(AssertUnwindSafe(|| parser.process_packet(&packet))) {
                Ok(samples) => samples,
                Err(err) => {
                    emitter.error(format!(
                        "Twinleaf parser panic while exporting HDF5 route {}: {}",
                        route,
                        panic_message(err)
                    ));
                    Vec::new()
                }
            };

        samples += parsed_samples.len();
        for sample in parsed_samples {
            let key = StreamKey::new(route.clone(), sample.stream.stream_id);
            writer
                .write_sample(sample, key)
                .map_err(|err| format!("HDF5 write failed: {err:?}"))?;
        }
    }

    let stats = writer
        .finish()
        .map_err(|err| format!("Failed to finalize HDF5 export: {err:?}"))?;
    if stats.total_samples == 0 {
        return Err("Log did not include parseable sample data".to_string());
    }

    emitter.debug(format!(
        "exported HDF5 from {source_path}: packets={packets}, samples={samples}, streams={}",
        stats.streams_written.len()
    ));
    Ok(ExportSummary {
        rows: stats.total_samples as usize,
        bytes: 0,
    })
}

#[cfg(not(feature = "hdf5"))]
fn export_log_hdf5(
    _source_path: &str,
    _output_path: &Path,
    _emitter: &EventSink,
) -> Result<ExportSummary, String> {
    Err("HDF5 export is not enabled in this build of tio-bridge".to_string())
}

fn temporary_export_path(output_path: &Path) -> PathBuf {
    let file_name = output_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "Twinleaf Export".into());
    output_path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

fn replace_export_file(temp_path: &Path, output_path: &Path) -> Result<(), String> {
    std::fs::rename(temp_path, output_path).map_err(|err| {
        format!(
            "Failed to move export into place from {} to {}: {err}",
            temp_path.display(),
            output_path.display()
        )
    })
}

fn write_csv_record<W, S>(writer: &mut W, fields: &[S]) -> io::Result<()>
where
    W: Write,
    S: AsRef<str>,
{
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        write_csv_field(writer, field.as_ref())?;
    }
    writer.write_all(b"\n")
}

fn write_csv_field<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    let needs_quotes = value
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quotes {
        return writer.write_all(value.as_bytes());
    }

    writer.write_all(b"\"")?;
    for byte in value.bytes() {
        if byte == b'"' {
            writer.write_all(b"\"\"")?;
        } else {
            writer.write_all(&[byte])?;
        }
    }
    writer.write_all(b"\"")
}

fn build_column_states(devices: &[DeviceDto]) -> HashMap<ColumnKeyDto, ColumnState> {
    let mut states = HashMap::new();
    for device in devices {
        for stream in &device.streams {
            for column in &stream.columns {
                let label = format!("{} {}.{}", device.meta.name, stream.name, column.name);
                states.insert(
                    column.key.clone(),
                    ColumnState::new(label, column.units.clone(), stream.effective_sampling_rate),
                );
            }
        }
    }
    states
}

fn build_rpc_index(devices: &[DeviceDto]) -> HashMap<(String, String), RpcDto> {
    let mut index = HashMap::new();
    for device in devices {
        for rpc in &device.rpcs {
            index.insert((device.route.clone(), rpc.name.clone()), rpc.clone());
        }
    }
    index
}

fn process_sample(
    sample: &Sample,
    route: &DeviceRoute,
    column_states: &mut HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
    plot_panes: &[PlotPaneConfig],
    max_window_seconds: f64,
    mut profiler: Option<&mut StreamIngestProfiler>,
) {
    let profile_enabled = profiler.as_ref().is_some_and(|profiler| profiler.enabled);
    let profile_start = profile_enabled.then(Instant::now);
    let mut display_elapsed = Duration::ZERO;
    let mut processed_columns = 0usize;
    let mut active_hits = 0usize;
    let route_string = route.to_string();
    for column in &sample.columns {
        let Some(value) = column.value.try_as_f64() else {
            continue;
        };
        let key = ColumnKeyDto {
            route: route_string.clone(),
            stream_id: sample.stream.stream_id,
            column_index: column.desc.index,
        };

        let point = Point {
            x: sample.timestamp_end(),
            y: value,
        };

        let active_fpcs_panes: Vec<_> = plot_panes
            .iter()
            .filter(|pane| pane_contains_column(pane, &key))
            .filter(|pane| {
                pane.view.mode == PlotMode::Timeseries
                    && pane.view.decimation_method == DecimationMethod::Fpcs
            })
            .map(|pane| (pane.id, pane.view.clone()))
            .collect();
        let is_active = !active_fpcs_panes.is_empty()
            || active_columns.contains(&key)
            || active_columns.iter().any(|active_key| {
                active_key.stream_id == key.stream_id && active_key.column_index == key.column_index
            });
        if is_active {
            active_hits += 1;
        }

        let effective_rate =
            sample.segment.sampling_rate as f64 / sample.segment.decimation.max(1) as f64;
        let state = column_states.entry(key.clone()).or_insert_with(|| {
            ColumnState::new(
                format!(
                    "{} stream {} column {}",
                    route, sample.stream.stream_id, column.desc.index
                ),
                column.desc.units.clone(),
                effective_rate,
            )
        });
        // Keep the per-column rate in sync with the live segment. A device-side
        // decimation change alters the effective sample rate without new
        // metadata; if the stale rate persists, the FPCS decimation ratio is
        // wrong and the displayed window collapses. `ensure_fpcs_configured`
        // rebuilds the decimator when the resulting ratio changes.
        if effective_rate.is_finite()
            && effective_rate > 0.0
            && (state.sample_rate - effective_rate).abs() > effective_rate * 0.001
        {
            state.sample_rate = effective_rate;
        }
        display_elapsed += state.push_raw_profiled(point, max_window_seconds, profile_enabled);
        processed_columns += 1;
        for (pane_id, view) in active_fpcs_panes {
            state.process_fpcs_point(pane_id, &view, point);
        }
    }
    if let (Some(start), Some(profiler)) = (profile_start, profiler.as_deref_mut()) {
        profiler.record(
            start.elapsed(),
            display_elapsed,
            processed_columns,
            active_hits,
            column_states.len(),
            active_columns.len(),
        );
    }
}

fn hydrate_fpcs(
    column_states: &mut HashMap<ColumnKeyDto, ColumnState>,
    plot_panes: &[PlotPaneConfig],
    emitter: &EventSink,
) {
    let pane_ids = active_fpcs_pane_ids(plot_panes);
    for state in column_states.values_mut() {
        state.retain_fpcs_panes(&pane_ids);
    }

    for pane in plot_panes {
        let view = &pane.view;
        if view.mode != PlotMode::Timeseries {
            continue;
        }

        if view.decimation_method != DecimationMethod::Fpcs {
            emitter.debug(format!(
                "pane {} timeseries decimation disabled; emitting raw points for {:.1}s window",
                pane.id, view.window_seconds
            ));
            continue;
        }

        let active_columns = pane_columns_set(pane);
        let max_sampling_rate = active_columns
            .iter()
            .filter_map(|key| column_states.get(key).map(|state| state.sample_rate))
            .fold(0.0, f64::max)
            .max(1.0);

        let target_points = target_plot_points(view);
        let ratio = fpcs_ratio(max_sampling_rate, view);
        emitter.debug(format!(
            "FPCS hydrate pane={}: active={}, width={}px, resolution={}%, target_points={}, max_sr={:.3}Hz, window={:.1}s, ratio={}",
            pane.id,
            active_columns.len(),
            view.plot_width_pixels,
            view.resolution_multiplier,
            target_points,
            max_sampling_rate,
            view.window_seconds,
            ratio
        ));

        for key in &active_columns {
            let Some(data_key) = resolve_data_key(column_states, key) else {
                continue;
            };
            if let Some(state) = column_states.get_mut(&data_key) {
                // Rebuild (not just ensure) so a column re-entering a pane gets
                // a fresh ring from the continuous raw buffer — no gap across
                // the interval it was unselected.
                state.rebuild_fpcs(pane.id, view);
            }
        }
    }
}

fn target_plot_points(view: &ViewConfig) -> usize {
    let width = view.plot_width_pixels.clamp(64, 8_192);
    let multiplier = view.resolution_multiplier.clamp(20, 200) as f64 / 100.0;
    ((width as f64 * multiplier).round() as usize).clamp(64, 8_192)
}

fn fpcs_ratio(sample_rate: f64, view: &ViewConfig) -> usize {
    let window_samples = sample_rate.max(1.0) * view.window_seconds.max(1e-6);
    ((2.0 * window_samples) / target_plot_points(view) as f64)
        .ceil()
        .max(1.0) as usize
}

fn default_plot_panes(view: &ViewConfig) -> Vec<PlotPaneConfig> {
    vec![PlotPaneConfig {
        id: 0,
        view: view.clone(),
        columns: Vec::new(),
        viewport_end: None,
    }]
}

fn pane_columns_set(pane: &PlotPaneConfig) -> HashSet<ColumnKeyDto> {
    pane.columns.iter().cloned().collect()
}

fn active_columns_for_panes(plot_panes: &[PlotPaneConfig]) -> HashSet<ColumnKeyDto> {
    plot_panes
        .iter()
        .flat_map(|pane| pane.columns.iter().cloned())
        .collect()
}

fn active_pane_ids(plot_panes: &[PlotPaneConfig]) -> HashSet<usize> {
    plot_panes.iter().map(|pane| pane.id).collect()
}

fn active_fpcs_pane_ids(plot_panes: &[PlotPaneConfig]) -> HashSet<usize> {
    plot_panes
        .iter()
        .filter(|pane| {
            pane.view.mode == PlotMode::Timeseries
                && pane.view.decimation_method == DecimationMethod::Fpcs
        })
        .map(|pane| pane.id)
        .collect()
}

fn pane_contains_column(pane: &PlotPaneConfig, key: &ColumnKeyDto) -> bool {
    pane.columns.contains(key)
        || pane.columns.iter().any(|active_key| {
            active_key.stream_id == key.stream_id && active_key.column_index == key.column_index
        })
}

fn update_default_pane_columns(
    plot_panes: &mut Vec<PlotPaneConfig>,
    columns: Vec<ColumnKeyDto>,
    view: &ViewConfig,
) {
    if plot_panes.is_empty() {
        *plot_panes = default_plot_panes(view);
    }

    if let Some(pane) = plot_panes.first_mut() {
        pane.columns = columns;
    }
}

fn apply_view_to_all_panes(plot_panes: &mut Vec<PlotPaneConfig>, view: &ViewConfig) {
    for pane in plot_panes {
        pane.view = view.clone();
    }
}

/// Upper bound on how many seconds of live samples we retain per stream. Needs
/// to scale with the user-selectable window (now up to 1000 s); the actual
/// per-stream memory is bounded in tandem by `MAX_RAW_POINTS_PER_STREAM` below.
const MAX_RETENTION_SECONDS: f64 = 1500.0;

/// Hard cap on raw points kept per stream. Prevents a long window combined
/// with a high sample rate from growing memory without bound: e.g. a 1000 s
/// window at 1 kHz would otherwise want 1M points/stream. Past this count
/// the oldest samples are dropped, so the visible window may be shorter
/// than requested for very high sample rates.
const MAX_RAW_POINTS_PER_STREAM: usize = 100_000;

fn live_retention_seconds(view: &ViewConfig) -> f64 {
    let window = view.window_seconds.max(1.0);
    (window * 1.5)
        .max(window + 5.0)
        .clamp(10.0, MAX_RETENTION_SECONDS)
}

fn live_retention_seconds_for_panes(plot_panes: &[PlotPaneConfig], fallback: &ViewConfig) -> f64 {
    plot_panes
        .iter()
        .map(|pane| live_retention_seconds(&pane.view))
        .fold(live_retention_seconds(fallback), f64::max)
}

fn retained_point_count(column_states: &HashMap<ColumnKeyDto, ColumnState>) -> usize {
    column_states.values().map(|state| state.raw.len()).sum()
}

/// Min/max of the data's timestamps, counting only points at or after `floor`
/// (the synced time reference). This drops pre-sync samples timestamped near 0
/// and — importantly — yields the *first real sample's* time, not the projected
/// time of sample 0 (which is `floor` itself and isn't in the data set). With
/// `floor = None`, every point counts.
fn data_range_since(
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    floor: Option<f64>,
) -> Option<(f64, f64)> {
    let mut start = f64::INFINITY;
    let mut end = f64::NEG_INFINITY;
    for point in column_states
        .values()
        .flat_map(|state| state.raw.iter())
        .filter(|point| point.x.is_finite())
        .filter(|point| floor.map_or(true, |floor| point.x >= floor))
    {
        start = start.min(point.x);
        end = end.max(point.x);
    }
    if start.is_finite() && end.is_finite() {
        Some((start, end))
    } else {
        None
    }
}

fn data_range(column_states: &HashMap<ColumnKeyDto, ColumnState>) -> Option<(f64, f64)> {
    let mut start = f64::INFINITY;
    let mut end = f64::NEG_INFINITY;

    for point in column_states
        .values()
        .flat_map(|state| state.raw.iter())
        .filter(|point| point.x.is_finite())
    {
        start = start.min(point.x);
        end = end.max(point.x);
    }

    if start.is_finite() && end.is_finite() {
        Some((start, end))
    } else {
        None
    }
}

fn earliest_time(current: Option<f64>, candidate: Option<f64>) -> Option<f64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (Some(current), None) => Some(current),
        (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

fn sample_time_reference_start(sample: &Sample) -> Option<f64> {
    if sample.segment.time_ref_session_id == 0 {
        return None;
    }

    match &sample.segment.time_ref_epoch {
        MetadataEpoch::Unix | MetadataEpoch::Systime => {
            let start = sample.timestamp_begin();
            if start.is_finite() && start > 0.0 {
                Some(start)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn fpcs_decimate_window(points: &[Point], sample_rate: f64, view: &ViewConfig) -> Vec<Point> {
    let target_points = target_plot_points(view);
    if points.len() <= target_points {
        return points.to_vec();
    }

    let ratio = fpcs_ratio(sample_rate, view);
    let mut fpcs = StreamingFpcs::new(ratio, target_points.max(2));
    for point in points {
        fpcs.process_point(*point);
    }
    fpcs.output.iter().copied().collect()
}

fn latest_active_x(
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
) -> Option<f64> {
    active_columns
        .iter()
        .filter_map(|key| resolve_column_state(column_states, key).map(|(_, state)| state))
        .filter_map(|state| state.raw.back())
        .filter(|point| point.x.is_finite())
        .map(|point| point.x)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

fn resolve_data_key(
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    key: &ColumnKeyDto,
) -> Option<ColumnKeyDto> {
    resolve_column_state(column_states, key).map(|(data_key, _)| data_key.clone())
}

fn resolve_column_state<'a>(
    column_states: &'a HashMap<ColumnKeyDto, ColumnState>,
    key: &ColumnKeyDto,
) -> Option<(&'a ColumnKeyDto, &'a ColumnState)> {
    let exact = column_states.get_key_value(key);
    if exact.is_some_and(|(_, state)| !state.raw.is_empty()) {
        return exact;
    }

    let mut candidates = column_states.iter().filter(|(candidate_key, state)| {
        candidate_key.stream_id == key.stream_id
            && candidate_key.column_index == key.column_index
            && !state.raw.is_empty()
    });
    let first = candidates.next();
    if first.is_some() && candidates.next().is_none() {
        return first;
    }

    exact
}

fn emit_active_columns(emitter: &EventSink, active_columns: &HashSet<ColumnKeyDto>) {
    let columns: Vec<_> = active_columns.iter().cloned().collect();
    emitter.emit(&json!({
        "type": "activeColumns",
        "columns": columns
    }));
}

fn emit_stream_values(
    emitter: &EventSink,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
) -> usize {
    let mut values: Vec<_> = column_states
        .iter()
        .filter_map(|(key, state)| {
            state
                .display_value
                .filter(|value| value.is_finite())
                .map(|value| StreamValueDto {
                    key: key.clone(),
                    value,
                })
        })
        .collect();
    values.sort_by(|a, b| {
        (&a.key.route, a.key.stream_id, a.key.column_index).cmp(&(
            &b.key.route,
            b.key.stream_id,
            b.key.column_index,
        ))
    });

    if values.is_empty() {
        return 0;
    }

    let value_count = values.len();
    emitter.emit(&json!({
        "type": "streamValues",
        "values": values
    }));
    value_count
}

fn emit_stream_values_for_view(
    emitter: &EventSink,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    viewport_end: f64,
    window_seconds: f64,
) -> usize {
    if !viewport_end.is_finite() {
        return 0;
    }

    let window_seconds = window_seconds.max(1e-6);
    let viewport_start = viewport_end - window_seconds;
    let mut values: Vec<_> = column_states
        .iter()
        .filter_map(|(key, state)| {
            state
                .display_value_between(viewport_start, viewport_end)
                .filter(|value| value.is_finite())
                .map(|value| StreamValueDto {
                    key: key.clone(),
                    value,
                })
        })
        .collect();
    values.sort_by(|a, b| {
        (&a.key.route, a.key.stream_id, a.key.column_index).cmp(&(
            &b.key.route,
            b.key.stream_id,
            b.key.column_index,
        ))
    });

    if values.is_empty() {
        return 0;
    }

    let value_count = values.len();
    emitter.emit(&json!({
        "type": "streamValues",
        "values": values
    }));
    value_count
}

fn emit_live_plot(
    emitter: &EventSink,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
    plot_panes: &[PlotPaneConfig],
    fft_workers: &mut HashMap<usize, FftWorker>,
) -> Option<PlotEmitStats> {
    let pane_ids = active_pane_ids(plot_panes);
    fft_workers.retain(|pane_id, _| pane_ids.contains(pane_id));

    let shared_view_end = latest_active_x(column_states, active_columns);
    let mut aggregate: Option<PlotEmitStats> = None;

    for pane in plot_panes {
        let pane_columns = pane_columns_set(pane);
        let stats = match pane.view.mode {
            PlotMode::Timeseries => Some(emit_live_timeseries_plot(
                emitter,
                pane.id,
                column_states,
                &pane.columns,
                &pane.view,
                shared_view_end,
                pane.viewport_end,
            )),
            PlotMode::Fft => {
                let fft_worker = fft_workers
                    .entry(pane.id)
                    .or_insert_with(|| FftWorker::new(emitter));
                emit_live_fft_plot(
                    emitter,
                    pane.id,
                    column_states,
                    &pane_columns,
                    &pane.view,
                    pane.viewport_end.or(shared_view_end),
                    fft_worker,
                )
            }
        };

        if let Some(stats) = stats {
            aggregate = Some(match aggregate {
                Some(current) => PlotEmitStats {
                    mode: stats.mode,
                    series_count: current.series_count + stats.series_count,
                    point_count: current.point_count + stats.point_count,
                    viewport_end: stats.viewport_end.or(current.viewport_end),
                },
                None => stats,
            });
        }
    }

    aggregate
}

/// Build the timeseries series for one pane, anchoring the time window to the
/// pane's first column that has data. Columns whose time references roughly
/// agree fall inside the window and plot normally; columns on an incompatible
/// time base have no points in the window and are sent flagged (and empty) so
/// the legend can mark them.
fn build_live_timeseries_series<'a>(
    pane_id: usize,
    column_states: &'a HashMap<ColumnKeyDto, ColumnState>,
    pane_columns: &'a [ColumnKeyDto],
    view: &ViewConfig,
    shared_view_end: Option<f64>,
    viewport: Option<f64>,
) -> (Option<f64>, Vec<BinaryPlotSeries<'a>>, usize) {
    // A pane that's been panned/frozen ends at `viewport`; otherwise it follows
    // the latest sample.
    let view_end = viewport.or_else(|| {
        pane_columns
            .iter()
            .filter_map(|key| resolve_column_state(column_states, key).map(|(_, state)| state))
            .filter_map(|state| state.raw.back())
            .map(|point| point.x)
            .find(|x| x.is_finite())
            .or(shared_view_end)
    });

    let window_start = view_end
        .map(|end| end - view.window_seconds.max(1e-6))
        .unwrap_or(f64::NEG_INFINITY);
    let window_end = view_end.unwrap_or(f64::INFINITY);

    let mut series = Vec::new();
    let mut point_count = 0usize;

    for key in pane_columns {
        let Some((data_key, state)) = resolve_column_state(column_states, key) else {
            continue;
        };
        if state.raw.is_empty() {
            continue;
        }
        let points = if viewport.is_some() {
            // Frozen/panned view: the live FPCS ring tracks the latest window,
            // so read raw points for the requested (past) window instead.
            state.point_source_between(window_start, window_end)
        } else {
            match view.decimation_method {
                DecimationMethod::Fpcs => state
                    .fpcs_for_pane(pane_id)
                    .map(|fpcs| fpcs.point_source_since(window_start))
                    .unwrap_or_else(|| state.point_source_between(window_start, window_end)),
                DecimationMethod::None => state.point_source_between(window_start, window_end),
            }
        };

        // FPCS retains points since window_start regardless of the upper
        // bound, so verify the column actually overlaps the window before
        // declaring it visible.
        let overlaps_window = state
            .raw
            .back()
            .map(|latest| latest.x >= window_start)
            .unwrap_or(false)
            && state
                .raw
                .front()
                .map(|earliest| earliest.x <= window_end)
                .unwrap_or(false);

        if points.is_empty() || !overlaps_window {
            series.push(BinaryPlotSeries {
                key: data_key,
                sample_rate: state.sample_rate,
                points: PlotPointSource::Slice(&[]),
                outside_window: true,
                noise_floor: None,
            });
            continue;
        }

        point_count += points.len();
        series.push(BinaryPlotSeries {
            key: data_key,
            sample_rate: state.sample_rate,
            points,
            outside_window: false,
            noise_floor: None,
        });
    }

    (view_end, series, point_count)
}

fn emit_live_timeseries_plot(
    emitter: &EventSink,
    pane_id: usize,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    pane_columns: &[ColumnKeyDto],
    view: &ViewConfig,
    shared_view_end: Option<f64>,
    viewport: Option<f64>,
) -> PlotEmitStats {
    let (view_end, series, point_count) = build_live_timeseries_series(
        pane_id,
        column_states,
        pane_columns,
        view,
        shared_view_end,
        viewport,
    );

    if let Err(err) = emitter.emit_plot_frame(pane_id, PlotMode::Timeseries, view_end, &series) {
        emitter.debug(format!("failed to emit binary timeseries plot: {err}"));
    }

    PlotEmitStats {
        mode: PlotMode::Timeseries,
        series_count: series.len(),
        point_count,
        viewport_end: view_end,
    }
}

fn emit_live_fft_plot(
    emitter: &EventSink,
    pane_id: usize,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
    view: &ViewConfig,
    view_end: Option<f64>,
    fft_worker: &mut FftWorker,
) -> Option<PlotEmitStats> {
    let window_seconds = view.window_seconds.max(1e-6);
    if fft_worker.is_ready_for_request() {
        let request = build_fft_request(
            fft_worker.generation,
            column_states,
            active_columns,
            view,
            view_end,
        );
        fft_worker.submit_request(request, emitter);
    }

    if let Some(result) = fft_worker.take_pending_emit(view_end, window_seconds) {
        let stats = PlotEmitStats {
            mode: PlotMode::Fft,
            series_count: result.series.len(),
            point_count: result.series.iter().map(|series| series.points.len()).sum(),
            viewport_end: result.viewport_end,
        };
        emit_plot_series_frame(
            emitter,
            pane_id,
            PlotMode::Fft,
            result.viewport_end,
            &result.series,
        );
        return Some(stats);
    }

    None
}

fn emit_plot_at(
    emitter: &EventSink,
    pane_id: usize,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
    view: &ViewConfig,
    viewport_end: Option<f64>,
) -> PlotEmitStats {
    let mut series = Vec::new();
    let is_live_latest_view = viewport_end.is_none();
    let view_end = viewport_end.or_else(|| latest_active_x(column_states, active_columns));
    let mut keys: Vec<_> = active_columns.iter().cloned().collect();
    keys.sort_by(|a, b| {
        (&a.route, a.stream_id, a.column_index).cmp(&(&b.route, b.stream_id, b.column_index))
    });

    for key in keys {
        let Some((_, state)) = resolve_column_state(column_states, &key) else {
            continue;
        };
        let label_state = column_states.get(&key).unwrap_or(state);
        let window_seconds = view.window_seconds.max(1e-6);
        let points = match view.mode {
            PlotMode::Timeseries => {
                let window_points = view_end.map(|end| {
                    let start = end - window_seconds;
                    state.points_between(start, end)
                });
                match view.decimation_method {
                    DecimationMethod::Fpcs if is_live_latest_view => {
                        let start = state
                            .raw
                            .back()
                            .map(|point| point.x - view.window_seconds)
                            .unwrap_or(f64::NEG_INFINITY);
                        let points = state
                            .fpcs_for_pane(pane_id)
                            .map(|fpcs| fpcs.points_since(start))
                            .unwrap_or_default();
                        if points.is_empty() {
                            fpcs_decimate_window(
                                &state.recent_points(view.window_seconds),
                                state.sample_rate,
                                view,
                            )
                        } else {
                            points
                        }
                    }
                    DecimationMethod::Fpcs => window_points
                        .as_deref()
                        .map(|points| fpcs_decimate_window(points, state.sample_rate, view))
                        .unwrap_or_else(|| {
                            fpcs_decimate_window(
                                &state.recent_points(view.window_seconds),
                                state.sample_rate,
                                view,
                            )
                        }),
                    DecimationMethod::None => window_points
                        .clone()
                        .unwrap_or_else(|| state.recent_points(view.window_seconds)),
                }
            }
            PlotMode::Fft => {
                let points = state.fft_window_points(window_seconds, view_end);
                fft_points(&points, state.sample_rate, view.detrend)
            }
        };

        if points.is_empty() {
            continue;
        }

        let noise_floor = match view.mode {
            PlotMode::Fft => estimate_white_noise_floor(&points),
            PlotMode::Timeseries => None,
        };

        series.push(PlotSeries {
            key,
            label: label_state.label.clone(),
            units: label_state.units.clone(),
            sample_rate: state.sample_rate,
            noise_floor,
            points,
        });
    }

    let stats = PlotEmitStats {
        mode: view.mode,
        series_count: series.len(),
        point_count: series.iter().map(|series| series.points.len()).sum(),
        viewport_end: view_end,
    };

    emit_plot_series_frame(emitter, pane_id, view.mode, view_end, &series);
    stats
}

fn emit_plot_panes_at(
    emitter: &EventSink,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    plot_panes: &[PlotPaneConfig],
    viewport_end: Option<f64>,
) -> Option<PlotEmitStats> {
    let mut aggregate: Option<PlotEmitStats> = None;

    for pane in plot_panes {
        let pane_columns = pane_columns_set(pane);
        let stats = emit_plot_at(
            emitter,
            pane.id,
            column_states,
            &pane_columns,
            &pane.view,
            viewport_end,
        );
        aggregate = Some(match aggregate {
            Some(current) => PlotEmitStats {
                mode: stats.mode,
                series_count: current.series_count + stats.series_count,
                point_count: current.point_count + stats.point_count,
                viewport_end: stats.viewport_end.or(current.viewport_end),
            },
            None => stats,
        });
    }

    aggregate
}

fn emit_plot_series_frame(
    emitter: &EventSink,
    pane_id: usize,
    mode: PlotMode,
    viewport_end: Option<f64>,
    series: &[PlotSeries],
) {
    let binary_series: Vec<_> = series
        .iter()
        .map(|item| BinaryPlotSeries {
            key: &item.key,
            sample_rate: item.sample_rate,
            points: PlotPointSource::Slice(&item.points),
            outside_window: false,
            noise_floor: item.noise_floor,
        })
        .collect();
    if let Err(err) = emitter.emit_plot_frame(pane_id, mode, viewport_end, &binary_series) {
        emitter.debug(format!("failed to emit binary plot: {err}"));
    }
}

fn build_fft_request(
    generation: u64,
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
    view: &ViewConfig,
    viewport_end: Option<f64>,
) -> FftRequest {
    let mut inputs = Vec::new();
    let window_seconds = view.window_seconds.max(1e-6);
    let mut keys: Vec<_> = active_columns.iter().cloned().collect();
    keys.sort_by(|a, b| {
        (&a.route, a.stream_id, a.column_index).cmp(&(&b.route, b.stream_id, b.column_index))
    });

    for key in keys {
        let Some((_, state)) = resolve_column_state(column_states, &key) else {
            continue;
        };
        let label_state = column_states.get(&key).unwrap_or(state);
        let points = state.fft_window_points(window_seconds, viewport_end);
        if points.len() < 16 || state.sample_rate <= 0.0 {
            continue;
        }
        inputs.push(FftSeriesInput {
            key,
            label: label_state.label.clone(),
            units: label_state.units.clone(),
            sample_rate: state.sample_rate,
            points,
        });
    }

    FftRequest {
        generation,
        viewport_end,
        window_seconds,
        detrend: view.detrend,
        target_points: target_plot_points(view),
        inputs,
    }
}

fn build_view_data_tsv(
    column_states: &HashMap<ColumnKeyDto, ColumnState>,
    active_columns: &HashSet<ColumnKeyDto>,
    view: &ViewConfig,
    viewport_end: Option<f64>,
) -> String {
    let view_end = viewport_end.or_else(|| latest_active_x(column_states, active_columns));
    let mut keys: Vec<_> = active_columns.iter().cloned().collect();
    keys.sort_by(|a, b| {
        (&a.route, a.stream_id, a.column_index).cmp(&(&b.route, b.stream_id, b.column_index))
    });

    let mut series = Vec::new();
    let window_seconds = view.window_seconds.max(1e-6);
    for key in keys {
        let Some((_, state)) = resolve_column_state(column_states, &key) else {
            continue;
        };
        let label_state = column_states.get(&key).unwrap_or(state);
        let points = match view.mode {
            PlotMode::Timeseries => {
                if let Some(end) = view_end {
                    let start = end - window_seconds;
                    state.points_between(start, end)
                } else {
                    state.recent_points(window_seconds)
                }
            }
            PlotMode::Fft => {
                let raw_points = state.fft_window_points(window_seconds, view_end);
                fft_points(&raw_points, state.sample_rate, view.detrend)
            }
        };

        series.push(ViewDataSeries {
            label: label_state.label.clone(),
            points,
        });
    }

    build_wide_view_data_tsv(
        match view.mode {
            PlotMode::Timeseries => "time",
            PlotMode::Fft => "frequency",
        },
        series,
    )
}

fn build_wide_view_data_tsv(x_header: &str, mut series: Vec<ViewDataSeries>) -> String {
    let mut output = String::new();
    output.push_str(x_header);
    for item in &series {
        output.push('\t');
        output.push_str(&escape_tsv(&item.label));
    }
    output.push('\n');

    let mut xs = Vec::new();
    let mut seen_xs = HashSet::new();
    for item in &mut series {
        item.points
            .retain(|point| point.x.is_finite() && point.y.is_finite());
        item.points
            .sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        for point in &item.points {
            if seen_xs.insert(view_data_x_key(point.x)) {
                xs.push(point.x);
            }
        }
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let value_maps: Vec<HashMap<u64, f64>> = series
        .iter()
        .map(|item| {
            item.points
                .iter()
                .map(|point| (view_data_x_key(point.x), point.y))
                .collect()
        })
        .collect();

    for x in xs {
        output.push_str(&format!("{x:.17}"));
        let x_key = view_data_x_key(x);
        for values in &value_maps {
            output.push('\t');
            if let Some(y) = values.get(&x_key) {
                output.push_str(&format!("{y:.17}"));
            }
        }
        output.push('\n');
    }
    output
}

fn view_data_x_key(x: f64) -> u64 {
    if x == 0.0 {
        0.0_f64.to_bits()
    } else {
        x.to_bits()
    }
}

fn emit_view_data(emitter: &EventSink, request_id: String, result: Result<String, String>) {
    match result {
        Ok(text) => {
            let rows = text.lines().count().saturating_sub(1);
            emitter.emit(&json!({
                "type": "viewData",
                "requestId": request_id,
                "ok": true,
                "text": text,
                "rows": rows
            }));
        }
        Err(error) => emitter.emit(&json!({
            "type": "viewData",
            "requestId": request_id,
            "ok": false,
            "error": error
        })),
    }
}

fn emit_export_result(
    emitter: &EventSink,
    request_id: String,
    output_path: String,
    format: ExportFormat,
    result: Result<ExportSummary, String>,
) {
    match result {
        Ok(summary) => emitter.emit(&json!({
            "type": "exportResult",
            "requestId": request_id,
            "ok": true,
            "outputPath": output_path,
            "format": format,
            "rows": summary.rows,
            "bytes": summary.bytes
        })),
        Err(error) => emitter.emit(&json!({
            "type": "exportResult",
            "requestId": request_id,
            "ok": false,
            "outputPath": output_path,
            "format": format,
            "error": error
        })),
    }
}

fn escape_tsv(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn calculate_fft_request(request: FftRequest) -> FftResult {
    let series = request
        .inputs
        .into_iter()
        .filter_map(|input| {
            let points = fft_points(&input.points, input.sample_rate, request.detrend);
            // Estimate before decimating: min/max bucketing preserves the
            // envelope, not the distribution the estimator reasons about.
            let noise_floor = estimate_white_noise_floor(&points);
            let points = decimate_min_max_by_x(&points, request.target_points);
            if points.is_empty() {
                return None;
            }
            Some(PlotSeries {
                key: input.key,
                label: input.label,
                units: input.units,
                sample_rate: input.sample_rate,
                points,
                noise_floor,
            })
        })
        .collect();

    FftResult {
        generation: request.generation,
        viewport_end: request.viewport_end,
        window_seconds: request.window_seconds,
        series,
    }
}

fn decimate_min_max_by_x(points: &[Point], target_points: usize) -> Vec<Point> {
    if points.len() <= target_points || target_points < 4 {
        return points.to_vec();
    }

    let bucket_count = (target_points / 2).max(1);
    let bucket_size = (points.len() as f64 / bucket_count as f64).ceil() as usize;
    let mut output = Vec::with_capacity(target_points);

    for bucket in points.chunks(bucket_size.max(1)) {
        let Some((min_index, min_point)) = bucket
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
        else {
            continue;
        };
        let Some((max_index, max_point)) = bucket
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
        else {
            continue;
        };

        if min_index == max_index {
            output.push(*min_point);
        } else if min_point.x <= max_point.x {
            output.push(*min_point);
            output.push(*max_point);
        } else {
            output.push(*max_point);
            output.push(*min_point);
        }
    }

    output
}

fn fft_points(points: &[Point], sample_rate: f64, detrend: DetrendMethod) -> Vec<Point> {
    if points.len() < 16 || sample_rate <= 0.0 {
        return Vec::new();
    }

    let y: Vec<f64> = points.iter().map(|point| point.y).collect();
    let y = detrended_values(&y, detrend);

    let welch: SpectralDensity<f64> = SpectralDensity::builder(&y, sample_rate).build();
    let psd = welch.periodogram();
    let frequencies = bin_frequencies(psd.len(), welch.dft_size, sample_rate);
    let asd: Vec<f64> = one_sided_amplitude_density(&psd);

    frequencies
        .into_iter()
        .zip(asd)
        .map(|(x, y)| Point { x, y })
        .collect()
}


/// Estimate the flat, broadband ASD level while rejecting low-frequency 1/f
/// content and narrow spectral peaks.
///
/// Equal-sized frequency bands make the band medians insensitive to narrow
/// peaks. The quiet quartile of all but the lowest bands identifies the white
/// plateau without assuming a fixed corner frequency. Only bands close to
/// that plateau contribute samples, and a final one-sided MAD clip removes
/// any peaks that survived their band's median.
fn estimate_white_noise_floor(points: &[Point]) -> Option<f64> {
    let log_values: Vec<f64> = points
        .iter()
        .filter(|point| point.x.is_finite() && point.x > 0.0 && point.y.is_finite() && point.y > 0.0)
        .map(|point| point.y.ln())
        .collect();
    if log_values.len() < 16 {
        return None;
    }

    let band_count = (log_values.len() / 16).clamp(8, 32).min(log_values.len());
    let band_size = log_values.len().div_ceil(band_count);
    let bands: Vec<&[f64]> = log_values.chunks(band_size).collect();
    if bands.len() < 4 {
        return None;
    }

    // Never seed the plateau from the lowest 1/8 of the frequency bands.
    let low_band_count = (bands.len() / 8).max(1);
    let mut usable_band_medians: Vec<f64> = bands[low_band_count..]
        .iter()
        .filter_map(|band| median(band))
        .collect();
    if usable_band_medians.len() < 3 {
        return None;
    }
    usable_band_medians.sort_by(f64::total_cmp);

    // A lower-quartile seed is resistant to both 1/f bands and broad peaks,
    // while remaining representative of a noisy white plateau.
    let plateau_seed = quantile_sorted(&usable_band_medians, 0.25)?;
    let lower_half_end = usable_band_medians.len().div_ceil(2);
    let lower_half = &usable_band_medians[..lower_half_end];
    let band_mad = median_absolute_deviation(lower_half, plateau_seed).unwrap_or(0.0);
    let plateau_tolerance = (3.0 * 1.4826 * band_mad).clamp((1.5_f64).ln(), (2.0_f64).ln());

    let mut candidates = Vec::new();
    for band in &bands[low_band_count..] {
        let Some(band_median) = median(band) else {
            continue;
        };
        if band_median <= plateau_seed + plateau_tolerance {
            candidates.extend_from_slice(band);
        }
    }
    if candidates.len() < 8 {
        return None;
    }

    // Iterative, upper-only clipping preserves the center of the broadband
    // distribution while removing spectral lines at any amplitude.
    for _ in 0..4 {
        let Some(center) = median(&candidates) else {
            return None;
        };
        let mad = median_absolute_deviation(&candidates, center).unwrap_or(0.0);
        let upper_limit = center + (3.5 * 1.4826 * mad).max((1.5_f64).ln());
        let previous_len = candidates.len();
        candidates.retain(|value| *value <= upper_limit);
        if candidates.len() == previous_len || candidates.len() < 8 {
            break;
        }
    }

    median(&candidates).map(f64::exp).filter(|value| value.is_finite() && *value > 0.0)
}

fn median(values: &[f64]) -> Option<f64> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|value| value.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    quantile_sorted(&sorted, 0.5)
}

fn median_absolute_deviation(values: &[f64], center: f64) -> Option<f64> {
    let deviations: Vec<f64> = values.iter().map(|value| (value - center).abs()).collect();
    median(&deviations)
}

fn quantile_sorted(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let position = quantile.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let fraction = position - lower as f64;
    Some(sorted[lower] + (sorted[upper] - sorted[lower]) * fraction)
}

/// Centre frequencies of the bins `welch-sde` returns.
///
/// Its own `frequency()` spreads the `n` bins evenly over `0..=fs/2` as
/// `i * fs / (2 * (n - 1))`, which puts the last bin exactly at Nyquist. But
/// those bins are the first `n = dft_size / 2` DFT bins, spaced `fs / dft_size`
/// apart, so the last one sits one spacing *below* Nyquist. The crate's mapping
/// therefore stretches the whole axis by `n / (n - 1)` — about 0.05% for a
/// 4096-point transform, and proportionally worse for the short transforms a
/// brief window produces.
fn bin_frequencies(bin_count: usize, dft_size: usize, sample_rate: f64) -> Vec<f64> {
    if dft_size == 0 {
        return Vec::new();
    }
    let spacing = sample_rate / dft_size as f64;
    (0..bin_count).map(|index| index as f64 * spacing).collect()
}

/// Convert `welch-sde`'s spectral density to a one-sided amplitude density.
///
/// The crate windows a real signal into the real part of a complex buffer and
/// transforms it, so the resulting spectrum is Hermitian: `X[N-k]` is the
/// conjugate of `X[k]` and the two have identical magnitude. The negative
/// frequencies are a mirror of the positive ones, not independent data — the
/// in-phase and quadrature parts of each bin are already combined by
/// `norm_sqr()`. The crate keeps the first `dft_size / 2` bins and scales them
/// by `1 / (sum(w^2) * n_segment * fs)`, omitting the factor of two that folds
/// the mirror back on. What it returns is therefore a *two-sided* density:
/// integrating it over the frequencies it reports comes to half the signal
/// variance.
///
/// A figure quoted in `units/sqrt(Hz)` is conventionally one-sided, so every
/// bin that has a mirror partner carries twice the power. Only DC is exempt:
/// the Nyquist bin would be too, but `dft_size / 2` bins spans indices
/// `0..dft_size/2 - 1`, so Nyquist (index `dft_size / 2`) is never among them
/// and the last kept bin is an ordinary mirrored one.
fn one_sided_amplitude_density(psd: &[f64]) -> Vec<f64> {
    psd.iter()
        .enumerate()
        .map(|(index, power)| {
            if index == 0 {
                power.sqrt()
            } else {
                (2.0 * power).sqrt()
            }
        })
        .collect()
}

fn detrended_values(y: &[f64], detrend: DetrendMethod) -> Vec<f64> {
    match detrend {
        DetrendMethod::None => y.to_vec(),
        DetrendMethod::Mean => remove_mean(y),
        DetrendMethod::Linear => remove_linear_trend(y),
        DetrendMethod::Quadratic => remove_quadratic_trend(y),
    }
}

fn remove_mean(y: &[f64]) -> Vec<f64> {
    if y.is_empty() {
        return Vec::new();
    }
    let mean = y.iter().sum::<f64>() / y.len() as f64;
    y.iter().map(|value| value - mean).collect()
}

fn remove_linear_trend(y: &[f64]) -> Vec<f64> {
    if y.len() < 2 {
        return remove_mean(y);
    }

    let center = (y.len() as f64 - 1.0) * 0.5;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut sum_x2 = 0.0;
    let mut sum_xy = 0.0;

    for (index, value) in y.iter().enumerate() {
        let x = index as f64 - center;
        sum_x += x;
        sum_y += value;
        sum_x2 += x * x;
        sum_xy += x * value;
    }

    let n = y.len() as f64;
    let denom = n * sum_x2 - sum_x * sum_x;
    if denom.abs() < 1e-12 {
        return remove_mean(y);
    }

    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;

    y.iter()
        .enumerate()
        .map(|(index, value)| {
            let x = index as f64 - center;
            value - (intercept + slope * x)
        })
        .collect()
}

fn remove_quadratic_trend(y: &[f64]) -> Vec<f64> {
    if y.len() < 3 {
        return remove_linear_trend(y);
    }

    let center = (y.len() as f64 - 1.0) * 0.5;
    let mut s0 = 0.0;
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    let mut s3 = 0.0;
    let mut s4 = 0.0;
    let mut sy = 0.0;
    let mut sxy = 0.0;
    let mut sx2y = 0.0;

    for (index, value) in y.iter().enumerate() {
        let x = index as f64 - center;
        let x2 = x * x;
        s0 += 1.0;
        s1 += x;
        s2 += x2;
        s3 += x2 * x;
        s4 += x2 * x2;
        sy += value;
        sxy += x * value;
        sx2y += x2 * value;
    }

    let Some([c0, c1, c2]) = solve_3x3([[s0, s1, s2, sy], [s1, s2, s3, sxy], [s2, s3, s4, sx2y]])
    else {
        return remove_linear_trend(y);
    };

    y.iter()
        .enumerate()
        .map(|(index, value)| {
            let x = index as f64 - center;
            value - (c0 + c1 * x + c2 * x * x)
        })
        .collect()
}

fn solve_3x3(mut matrix: [[f64; 4]; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let mut pivot = col;
        for row in (col + 1)..3 {
            if matrix[row][col].abs() > matrix[pivot][col].abs() {
                pivot = row;
            }
        }

        if matrix[pivot][col].abs() < 1e-12 {
            return None;
        }

        if pivot != col {
            matrix.swap(pivot, col);
        }

        let divisor = matrix[col][col];
        for item in &mut matrix[col][col..4] {
            *item /= divisor;
        }

        for row in 0..3 {
            if row == col {
                continue;
            }
            let factor = matrix[row][col];
            for item in col..4 {
                matrix[row][item] -= factor * matrix[col][item];
            }
        }
    }

    Some([matrix[0][3], matrix[1][3], matrix[2][3]])
}

fn dispatch_rpc(
    proxy: &Arc<proxy::Interface>,
    rpc_index: &HashMap<(String, String), RpcDto>,
    request_id: String,
    route: String,
    name: String,
    arg: Option<Value>,
    emitter: &EventSink,
) {
    let Some(rpc_meta) = rpc_index.get(&(route.clone(), name.clone())).cloned() else {
        emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": false,
            "error": "RPC not found"
        }));
        return;
    };

    if rpc_base_type(&rpc_meta.arg_type) == "capture" {
        let request_id_for_error = request_id.clone();
        let proxy = Arc::clone(proxy);
        let worker_emitter = emitter.clone();
        let error_emitter = emitter.clone();
        if let Err(err) = thread::Builder::new()
            .name("twinleaf-capture-rpc".into())
            .spawn(move || {
                execute_rpc(
                    &proxy,
                    rpc_meta,
                    request_id,
                    route,
                    name,
                    arg,
                    &worker_emitter,
                )
            })
        {
            error_emitter.emit(&json!({
                "type": "rpcResult",
                "requestId": request_id_for_error,
                "ok": false,
                "error": format!("Failed to start capture worker: {err}")
            }));
        }
        return;
    }

    execute_rpc(proxy, rpc_meta, request_id, route, name, arg, emitter);
}

fn execute_rpc(
    proxy: &Arc<proxy::Interface>,
    rpc_meta: RpcDto,
    request_id: String,
    route: String,
    name: String,
    arg: Option<Value>,
    emitter: &EventSink,
) {
    let route_value = match DeviceRoute::from_str(&route) {
        Ok(route) => route,
        Err(_) => {
            emitter.emit(&json!({
                "type": "rpcResult",
                "requestId": request_id,
                "ok": false,
                "error": "Invalid route"
            }));
            return;
        }
    };

    let port = match proxy.device_rpc(route_value) {
        Ok(port) => port,
        Err(err) => {
            emitter.emit(&json!({
                "type": "rpcResult",
                "requestId": request_id,
                "ok": false,
                "error": format!("{err:?}")
            }));
            return;
        }
    };

    if rpc_base_type(&rpc_meta.arg_type) == "capture" {
        execute_capture_rpc(port, request_id, route, name, emitter);
        return;
    }

    let arg_bytes = match json_to_bytes(arg, &rpc_meta.arg_type) {
        Ok(bytes) => bytes,
        Err(err) => {
            emitter.emit(&json!({
                "type": "rpcResult",
                "requestId": request_id,
                "ok": false,
                "error": err
            }));
            return;
        }
    };

    match panic::catch_unwind(AssertUnwindSafe(|| port.raw_rpc(&name, &arg_bytes))) {
        Ok(Ok(reply)) => emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": true,
            "route": route,
            "name": name,
            "value": bytes_to_json_value(&reply, &rpc_meta.arg_type)
        })),
        Ok(Err(err)) => emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": false,
            "route": route,
            "name": name,
            "error": format!("{err:?}")
        })),
        Err(err) => emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": false,
            "route": route,
            "name": name,
            "error": format!("Twinleaf parser panic while calling RPC: {}", panic_message(err))
        })),
    }
}

fn execute_capture_rpc(
    port: proxy::Port,
    request_id: String,
    route: String,
    name: String,
    emitter: &EventSink,
) {
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);
    match panic::catch_unwind(AssertUnwindSafe(|| {
        let capture_rpc = PortCaptureRpc { port: &port };
        read_capture(&capture_rpc, &name, CAPTURE_TIMEOUT)
    })) {
        Ok(Ok(readout)) => emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": true,
            "route": route,
            "name": name,
            "value": capture_readout_to_json_value(&readout)
        })),
        Ok(Err(err)) => emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": false,
            "route": route,
            "name": name,
            "error": format!("{err}")
        })),
        Err(err) => emitter.emit(&json!({
            "type": "rpcResult",
            "requestId": request_id,
            "ok": false,
            "route": route,
            "name": name,
            "error": format!("Twinleaf parser panic while calling RPC: {}", panic_message(err))
        })),
    }
}

struct PortCaptureRpc<'a> {
    port: &'a proxy::Port,
}

impl CaptureRpc for PortCaptureRpc<'_> {
    fn capture_raw_rpc(&self, name: &str, arg: &[u8]) -> Result<Vec<u8>, proxy::RpcError> {
        self.port.raw_rpc(name, arg)
    }
}

fn capture_readout_to_json_value(readout: &CaptureReadout) -> Value {
    use std::fmt::Write as _;

    let metadata = &readout.metadata;
    let mut text = String::new();
    let mut points = Vec::new();
    let _ = writeln!(text, "# capture {}", metadata.name);
    let _ = writeln!(text, "# y {} ({})", metadata.name, metadata.units);
    let _ = writeln!(text, "# x {} ({})", metadata.x_name, metadata.x_units);
    let _ = writeln!(
        text,
        "# data_type={} length={} size={} blocksize={} y_calibration={}",
        metadata.data_type_label(),
        metadata.length,
        metadata.size,
        metadata.blocksize,
        metadata.y_calibration
    );

    match readout.values_f64() {
        Ok(values) => {
            for (index, y) in values.into_iter().enumerate() {
                let x = metadata.x_value_f64(index);
                points.push(json!([x, y]));
                let _ = writeln!(text, "{}\t{}", x, y);
            }
        }
        Err(err) => {
            let _ = writeln!(text, "# failed to decode capture values: {err}");
        }
    }

    json!({
        "encoding": "capture",
        "bytes": readout.data.len(),
        "text": text,
        "points": points,
        "metadata": {
            "name": &metadata.name,
            "units": &metadata.units,
            "xName": &metadata.x_name,
            "xUnits": &metadata.x_units,
            "dataType": metadata.data_type_label(),
            "length": metadata.length,
            "size": metadata.size,
            "blocksize": metadata.blocksize,
            "yCalibration": metadata.y_calibration,
            "xOffset": metadata.x_offset,
            "xStride": metadata.x_stride
        }
    })
}

fn rpc_base_type(rpc_type: &str) -> &str {
    rpc_type.split('<').next().unwrap_or(rpc_type)
}

fn rpc_arg_type(descriptor: &RpcDescriptor) -> String {
    if descriptor.meta.is_unknown() {
        return if is_capture_rpc_descriptor(descriptor) {
            "capture".to_string()
        } else {
            "missing".to_string()
        };
    }

    if is_capture_rpc_descriptor(descriptor) {
        return "capture".to_string();
    }

    if descriptor.meta.flags().contains(RpcMetaFlags::BOOL) {
        return descriptor.meta.type_str();
    }

    match descriptor.meta.kind() {
        RpcValueType::Unit => "unit".to_string(),
        RpcValueType::Raw { .. } => "missing".to_string(),
        _ => descriptor.meta.type_str(),
    }
}

fn rpc_size(descriptor: &RpcDescriptor) -> usize {
    if is_capture_rpc_descriptor(descriptor) {
        return 0;
    }

    match descriptor.meta.kind() {
        RpcValueType::String { max_len } => max_len.map(usize::from).unwrap_or(0),
        _ => descriptor.meta.size_bytes().unwrap_or(0),
    }
}

fn rpc_permissions(descriptor: &RpcDescriptor) -> String {
    if descriptor.meta.is_unknown() && is_capture_rpc_descriptor(descriptor) {
        "R--".to_string()
    } else {
        descriptor.meta.perm_str()
    }
}

fn rpc_readable(descriptor: &RpcDescriptor) -> bool {
    descriptor.meta.flags().contains(RpcMetaFlags::READABLE)
        || is_capture_rpc_descriptor(descriptor)
}

fn rpc_unknown(descriptor: &RpcDescriptor) -> bool {
    descriptor.meta.is_unknown() && !is_capture_rpc_descriptor(descriptor)
}

fn is_capture_rpc_descriptor(descriptor: &RpcDescriptor) -> bool {
    descriptor.meta.flags().contains(RpcMetaFlags::CAPTURE)
        || is_capture_rpc_name(&descriptor.full_name)
}

fn is_capture_rpc_name(name: &str) -> bool {
    name.rsplit('.').next() == Some("capture")
}

fn bytes_to_json_value(reply_bytes: &[u8], rpc_type: &str) -> Option<Value> {
    let base_type = rpc_type.split('<').next().unwrap_or(rpc_type);
    if reply_bytes.is_empty() {
        return match base_type {
            "string" => Some(json!("")),
            _ => Some(Value::Null),
        };
    }

    match base_type {
        "unit" | "" | "missing" => Some(Value::Null),
        "bool" => u8::from_reply(reply_bytes)
            .ok()
            .map(|value| json!(value != 0)),
        "u8" => u8::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "u16" => u16::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "u32" => u32::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "u64" => u64::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "i8" => i8::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "i16" => i16::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "i32" => i32::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "i64" => i64::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "f32" => f32::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "f64" => f64::from_reply(reply_bytes).ok().map(|value| json!(value)),
        "string" => {
            let string_bytes = reply_bytes
                .split(|byte| *byte == 0)
                .next()
                .unwrap_or(reply_bytes);
            Some(json!(String::from_utf8_lossy(string_bytes).to_string()))
        }
        _ => None,
    }
}

fn json_to_bytes(arg: Option<Value>, rpc_type: &str) -> Result<Vec<u8>, String> {
    let Some(arg) = arg else {
        return Ok(Vec::new());
    };

    match rpc_type.split('<').next().unwrap_or(rpc_type) {
        "unit" | "" | "missing" => Ok(Vec::new()),
        "bool" => arg
            .as_bool()
            .map(|value| vec![u8::from(value)])
            .ok_or_else(|| "Expected a bool".to_string()),
        "string" => arg
            .as_str()
            .map(|value| value.as_bytes().to_vec())
            .ok_or_else(|| "Expected a string".to_string()),
        "u8" => arg
            .as_u64()
            .map(|value| (value as u8).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected a u8".to_string()),
        "u16" => arg
            .as_u64()
            .map(|value| (value as u16).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected a u16".to_string()),
        "u32" => arg
            .as_u64()
            .map(|value| (value as u32).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected a u32".to_string()),
        "u64" => arg
            .as_u64()
            .map(|value| value.to_le_bytes().to_vec())
            .ok_or_else(|| "Expected a u64".to_string()),
        "i8" => arg
            .as_i64()
            .map(|value| (value as i8).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected an i8".to_string()),
        "i16" => arg
            .as_i64()
            .map(|value| (value as i16).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected an i16".to_string()),
        "i32" => arg
            .as_i64()
            .map(|value| (value as i32).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected an i32".to_string()),
        "i64" => arg
            .as_i64()
            .map(|value| value.to_le_bytes().to_vec())
            .ok_or_else(|| "Expected an i64".to_string()),
        "f32" => arg
            .as_f64()
            .map(|value| (value as f32).to_le_bytes().to_vec())
            .ok_or_else(|| "Expected an f32".to_string()),
        "f64" => arg
            .as_f64()
            .map(|value| value.to_le_bytes().to_vec())
            .ok_or_else(|| "Expected an f64".to_string()),
        other => Err(format!("Unsupported RPC argument type: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Box-Muller over a xorshift stream: repeatable normal noise for tests
    /// without pulling in an RNG dependency.
    fn gaussian_noise(seed: u64) -> impl FnMut() -> f64 {
        let mut state = seed;
        move || {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 11) as f64 / (1u64 << 53) as f64
            };
            let u1: f64 = next().max(1e-12);
            let u2: f64 = next();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    #[test]
    fn noise_floor_matches_injected_level_and_rejects_a_tone() {
        // A strong tone on top of known white noise: the estimate must track
        // the broadband level, not the peak.
        let sample_rate: f64 = 1000.0;
        let target_asd = 0.02_f64;
        let sigma = target_asd * (sample_rate / 2.0).sqrt();
        let mut gauss = gaussian_noise(0x2545F4914F6CDD1D);

        let points: Vec<Point> = (0..20_000)
            .map(|index| {
                let t = index as f64 / sample_rate;
                Point {
                    x: t,
                    y: (std::f64::consts::TAU * 60.0 * t).sin() + sigma * gauss(),
                }
            })
            .collect();

        let spectrum = fft_points(&points, sample_rate, DetrendMethod::Quadratic);
        let estimate = estimate_white_noise_floor(&spectrum).expect("estimate");
        let ratio = estimate / target_asd;
        assert!(
            (0.95..1.05).contains(&ratio),
            "noise floor drifted from the injected level: {ratio}"
        );

        // The tone is an order of magnitude above the floor; the estimate must
        // not be pulled anywhere near it.
        let peak = spectrum
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            estimate < peak / 10.0,
            "estimate {estimate} was pulled toward the {peak} peak"
        );
    }

    #[test]
    fn spectrum_axis_is_spaced_by_fs_over_dft_size() {
        // Bins are spaced `fs / dft_size` and the last sits one spacing below
        // Nyquist. `welch-sde`'s own `frequency()` instead spreads them to land
        // exactly on Nyquist, stretching every frequency by `n / (n - 1)`.
        let sample_rate: f64 = 1000.0;
        let points: Vec<Point> = (0..16_384)
            .map(|index| {
                let t = index as f64 / sample_rate;
                Point {
                    x: t,
                    y: (std::f64::consts::TAU * 400.0 * t).sin(),
                }
            })
            .collect();

        let spectrum = fft_points(&points, sample_rate, DetrendMethod::None);
        let dft_size = 2 * spectrum.len();
        let expected_spacing = sample_rate / dft_size as f64;

        let spacing = spectrum[1].x - spectrum[0].x;
        assert!(
            (spacing - expected_spacing).abs() < 1e-9,
            "bin spacing {spacing}, expected {expected_spacing}"
        );
        assert_eq!(spectrum[0].x, 0.0, "first bin is DC");

        let last = spectrum[spectrum.len() - 1].x;
        let expected_last = sample_rate / 2.0 - expected_spacing;
        assert!(
            (last - expected_last).abs() < 1e-9,
            "last bin {last}, expected {expected_last} (one spacing below Nyquist)"
        );
    }

    #[test]
    fn every_bin_but_dc_is_folded(){
        // Only DC is exempt from the mirror fold: `welch-sde` keeps
        // `dft_size / 2` bins, spanning indices 0..dft_size/2 - 1, so Nyquist
        // (index dft_size/2) is never among them and the last kept bin is an
        // ordinary mirrored one.
        let folded = one_sided_amplitude_density(&[4.0, 4.0, 4.0, 4.0]);
        assert_eq!(folded[0], 2.0, "DC must not be doubled");
        for (index, value) in folded.iter().enumerate().skip(1) {
            assert!(
                (value - 8.0_f64.sqrt()).abs() < 1e-12,
                "bin {index} was not folded: {value}"
            );
        }
    }

    #[test]
    fn fft_points_returns_a_one_sided_spectral_density() {
        // For white noise the integral of a one-sided PSD over frequency
        // equals the signal's variance. The underlying Welch implementation
        // returns the two-sided form, so `fft_points` folds it; without that
        // every displayed ASD would sit a factor of sqrt(2) below the value a
        // `units/sqrt(Hz)` figure denotes.
        let sample_rate: f64 = 1000.0;
        let mut gauss = gaussian_noise(0x9E3779B97F4A7C15);
        let points: Vec<Point> = (0..20_000)
            .map(|index| Point {
                x: index as f64 / sample_rate,
                y: 0.5 * gauss(),
            })
            .collect();

        let mean = points.iter().map(|point| point.y).sum::<f64>() / points.len() as f64;
        let variance = points
            .iter()
            .map(|point| (point.y - mean).powi(2))
            .sum::<f64>()
            / points.len() as f64;

        let spectrum = fft_points(&points, sample_rate, DetrendMethod::None);
        let df = spectrum[1].x - spectrum[0].x;
        let integrated: f64 = spectrum.iter().map(|point| point.y * point.y * df).sum();

        let ratio = integrated / variance;
        assert!(
            (0.97..1.03).contains(&ratio),
            "spectral density convention changed: integral/variance = {ratio}"
        );
    }

    #[test]
    fn spectrum_level_matches_injected_white_noise() {
        // A tone on top of known white noise: the broadband bins must sit at
        // the injected amplitude density regardless of the peak.
        let sample_rate: f64 = 1000.0;
        let target_asd = 0.02_f64;
        let sigma = target_asd * (sample_rate / 2.0).sqrt();
        let mut gauss = gaussian_noise(0x2545F4914F6CDD1D);

        let points: Vec<Point> = (0..20_000)
            .map(|index| {
                let t = index as f64 / sample_rate;
                Point {
                    x: t,
                    y: (std::f64::consts::TAU * 60.0 * t).sin() + sigma * gauss(),
                }
            })
            .collect();

        let spectrum = fft_points(&points, sample_rate, DetrendMethod::Quadratic);
        let broadband: Vec<f64> = spectrum
            .iter()
            .filter(|point| point.x > 100.0 && point.x < 400.0 && point.y.is_finite())
            .map(|point| point.y)
            .collect();
        let rms = (broadband.iter().map(|asd| asd * asd).sum::<f64>()
            / broadband.len() as f64)
            .sqrt();

        let ratio = rms / target_asd;
        assert!(
            (0.97..1.03).contains(&ratio),
            "spectrum level drifted from the injected noise: {ratio}"
        );
    }

    #[test]
    fn data_range_since_uses_first_real_sample_not_the_reference() {
        let mut state = ColumnState::new("a".to_string(), "V".to_string(), 1.0);
        // Pre-sync points near 0, then real data starting at 1000. The reference
        // (990) sits below the first real sample — like a projected sample-0 time.
        for x in [0.0, 1.0, 2.0, 1000.0, 1001.0, 1002.0] {
            state.push_raw(Point { x, y: 0.0 }, 1.0e12);
        }
        let mut states = HashMap::new();
        states.insert(
            ColumnKeyDto {
                route: "/".to_string(),
                stream_id: 0,
                column_index: 0,
            },
            state,
        );
        // At/after the reference: starts at the first *real* sample (1000), not
        // the reference (990) and not the pre-sync 0.
        assert_eq!(
            data_range_since(&states, Some(990.0)),
            Some((1000.0, 1002.0))
        );
        // No reference → full range, pre-sync included.
        assert_eq!(data_range_since(&states, None), Some((0.0, 1002.0)));
        // Empty column states → no range.
        assert_eq!(data_range_since(&HashMap::new(), Some(5.0)), None);
    }

    #[test]
    fn decodes_camel_case_rpc_command_fields() {
        let command: CoreCommand = serde_json::from_value(json!({
            "type": "callRpc",
            "requestId": "req-1",
            "route": "/",
            "name": "device.name",
            "arg": null
        }))
        .unwrap();

        match command {
            CoreCommand::CallRpc {
                request_id,
                route,
                name,
                arg,
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(route, "/");
                assert_eq!(name, "device.name");
                assert_eq!(arg, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn decodes_other_camel_case_command_fields() {
        let command: CoreCommand = serde_json::from_value(json!({
            "type": "copyViewData",
            "requestId": "copy-1",
            "viewportEnd": 12.5
        }))
        .unwrap();

        match command {
            CoreCommand::CopyViewData {
                request_id,
                pane_id,
                viewport_end,
            } => {
                assert_eq!(request_id, "copy-1");
                assert_eq!(pane_id, None);
                assert_eq!(viewport_end, Some(12.5));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let command: CoreCommand = serde_json::from_value(json!({
            "type": "exportLog",
            "requestId": "export-1",
            "sourcePath": "/tmp/source.tio",
            "outputPath": "/tmp/output.csv",
            "format": "csv"
        }))
        .unwrap();

        match command {
            CoreCommand::ExportLog {
                request_id,
                source_path,
                output_path,
                format,
            } => {
                assert_eq!(request_id, "export-1");
                assert_eq!(source_path, "/tmp/source.tio");
                assert_eq!(output_path, "/tmp/output.csv");
                assert_eq!(format, ExportFormat::Csv);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    use twinleaf::device::RpcMetaFlags;

    #[test]
    fn maps_rpc_descriptors_to_bridge_metadata() {
        let missing = RpcDescriptor::from_meta(0, "missing.rpc".to_string());
        assert_eq!(rpc_arg_type(&missing), "missing");
        assert_eq!(rpc_size(&missing), 0);
        assert_eq!(missing.meta.perm_str(), "???");
        assert!(missing.meta.is_unknown());

        let unsupported = RpcDescriptor::from_meta(
            RpcMetaFlags::READABLE.bits() | (1 << 4) | 2,
            "bad.float".to_string(),
        );
        assert_eq!(rpc_arg_type(&unsupported), "missing");
        assert_eq!(rpc_size(&unsupported), 0);
        assert_eq!(unsupported.meta.perm_str(), "R--");

        let enabled = RpcDescriptor::from_meta(
            RpcMetaFlags::READABLE.bits()
                | RpcMetaFlags::WRITABLE.bits()
                | (1 << 4)
                | RpcMetaFlags::BOOL.bits(),
            "test.enable".to_string(),
        );
        assert_eq!(rpc_arg_type(&enabled), "bool");
        assert_eq!(rpc_size(&enabled), 1);
        assert_eq!(enabled.meta.perm_str(), "RW-");
        assert!(enabled.meta.flags().contains(RpcMetaFlags::READABLE));
        assert!(enabled.meta.flags().contains(RpcMetaFlags::WRITABLE));

        let action = RpcDescriptor::from_meta(RpcMetaFlags::WRITABLE.bits(), "test.go".to_string());
        assert_eq!(rpc_arg_type(&action), "unit");
        assert_eq!(rpc_size(&action), 0);
        assert_eq!(action.meta.perm_str(), "-W-");

        let capture = RpcDescriptor::from_meta(
            RpcMetaFlags::READABLE.bits() | RpcMetaFlags::CAPTURE.bits(),
            "test.capture".to_string(),
        );
        assert_eq!(rpc_arg_type(&capture), "capture");
        assert_eq!(rpc_size(&capture), 0);
        assert_eq!(rpc_permissions(&capture), "R--");
        assert!(rpc_readable(&capture));
        assert!(!rpc_unknown(&capture));

        let legacy_capture = RpcDescriptor::from_meta(0, "test.capture".to_string());
        assert_eq!(rpc_arg_type(&legacy_capture), "capture");
        assert_eq!(rpc_size(&legacy_capture), 0);
        assert_eq!(rpc_permissions(&legacy_capture), "R--");
        assert!(rpc_readable(&legacy_capture));
        assert!(!rpc_unknown(&legacy_capture));

        let string = RpcDescriptor::from_meta(
            RpcMetaFlags::READABLE.bits() | (8 << 4) | 3,
            "device.name".to_string(),
        );
        assert_eq!(rpc_arg_type(&string), "string<8>");
        assert_eq!(rpc_size(&string), 8);
        assert_eq!(string.meta.perm_str(), "R--");
    }

    #[test]
    fn flags_series_outside_the_anchored_time_window() {
        let key_a = ColumnKeyDto {
            route: "/0".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let key_b = ColumnKeyDto {
            route: "/1".to_string(),
            stream_id: 1,
            column_index: 0,
        };

        // Stream A's clock is near x=1000; stream B's is near x=20: their
        // time references are not compatible within the 10 s default window.
        let mut state_a = ColumnState::new("a".to_string(), "V".to_string(), 1.0);
        let mut state_b = ColumnState::new("b".to_string(), "V".to_string(), 1.0);
        for i in 0..20 {
            state_a.push_raw(
                Point {
                    x: 1000.0 + i as f64,
                    y: 1.0,
                },
                1e9,
            );
            state_b.push_raw(
                Point {
                    x: 5.0 + i as f64,
                    y: 2.0,
                },
                1e9,
            );
        }
        let states = HashMap::from([(key_a.clone(), state_a), (key_b.clone(), state_b)]);
        let view = ViewConfig::default();

        // Anchored to the first pane column (stream A).
        let pane = vec![key_a.clone(), key_b.clone()];
        let (view_end, series, _) =
            build_live_timeseries_series(0, &states, &pane, &view, None, None);
        assert_eq!(view_end, Some(1019.0));
        assert_eq!(series.len(), 2);
        assert!(!series[0].outside_window);
        assert!(series[0].points.len() > 0);
        assert!(series[1].outside_window);
        assert_eq!(series[1].points.len(), 0);

        // Swapping the pane order anchors to stream B instead.
        let pane = vec![key_b.clone(), key_a.clone()];
        let (view_end, series, _) =
            build_live_timeseries_series(0, &states, &pane, &view, None, None);
        assert_eq!(view_end, Some(24.0));
        assert!(!series[0].outside_window);
        assert!(series[0].points.len() > 0);
        assert!(series[1].outside_window);
        assert_eq!(series[1].points.len(), 0);
    }

    #[test]
    fn plots_series_with_compatible_time_references_together() {
        let key_a = ColumnKeyDto {
            route: "/0".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let key_b = ColumnKeyDto {
            route: "/1".to_string(),
            stream_id: 1,
            column_index: 0,
        };

        // Clocks differ by a couple of seconds: both fit the 10 s window.
        let mut state_a = ColumnState::new("a".to_string(), "V".to_string(), 1.0);
        let mut state_b = ColumnState::new("b".to_string(), "V".to_string(), 1.0);
        for i in 0..20 {
            state_a.push_raw(
                Point {
                    x: 1000.0 + i as f64,
                    y: 1.0,
                },
                1e9,
            );
            state_b.push_raw(
                Point {
                    x: 1002.5 + i as f64,
                    y: 2.0,
                },
                1e9,
            );
        }
        let states = HashMap::from([(key_a.clone(), state_a), (key_b.clone(), state_b)]);
        let view = ViewConfig::default();

        let pane = vec![key_a.clone(), key_b.clone()];
        let (view_end, series, _) =
            build_live_timeseries_series(0, &states, &pane, &view, None, None);
        assert_eq!(view_end, Some(1019.0));
        assert_eq!(series.len(), 2);
        assert!(!series[0].outside_window);
        assert!(!series[1].outside_window);
        assert!(series[0].points.len() > 0);
        assert!(series[1].points.len() > 0);
    }

    #[test]
    fn decodes_string_rpc_replies() {
        assert_eq!(
            bytes_to_json_value(b"SN123\0\0\0", "string<8>"),
            Some(json!("SN123"))
        );
        assert_eq!(bytes_to_json_value(b"", "string"), Some(json!("")));
    }

    #[test]
    fn encodes_and_decodes_bool_metadata_as_uint8() {
        assert_eq!(bytes_to_json_value(&[0], "bool"), Some(json!(false)));
        assert_eq!(bytes_to_json_value(&[7], "bool"), Some(json!(true)));
        assert_eq!(json_to_bytes(Some(json!(false)), "bool").unwrap(), vec![0]);
        assert_eq!(json_to_bytes(Some(json!(true)), "bool").unwrap(), vec![1]);
    }

    #[test]
    fn wraps_capture_readout_for_json_results() {
        let mut data = Vec::new();
        data.extend(1.5f32.to_le_bytes());
        data.extend((-2.0f32).to_le_bytes());

        let readout = CaptureReadout {
            metadata: twinleaf::device::capture::CaptureMetadata {
                size: data.len() as u32,
                blocksize: 8,
                data_type: twinleaf::tio::proto::DataType::Float32,
                length: 2,
                y_calibration: 1.0,
                x_offset: 10.0,
                x_stride: 0.5,
                name: "test.capture".to_string(),
                units: "V".to_string(),
                x_name: "time".to_string(),
                x_units: "s".to_string(),
            },
            data,
        };

        let value = capture_readout_to_json_value(&readout);
        assert_eq!(value["encoding"], json!("capture"));
        assert_eq!(value["bytes"], json!(8));
        assert_eq!(value["metadata"]["name"], json!("test.capture"));
        assert_eq!(value["metadata"]["dataType"], json!("f32"));
        assert!(value["text"].as_str().unwrap().contains("10\t1.5"));
        assert!(value["text"].as_str().unwrap().contains("10.5\t-2"));
    }

    #[test]
    fn resolves_active_column_to_unique_live_route_fallback() {
        let active_key = ColumnKeyDto {
            route: "/2".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let live_key = ColumnKeyDto {
            route: "/".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let mut live_state = ColumnState::new("live".to_string(), "V".to_string(), 100.0);
        live_state.push_raw(Point { x: 1.0, y: 2.0 }, 10.0);

        let mut column_states = HashMap::new();
        column_states.insert(
            active_key.clone(),
            ColumnState::new("metadata".to_string(), "V".to_string(), 100.0),
        );
        column_states.insert(live_key.clone(), live_state);

        let resolved_key = resolve_data_key(&column_states, &active_key);
        assert_eq!(resolved_key, Some(live_key));
    }

    #[test]
    fn column_state_extracts_points_by_time_range() {
        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 100.0);
        for index in 0..10 {
            let value = index as f64;
            state.push_raw(Point { x: value, y: value }, 20.0);
        }

        let points = state.points_between(3.0, 6.0);
        assert_eq!(points.len(), 4);
        assert_eq!(points.first().map(|point| point.x), Some(3.0));
        assert_eq!(points.last().map(|point| point.x), Some(6.0));

        let recent = state.recent_points(2.0);
        assert_eq!(recent.first().map(|point| point.x), Some(7.0));
        assert_eq!(recent.last().map(|point| point.x), Some(9.0));
    }

    #[test]
    fn fft_window_uses_constant_sample_count() {
        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 10.0);
        for index in 0..=100 {
            let x = index as f64 * 0.1;
            state.push_raw(Point { x, y: x }, 20.0);
        }

        let points = state.fft_window_points(2.0, Some(6.05));
        assert_eq!(points.len(), 20);
        assert!((points.first().unwrap().x - 4.1).abs() < 1e-12);
        assert!((points.last().unwrap().x - 6.0).abs() < 1e-12);

        let scrolled_points = state.fft_window_points(2.0, Some(6.14));
        assert_eq!(scrolled_points.len(), 20);
        assert!((scrolled_points.first().unwrap().x - 4.2).abs() < 1e-12);
        assert!((scrolled_points.last().unwrap().x - 6.1).abs() < 1e-12);
    }

    #[test]
    fn fft_request_uses_displayed_time_window() {
        let key = ColumnKeyDto {
            route: "/".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 1.0);
        for index in 0..=100 {
            let value = index as f64;
            state.push_raw(Point { x: value, y: value }, 200.0);
        }

        let column_states = HashMap::from([(key.clone(), state)]);
        let active_columns = HashSet::from([key]);
        let mut view = ViewConfig::default();
        view.window_seconds = 20.0;

        let request = build_fft_request(7, &column_states, &active_columns, &view, Some(60.0));
        assert_eq!(request.generation, 7);
        assert_eq!(request.window_seconds, 20.0);
        assert_eq!(request.inputs.len(), 1);
        assert_eq!(request.inputs[0].points.len(), 20);
        assert_eq!(
            request.inputs[0].points.first().map(|point| point.x),
            Some(41.0)
        );
        assert_eq!(
            request.inputs[0].points.last().map(|point| point.x),
            Some(60.0)
        );

        let live_request = build_fft_request(8, &column_states, &active_columns, &view, None);
        assert_eq!(live_request.window_seconds, 20.0);
        assert_eq!(live_request.inputs[0].points.len(), 20);
        assert_eq!(
            live_request.inputs[0].points.first().map(|point| point.x),
            Some(81.0)
        );
        assert_eq!(
            live_request.inputs[0].points.last().map(|point| point.x),
            Some(100.0)
        );
    }

    #[test]
    fn live_retention_tracks_display_window() {
        let mut view = ViewConfig::default();
        view.window_seconds = 10.0;
        assert_eq!(live_retention_seconds(&view), 15.0);

        view.window_seconds = 120.0;
        assert_eq!(live_retention_seconds(&view), 180.0);
    }

    #[test]
    fn configuring_streaming_fpcs_expands_default_capacity() {
        let mut view = ViewConfig::default();
        view.window_seconds = 10.0;
        view.plot_width_pixels = 200;
        view.resolution_multiplier = 100;

        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 1.0);
        assert!(state.fpcs_for_pane(0).is_none());

        state.ensure_fpcs_configured(0, &view);
        assert_eq!(
            state.fpcs_for_pane(0).unwrap().capacity,
            target_plot_points(&view)
        );

        for index in 0..100 {
            let point = Point {
                x: index as f64 * 0.01,
                y: index as f64,
            };
            state.push_raw(point, live_retention_seconds(&view));
            state.process_fpcs_point(0, &view, point);
        }

        assert!(state.fpcs_for_pane(0).unwrap().points_since(0.0).len() > 2);
    }

    #[test]
    fn hydrate_fpcs_drops_cache_when_pane_leaves_timeseries() {
        let key = ColumnKeyDto {
            route: "/".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let mut view = ViewConfig::default();
        view.window_seconds = 10.0;
        view.plot_width_pixels = 200;
        view.resolution_multiplier = 100;
        view.decimation_method = DecimationMethod::Fpcs;

        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 1.0);
        for index in 0..5 {
            let point = Point {
                x: index as f64,
                y: index as f64,
            };
            state.push_raw(point, live_retention_seconds(&view));
            state.process_fpcs_point(0, &view, point);
        }
        assert!(state.fpcs_for_pane(0).is_some());

        let mut column_states = HashMap::from([(key.clone(), state)]);
        let (event_tx, _event_rx) = async_channel::unbounded();
        let emitter = EventSink::new(event_tx);
        let fft_pane = PlotPaneConfig {
            id: 0,
            view: ViewConfig {
                mode: PlotMode::Fft,
                ..view.clone()
            },
            columns: vec![key.clone()],
            viewport_end: None,
        };
        hydrate_fpcs(&mut column_states, &[fft_pane], &emitter);
        assert!(column_states.get(&key).unwrap().fpcs_for_pane(0).is_none());

        for index in 5..10 {
            let point = Point {
                x: index as f64,
                y: index as f64,
            };
            column_states
                .get_mut(&key)
                .unwrap()
                .push_raw(point, live_retention_seconds(&view));
        }

        let timeseries_pane = PlotPaneConfig {
            id: 0,
            view: view.clone(),
            columns: vec![key.clone()],
            viewport_end: None,
        };
        hydrate_fpcs(&mut column_states, &[timeseries_pane], &emitter);
        let points = column_states
            .get(&key)
            .unwrap()
            .fpcs_for_pane(0)
            .unwrap()
            .points_since(0.0);
        assert!(points.iter().any(|point| point.x >= 5.0));
    }

    #[test]
    fn reselecting_a_column_rebuilds_fpcs_without_a_gap() {
        let key = ColumnKeyDto {
            route: "/".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let mut view = ViewConfig::default();
        view.window_seconds = 10.0;
        view.plot_width_pixels = 200;
        view.resolution_multiplier = 100;
        view.decimation_method = DecimationMethod::Fpcs;
        let retention = live_retention_seconds(&view);

        // Column selected: raw and FPCS ring fill together for x = 0..5.
        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 1.0);
        for index in 0..5 {
            let point = Point {
                x: index as f64,
                y: index as f64,
            };
            state.push_raw(point, retention);
            state.process_fpcs_point(0, &view, point);
        }

        let mut column_states = HashMap::from([(key.clone(), state)]);
        let (event_tx, _event_rx) = async_channel::unbounded();
        let emitter = EventSink::new(event_tx);

        // Unselect: the pane stays timeseries+FPCS but no longer lists the
        // column, so its ring is retained (frozen), not dropped.
        let empty_pane = PlotPaneConfig {
            id: 0,
            view: view.clone(),
            columns: vec![],
            viewport_end: None,
        };
        hydrate_fpcs(&mut column_states, &[empty_pane], &emitter);

        // The raw buffer keeps filling while unselected (process_sample pushes
        // every column), trimming the original x = 0..5 out of retention.
        for index in 5..60 {
            let point = Point {
                x: index as f64,
                y: index as f64,
            };
            column_states
                .get_mut(&key)
                .unwrap()
                .push_raw(point, retention);
        }

        // Reselect.
        let pane = PlotPaneConfig {
            id: 0,
            view: view.clone(),
            columns: vec![key.clone()],
            viewport_end: None,
        };
        hydrate_fpcs(&mut column_states, &[pane], &emitter);

        let state = column_states.get(&key).unwrap();
        let oldest_raw = state.raw.front().unwrap().x;
        let ring_points = state.fpcs_for_pane(0).unwrap().points_since(-1.0);
        assert!(!ring_points.is_empty());
        // The ring must be rebuilt from the continuous raw buffer — it must not
        // retain stale points from before the column was unselected (those were
        // trimmed out of raw), which would leave a gap in the trace.
        assert!(
            ring_points.iter().all(|point| point.x >= oldest_raw),
            "FPCS ring kept stale pre-unselect points: oldest_raw={oldest_raw}, ring_xs={:?}",
            ring_points.iter().map(|point| point.x).collect::<Vec<_>>()
        );
    }

    #[test]
    fn min_max_decimator_caps_points_and_keeps_peaks() {
        let points: Vec<_> = (0..100)
            .map(|index| Point {
                x: index as f64,
                y: if index == 42 { 100.0 } else { index as f64 },
            })
            .collect();

        let decimated = decimate_min_max_by_x(&points, 10);
        assert!(decimated.len() <= 10);
        assert!(decimated
            .iter()
            .any(|point| point.x == 42.0 && point.y == 100.0));
    }

    #[test]
    fn column_state_tracks_smoothed_display_value() {
        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 100.0);
        state.push_raw(Point { x: 0.0, y: 0.0 }, 10.0);
        state.push_raw(Point { x: 0.01, y: 100.0 }, 10.0);

        let value = state.display_value.unwrap();
        assert!(value > 0.0);
        assert!(value < 100.0);
    }

    #[test]
    fn column_state_computes_smoothed_display_value_for_view_window() {
        let mut state = ColumnState::new("signal".to_string(), "V".to_string(), 100.0);
        for index in 0..5 {
            let value = index as f64 * 100.0;
            state.push_raw(
                Point {
                    x: index as f64,
                    y: value,
                },
                10.0,
            );
        }

        let view_value = state.display_value_between(2.0, 4.0).unwrap();
        assert!(view_value > 200.0);
        assert!(view_value < 400.0);

        let later_view_value = state.display_value_between(4.0, 4.0).unwrap();
        assert_eq!(later_view_value, 400.0);
    }

    #[test]
    fn sorts_device_routes_with_leaves_first_and_hub_last() {
        let mut devices = vec![
            test_device("/"),
            test_device("/hub/leaf-b"),
            test_device("/solo"),
            test_device("/hub"),
            test_device("/hub/leaf-a"),
        ];

        sort_devices_leaf_first_with_root(&mut devices, Some("/"));

        let routes: Vec<_> = devices.iter().map(|device| device.route.as_str()).collect();
        assert_eq!(
            routes,
            vec!["/hub/leaf-a", "/hub/leaf-b", "/solo", "/hub", "/"]
        );
    }

    #[test]
    fn sorts_requested_root_route_last() {
        let mut devices = vec![
            test_device("/hub"),
            test_device("/hub/leaf"),
            test_device("/hub/other"),
        ];

        sort_devices_leaf_first_with_root(&mut devices, Some("/hub"));

        let routes: Vec<_> = devices.iter().map(|device| device.route.as_str()).collect();
        assert_eq!(routes, vec!["/hub/leaf", "/hub/other", "/hub"]);
    }

    #[test]
    fn copied_view_data_is_wide_by_active_channel() {
        let key_1 = ColumnKeyDto {
            route: "/".to_string(),
            stream_id: 1,
            column_index: 0,
        };
        let key_2 = ColumnKeyDto {
            route: "/".to_string(),
            stream_id: 1,
            column_index: 1,
        };

        let mut ch_1 = ColumnState::new("ch1".to_string(), "V".to_string(), 100.0);
        ch_1.push_raw(Point { x: 0.0, y: 1.0 }, 10.0);
        ch_1.push_raw(Point { x: 1.0, y: 2.0 }, 10.0);

        let mut ch_2 = ColumnState::new("ch2".to_string(), "V".to_string(), 100.0);
        ch_2.push_raw(Point { x: 0.0, y: 10.0 }, 10.0);
        ch_2.push_raw(Point { x: 0.5, y: 11.0 }, 10.0);
        ch_2.push_raw(Point { x: 1.0, y: 12.0 }, 10.0);

        let mut column_states = HashMap::new();
        column_states.insert(key_1.clone(), ch_1);
        column_states.insert(key_2.clone(), ch_2);

        let mut active_columns = HashSet::new();
        active_columns.insert(key_1);
        active_columns.insert(key_2);

        let text = build_view_data_tsv(
            &column_states,
            &active_columns,
            &ViewConfig::default(),
            Some(1.0),
        );

        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "time\tch1\tch2");
        assert_eq!(
            lines[1],
            "0.00000000000000000\t1.00000000000000000\t10.00000000000000000"
        );
        assert_eq!(lines[2], "0.50000000000000000\t\t11.00000000000000000");
        assert_eq!(
            lines[3],
            "1.00000000000000000\t2.00000000000000000\t12.00000000000000000"
        );
    }

    #[test]
    fn plot_point_source_collects_deque_range() {
        let points: VecDeque<_> = [
            Point { x: 0.0, y: 10.0 },
            Point { x: 1.0, y: 11.0 },
            Point { x: 2.0, y: 12.0 },
            Point { x: 3.0, y: 13.0 },
        ]
        .into();
        let source = PlotPointSource::DequeRange {
            points: &points,
            start: 1,
            end: 3,
        };

        assert_eq!(source.len(), 2);
        assert_eq!(
            source.collect(),
            vec![Point { x: 1.0, y: 11.0 }, Point { x: 2.0, y: 12.0 }]
        );
    }

    fn test_device(route: &str) -> DeviceDto {
        DeviceDto {
            url: "test://device".to_string(),
            route: route.to_string(),
            meta: DeviceMetaDto {
                serial_number: String::new(),
                firmware_hash: String::new(),
                n_streams: 0,
                session_id: 0,
                name: route.to_string(),
            },
            streams: Vec::new(),
            rpcs: Vec::new(),
            full_metadata: None,
        }
    }
}
