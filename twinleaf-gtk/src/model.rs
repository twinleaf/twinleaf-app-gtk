// SPDX-License-Identifier: Apache-2.0
//! UI-side mirrors of the core's structured (`CoreEvent::Json`) event payloads.
//!
//! The core currently carries the non-hot-path events as tagged JSON; these
//! `Deserialize` structs decode them into typed data for the widgets. The
//! field names match the core DTOs' camelCase serialization. Column identity
//! reuses [`twinleaf_core::ColumnKeyDto`] so it composes with the typed
//! `CoreCommand`s and `PlotFrame`s.
//!
//! Some fields are populated by deserialization but not yet read by the UI
//! (they back later milestones: device picker, RPC sidebar, inspector).
#![allow(dead_code)]

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use twinleaf_core::ColumnKeyDto;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableDevice {
    pub url: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub routes: Vec<AvailableRoute>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableRoute {
    pub route: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub route: String,
    #[serde(default)]
    pub meta: DeviceMeta,
    #[serde(default)]
    pub streams: Vec<StreamInfo>,
    #[serde(default)]
    pub rpcs: Vec<RpcInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceMeta {
    #[serde(default)]
    pub serial_number: String,
    #[serde(default)]
    pub firmware_hash: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    pub stream_id: u8,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub effective_sampling_rate: f64,
    #[serde(default)]
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub key: ColumnKeyDto,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub units: String,
    #[serde(default)]
    pub data_type: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub display_value: Option<f64>,
}

/// RPC descriptor. The classification helpers mirror the SwiftUI app's
/// `RpcInfo` computed properties and drive widget selection in the RPC sidebar.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcInfo {
    #[serde(default)]
    pub route: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub arg_type: String,
    #[serde(default)]
    pub readable: bool,
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub persistent: bool,
    #[serde(default)]
    pub unknown: bool,
    #[serde(default)]
    pub value: Option<Value>,
}

impl RpcInfo {
    /// Argument type with any `<range>` suffix stripped (e.g. `u16<0..100>` -> `u16`).
    pub fn base_arg_type(&self) -> &str {
        self.arg_type.split('<').next().unwrap_or(&self.arg_type)
    }
    pub fn has_metadata(&self) -> bool {
        !self.unknown && !self.arg_type.is_empty() && self.arg_type != "missing"
    }
    pub fn is_internal(&self) -> bool {
        self.name.starts_with("rpc.")
    }
    pub fn is_capture(&self) -> bool {
        self.base_arg_type() == "capture" || self.name.ends_with(".capture")
    }
    /// Shown in the RPC list at all?
    pub fn is_visible(&self) -> bool {
        !self.name.is_empty()
            && !self.is_internal()
            && (self.readable || self.writable || self.is_capture())
    }
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.base_arg_type(),
            "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64" | "f32" | "f64"
        )
    }
    pub fn is_bool(&self) -> bool {
        self.has_metadata() && self.base_arg_type() == "bool"
    }
    pub fn is_action(&self) -> bool {
        self.writable && self.has_metadata() && self.base_arg_type() == "unit"
    }
    pub fn is_string(&self) -> bool {
        self.base_arg_type() == "string"
    }
}

/// All visible RPCs across every connected device, in stable order.
pub fn visible_rpcs(devices: &[DeviceInfo]) -> Vec<RpcInfo> {
    devices
        .iter()
        .flat_map(|d| d.rpcs.iter())
        .filter(|r| r.is_visible())
        .cloned()
        .collect()
}

/// Readable RPCs whose current value should be primed with a read on connect.
/// Mirrors the SwiftUI app's `loadReadableRPCs` filter — the core sends RPC
/// descriptors without values, so the UI must read them to populate controls.
pub fn readable_rpcs(devices: &[DeviceInfo]) -> Vec<RpcInfo> {
    devices
        .iter()
        .flat_map(|d| d.rpcs.iter())
        .filter(|r| r.readable && r.has_metadata() && !r.is_capture() && !r.is_action())
        .cloned()
        .collect()
}

/// Render an RPC value for display.
pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamValue {
    pub key: ColumnKeyDto,
    pub value: f64,
}

fn array_field<T: serde::de::DeserializeOwned>(value: &Value, field: &str) -> Vec<T> {
    value
        .get(field)
        .cloned()
        .and_then(|v| serde_json::from_value::<Vec<T>>(v).ok())
        .unwrap_or_default()
}

/// `deviceList` payload -> discovered devices.
pub fn device_list(value: &Value) -> Vec<AvailableDevice> {
    array_field(value, "devices")
}

