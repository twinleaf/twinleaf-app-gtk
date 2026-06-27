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

use std::collections::HashMap;
use serde::Deserialize;
use serde_json::Value;
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
        !self.name.is_empty() && !self.is_internal() && (self.readable || self.writable || self.is_capture())
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

/// `metadata` payload -> connected devices with streams/columns/rpcs.
/// The default channel to display when a session opens (a fresh connect or an
/// opened file): the first column (lowest column index) of the lowest-id stream
/// that carries data (has columns). Returns `None` if nothing is plottable.
pub fn default_display_column(devices: &[DeviceInfo]) -> Option<ColumnKeyDto> {
    devices
        .iter()
        .flat_map(|device| &device.streams)
        .filter(|stream| !stream.columns.is_empty())
        .min_by_key(|stream| stream.stream_id)
        .and_then(|stream| stream.columns.iter().min_by_key(|column| column.key.column_index))
        .map(|column| column.key.clone())
}

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
    fn default_display_column_picks_lowest_stream_with_data_first_column() {
        // Streams out of order, an empty (data-less) stream, and columns out of
        // index order — the default is stream 2 (lowest with data; 0 is empty),
        // column 0 (lowest index).
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
        let key = default_display_column(&devices).expect("a plottable column");
        assert_eq!(key.stream_id, 2);
        assert_eq!(key.column_index, 0);

        // No plottable columns → None.
        let empty: Vec<DeviceInfo> = serde_json::from_value(serde_json::json!([{
            "route": "/",
            "streams": [{"streamId": 1, "columns": []}]
        }]))
        .unwrap();
        assert!(default_display_column(&empty).is_none());
    }
}
