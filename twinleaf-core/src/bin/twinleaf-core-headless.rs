// SPDX-License-Identifier: Apache-2.0
//! Headless smoke test for `twinleaf-core` (milestone M0/M1 verification).
//!
//! Usage:
//!   twinleaf-core-headless                  # list available devices, then exit
//!   twinleaf-core-headless <url> [route]    # connect, auto-select a channel,
//!                                           # and verify the streaming pipeline
//!
//! Examples:
//!   twinleaf-core-headless
//!   twinleaf-core-headless udp://localhost:7855      # against `tio simulate`
//!   twinleaf-core-headless serial:///dev/cu.usbserial-XXXX
//!
//! In connect mode it waits for metadata, picks the first plottable column,
//! sets pane 0, and exits after a few `PlotFrame`s — exercising connect →
//! metadata → FPCS decimation → plot frame with no UI.

use twinleaf_core::{
    ColumnKeyDto, CoreCommand, CoreEvent, ExportFormat, PlotMode, PlotPaneConfig, TwinleafCore,
    ViewConfig,
};

fn first_column_key(meta: &serde_json::Value) -> Option<ColumnKeyDto> {
    for device in meta.get("devices")?.as_array()? {
        let Some(streams) = device.get("streams").and_then(|s| s.as_array()) else {
            continue;
        };
        for stream in streams {
            let Some(columns) = stream.get("columns").and_then(|c| c.as_array()) else {
                continue;
            };
            if let Some(key) = columns.first().and_then(|c| c.get("key")) {
                if let Ok(key) = serde_json::from_value::<ColumnKeyDto>(key.clone()) {
                    return Some(key);
                }
            }
        }
    }
    None
}

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next();
    let route = args.next();
    let list_mode = url.is_none() && std::env::var_os("TWINLEAF_OPEN_LOG").is_none();

    let (event_tx, event_rx) = async_channel::unbounded::<CoreEvent>();
    let core = TwinleafCore::spawn(event_tx);

    // Export mode (M6): convert a `.tio` to CSV and exit.
    if let Ok(source_path) = std::env::var("TWINLEAF_EXPORT_SRC") {
        let output_path = std::env::var("TWINLEAF_EXPORT_OUT")
            .unwrap_or_else(|_| "/tmp/twinleaf-export.csv".to_string());
        eprintln!("exporting {source_path} -> {output_path}");
        core.send(CoreCommand::ExportLog {
            request_id: "export".to_string(),
            source_path,
            output_path,
            format: ExportFormat::Csv,
        });
        while let Ok(event) = event_rx.recv_blocking() {
            if let CoreEvent::Json(value) = &event {
                if value.get("type").and_then(|t| t.as_str()) == Some("exportResult") {
                    println!("exportResult: {value}");
                    break;
                }
            }
        }
        return;
    }

    if let Ok(path) = std::env::var("TWINLEAF_OPEN_LOG") {
        // Playback mode (M7): open a recorded log for inspection.
        eprintln!("opening log {path} ...");
        core.send(CoreCommand::OpenLog { path });
    } else if let Some(url) = url {
        eprintln!("connecting to {url} ...");
        core.send(CoreCommand::Connect {
            url,
            route,
            log_path: None,
        });
    } else {
        eprintln!("listing devices ...");
        core.send(CoreCommand::ListDevices {
            include_all: Some(true),
        });
    }

    let mut selected = false;
    let mut plot_frames = 0u32;
    let mut stream_batches = 0u32;
    let mut health_count = 0u32;
    let upgrade_mode = std::env::var_os("TWINLEAF_HEADLESS_UPGRADE").is_some();
    while let Ok(event) = event_rx.recv_blocking() {
        match &event {
            CoreEvent::Status { state, message } => eprintln!("[status] {state}: {message}"),
            CoreEvent::Error(message) => eprintln!("[error] {message}"),
            CoreEvent::Debug(_) => {}
            CoreEvent::Plot(frame) => {
                let points: usize = frame.series.iter().map(|s| s.points.len()).sum();
                plot_frames += 1;
                // Peak point (for FFT, the dominant frequency).
                let peak = frame
                    .series
                    .iter()
                    .flat_map(|s| &s.points)
                    .filter(|p| p.x.is_finite() && p.y.is_finite())
                    .fold(None::<&twinleaf_core::Point>, |best, p| match best {
                        Some(b) if b.y >= p.y => Some(b),
                        _ => Some(p),
                    });
                let peak_str = peak
                    .map(|p| format!(", peak x={:.3} y={:.5}", p.x, p.y))
                    .unwrap_or_default();
                println!(
                    "Plot #{plot_frames}: mode={:?} series={} points={}{peak_str}",
                    frame.mode,
                    frame.series.len(),
                    points
                );
                if plot_frames >= 8 && !upgrade_mode {
                    println!("done after {plot_frames} frames");
                    break;
                }
            }
            CoreEvent::Json(value) => {
                match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "deviceList" => {
                        let devices = value
                            .get("devices")
                            .map(|d| d.to_string())
                            .unwrap_or_default();
                        println!("deviceList: {devices}");
                        if list_mode {
                            break;
                        }
                    }
                    "metadata" => {
                        println!("metadata received");
                        if !selected {
                            selected = true;
                            if upgrade_mode {
                                eprintln!("checking for firmware updates ...");
                                core.send(CoreCommand::CheckUpgrade);
                            } else if let Some(key) = first_column_key(value) {
                                eprintln!("auto-selecting {key:?}, setting pane 0");
                                core.send(CoreCommand::SetActiveColumns {
                                    columns: vec![key.clone()],
                                });
                                let mut view = ViewConfig::default();
                                if std::env::var_os("TWINLEAF_HEADLESS_FFT").is_some() {
                                    view.mode = PlotMode::Fft;
                                }
                                core.send(CoreCommand::SetPlotPanes {
                                    panes: vec![PlotPaneConfig {
                                        id: 0,
                                        view,
                                        columns: vec![key],
                                        viewport_end: None,
                                    }],
                                });
                            } else {
                                eprintln!("no plottable column found in metadata");
                            }
                        }
                    }
                    "streamValues" => {
                        stream_batches += 1;
                        if stream_batches <= 3 {
                            println!("streamValues batch #{stream_batches}");
                        }
                    }
                    "playback" => {
                        println!(
                            "playback: start={} end={} position={}",
                            value.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0),
                            value.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0),
                            value
                                .get("position")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(0.0),
                        );
                    }
                    "health" => {
                        println!("health: {value}");
                        if upgrade_mode {
                            health_count += 1;
                            if health_count >= 4 {
                                break;
                            }
                        }
                    }
                    "upgradeStatus" => {
                        println!("upgradeStatus: {value}");
                    }
                    "upgradeProgress" => {
                        println!("upgradeProgress: {value}");
                    }
                    _ => {}
                }
            }
        }
    }
    eprintln!("done (plot_frames={plot_frames}, streamValue_batches={stream_batches})");
}