/// The channels to display when a session opens (a fresh connect or an opened
/// file): every column of the lowest-id data-bearing stream of the first
/// device — skipping comm/hub interface boards when `skip_comm_hub` is set, so
/// e.g. a VMR behind a COMM hub is plotted instead of the hub's own IMU
/// (mirroring the SwiftUI app). Empty if nothing qualifies, including when
/// every device is a comm/hub.
pub fn default_display_columns(devices: &[DeviceInfo], skip_comm_hub: bool) -> Vec<ColumnKeyDto> {
    devices
        .iter()
        .filter(|device| {
            !skip_comm_hub
                || !(is_comm_or_hub_board_name(&device.meta.name)
                    || is_comm_or_hub_board_name(&device.route))
        })
        .find_map(|device| {
            let stream = device
                .streams
                .iter()
                .filter(|stream| !stream.columns.is_empty())
                .min_by_key(|stream| stream.stream_id)?;
            let mut columns: Vec<ColumnKeyDto> =
                stream.columns.iter().map(|c| c.key.clone()).collect();
            columns.sort_by_key(|key| key.column_index);
            Some(columns)
        })
        .unwrap_or_default()
}

/// True for comm/hub interface boards (e.g. "COMM-USB"), which relay data for
/// the sensors behind them; their own streams (IMU etc.) aren't what the user
/// connected for.
fn is_comm_or_hub_board_name(raw: &str) -> bool {
    let name = raw.trim().trim_matches('/').to_ascii_lowercase();
    name.starts_with("comm") || name.starts_with("hub")
}

/// `metadata` payload -> connected devices with streams/columns/rpcs.
pub fn metadata_devices(value: &Value) -> Vec<DeviceInfo> {
    array_field(value, "devices")
}

/// `streamValues` payload -> latest scalar per column.
pub fn stream_values(value: &Value) -> Vec<StreamValue> {
    array_field(value, "values")
}

/// First plottable column across all devices/streams (used by the self-test).
pub fn first_column_key(devices: &[DeviceInfo]) -> Option<ColumnKeyDto> {
    devices
        .iter()
        .flat_map(|d| &d.streams)
        .flat_map(|s| &s.columns)
        .map(|c| c.key.clone())
        .next()
}

/// Map every column key to its `(name, units)` for plot legends/labels.
pub fn column_labels(devices: &[DeviceInfo]) -> HashMap<ColumnKeyDto, (String, String)> {
    let mut labels = HashMap::new();
    for device in devices {
        for stream in &device.streams {
            for column in &stream.columns {
                labels.insert(
                    column.key.clone(),
                    (column.name.clone(), column.units.clone()),
                );
            }
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_display_columns_picks_lowest_data_stream_whole() {
        // Streams out of order, an empty (data-less) stream, and columns out of
        // index order — the default is every column of stream 2 (lowest with
        // data; 0 is empty), in column-index order.
        let devices: Vec<DeviceInfo> = serde_json::from_value(serde_json::json!([{
            "route": "/",
            "streams": [
                {"streamId": 5, "columns": [
                    {"key": {"route": "/", "streamId": 5, "columnIndex": 0}}
                ]},
                {"streamId": 0, "columns": []},
                {"streamId": 2, "columns": [
                    {"key": {"route": "/", "streamId": 2, "columnIndex": 1}},
                    {"key": {"route": "/", "streamId": 2, "columnIndex": 0}}
                ]}
            ]
        }]))
        .unwrap();
        let keys = default_display_columns(&devices, true);
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| k.stream_id == 2));
        assert_eq!(keys[0].column_index, 0);
        assert_eq!(keys[1].column_index, 1);

        // No plottable columns → empty.
        let empty: Vec<DeviceInfo> = serde_json::from_value(serde_json::json!([{
            "route": "/",
            "streams": [{"streamId": 1, "columns": []}]
        }]))
        .unwrap();
        assert!(default_display_columns(&empty, true).is_empty());
    }

    #[test]
    fn default_display_columns_skips_comm_hub_boards() {
        // A COMM hub (with its own IMU stream) listed before the sensor behind
        // it: the sensor's stream wins, even though the hub comes first.
        let devices: Vec<DeviceInfo> = serde_json::from_value(serde_json::json!([
            {
                "route": "/",
                "meta": {"name": "COMM-USB"},
                "streams": [{"streamId": 1, "columns": [
                    {"key": {"route": "/", "streamId": 1, "columnIndex": 0}}
                ]}]
            },
            {
                "route": "/0",
                "meta": {"name": "VMR"},
                "streams": [{"streamId": 1, "columns": [
                    {"key": {"route": "/0", "streamId": 1, "columnIndex": 0}}
                ]}]
            }
        ]))
        .unwrap();
        let keys = default_display_columns(&devices, true);
        assert!(keys.iter().all(|k| k.route == "/0"));
        assert!(!keys.is_empty());

        // Preference off → the hub's own stream is eligible again.
        assert_eq!(default_display_columns(&devices, false)[0].route, "/");

        // Every device a comm/hub → nothing is auto-selected.
        let hubs_only = &devices[..1].to_vec();
        assert!(default_display_columns(hubs_only, true).is_empty());
    }
}
