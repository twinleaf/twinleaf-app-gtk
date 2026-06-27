// SPDX-License-Identifier: Apache-2.0
//! Left sidebar: one section per device (or per unified group), each listing
//! the device's channels (plot toggles + live values) and its RPC settings
//! controls. The "Unify matching sensors" toggle groups devices that share a
//! model name so a single channel/RPC row drives every matching device at once
//! — mirroring the SwiftUI app.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use serde_json::Value;
use twinleaf_core::ColumnKeyDto;

use crate::app::AppState;
use crate::controls::MainControls;
use crate::model::{self, ColumnInfo, DeviceInfo, RpcInfo, StreamValue};

enum RpcControl {
    /// Switch + a flag that suppresses the write when we set it programmatically.
    Toggle(gtk::Switch, Rc<Cell<bool>>),
    /// Entry + the last-applied text (so focus-out only writes on real changes) +
    /// a focus flag (a GtkEntry's focus lives on its inner GtkText, so
    /// `has_focus()` is unreliable; we track enter/leave ourselves).
    Field(gtk::Entry, Rc<RefCell<String>>, Rc<Cell<bool>>),
    Display(gtk::Label),
    None,
}

/// One RPC row, possibly spanning several devices (unified). `members` are the
/// (route, name) pairs the control reads from and writes to.
struct RpcRow {
    members: Vec<(String, String)>,
    control: RpcControl,
}

pub struct Sidebar {
    pub widget: gtk::Box,
    state: Rc<AppState>,
    /// Main-view tray that hosts promoted sliders and capture graphs.
    controls: Rc<MainControls>,
    unify_btn: gtk::ToggleButton,
    content: gtk::Box,
    devices: RefCell<Vec<DeviceInfo>>,
    /// Structure fingerprint of the last metadata that was rendered, so an
    /// unchanged re-emit (stream/device reset) doesn't rebuild and steal focus.
    signature: RefCell<String>,
    /// Each channel key → the value label of its row (a unified row maps several
    /// keys to one label).
    value_labels: RefCell<HashMap<ColumnKeyDto, gtk::Label>>,
    rpc_rows: RefCell<Vec<RpcRow>>,
    /// Unified RPC groups (≥2 sensors): reveal per-device rows when the sensors'
    /// values disagree.
    unified_groups: RefCell<Vec<UnifiedGroup>>,
}

impl Sidebar {
    pub fn new(state: &Rc<AppState>, controls: &Rc<MainControls>) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.set_margin_top(6);
        widget.set_margin_bottom(6);
        widget.set_margin_start(6);
        widget.set_margin_end(6);

        // A header toolbar toggle (the window packs it next to the sidebar
        // button). Hidden until at least two connected devices share a model
        // name.
        // An icon button (not a text label) so it's the same size as the other
        // toolbar buttons; the bundled glyph is a U / ∪ (union).
        let unify_btn = gtk::ToggleButton::new();
        unify_btn.set_icon_name("tl-unify-symbolic");
        unify_btn.set_tooltip_text(Some("Unify matching sensors"));
        unify_btn.set_visible(false);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&content));
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        widget.append(&scroll);

        let this = Rc::new(Self {
            widget,
            state: state.clone(),
            controls: controls.clone(),
            unify_btn: unify_btn.clone(),
            content,
            devices: RefCell::new(Vec::new()),
            signature: RefCell::new(String::new()),
            value_labels: RefCell::new(HashMap::new()),
            rpc_rows: RefCell::new(Vec::new()),
            unified_groups: RefCell::new(Vec::new()),
        });

        {
            let this = this.clone();
            unify_btn.connect_toggled(move |_| {
                this.rebuild();
                // Re-read so the regrouped RPC controls repopulate.
                this.reread_rpcs();
            });
        }
        this
    }

    /// The "unify matching sensors" toggle, for the window to pack in the header.
    pub fn unify_button(&self) -> &gtk::ToggleButton {
        &self.unify_btn
    }

    /// Replace the connected-device metadata and rebuild the sidebar.
    pub fn set_metadata(&self, devices: &[DeviceInfo]) {
        *self.devices.borrow_mut() = devices.to_vec();
        self.unify_btn.set_visible(has_unifiable(devices));
        if std::env::var_os("TWINLEAF_SELFTEST_UNIFY").is_some()
            && self.unify_btn.is_visible()
            && !self.unify_btn.is_active()
        {
            self.unify_btn.set_active(true); // fires a rebuild via the toggle handler
        }
        // Skip the rebuild when the device/stream/RPC structure is unchanged
        // (a stream/device reset re-emits identical metadata) so an in-progress
        // edit keeps keyboard focus. Values still refresh via streamValues and
        // rpcResult on the existing widgets.
        let signature = metadata_signature(devices);
        if *self.signature.borrow() == signature {
            return;
        }
        *self.signature.borrow_mut() = signature;
        self.rebuild();
    }

    /// Update the live value readouts from a `streamValues` batch.
    pub fn update_values(&self, values: &[StreamValue]) {
        let labels = self.value_labels.borrow();
        for v in values {
            if let Some(label) = labels.get(&v.key) {
                label.set_text(&fmt_value(v.value));
            }
        }
    }

    /// Apply an `rpcResult` value to every row that owns this (route, name).
    pub fn update_rpc_value(&self, route: &str, name: &str, value: &Value) {
        for row in self.rpc_rows.borrow().iter() {
            if row.members.iter().any(|(r, n)| r == route && n == name) {
                apply_rpc_value(&row.control, value);
            }
        }
        for group in self.unified_groups.borrow().iter() {
            if group.members.iter().any(|(r, n)| r == route && n == name) {
                group.note(route, value);
            }
        }
    }

    fn reread_rpcs(&self) {
        for rpc in model::readable_rpcs(&self.devices.borrow()) {
            self.state.call_rpc(rpc.route, rpc.name, None);
        }
    }

    fn rebuild(&self) {
        clear_box(&self.content);
        self.value_labels.borrow_mut().clear();
        self.rpc_rows.borrow_mut().clear();
        self.unified_groups.borrow_mut().clear();

        let devices = self.devices.borrow();
        let unify = self.unify_btn.is_visible() && self.unify_btn.is_active();
        for group in build_groups(&devices, unify) {
            let header = section_label(&group.label);
            header.set_margin_top(8);
            self.content.append(&header);
            if !group.serial.is_empty() {
                let serial = gtk::Label::new(Some(&group.serial));
                serial.set_xalign(0.0);
                serial.add_css_class("dim-label");
                self.content.append(&serial);
            }

            for stream in &group.streams {
                let sh = gtk::Label::new(Some(&stream.title));
                sh.set_xalign(0.0);
                sh.set_margin_start(8);
                sh.add_css_class("dim-label");
                self.content.append(&sh);

                for chan in &stream.channels {
                    self.append_channel_row(chan);
                }
            }

            if !group.rpcs.is_empty() {
                let sh = gtk::Label::new(Some("Settings"));
                sh.set_xalign(0.0);
                sh.set_margin_start(8);
                sh.set_margin_top(4);
                sh.add_css_class("dim-label");
                self.content.append(&sh);

                for rpc in &group.rpcs {
                    // A settable RPC shared by ≥2 sensors gets a combined
                    // (write-all) control plus per-device rows that appear only
                    // when the sensors disagree. Actions/captures just write all.
                    let unified_multi =
                        rpc.members.len() >= 2 && !rpc.rep.is_action() && !rpc.rep.is_capture();
                    if unified_multi {
                        let (row_widget, combined) =
                            build_rpc_row(&rpc.rep, &rpc.members, &self.state, &self.controls, None);
                        self.content.append(&row_widget);

                        let container = gtk::Box::new(gtk::Orientation::Vertical, 2);
                        container.set_margin_start(16);
                        container.set_visible(false);
                        for (i, member) in rpc.members.iter().enumerate() {
                            let label = rpc.member_labels.get(i).cloned().unwrap_or_default();
                            let (prow, pcontrol) = build_rpc_row(
                                &rpc.rep,
                                std::slice::from_ref(member),
                                &self.state,
                                &self.controls,
                                Some(&label),
                            );
                            container.append(&prow);
                            self.rpc_rows.borrow_mut().push(RpcRow {
                                members: vec![member.clone()],
                                control: pcontrol,
                            });
                        }
                        self.content.append(&container);
                        self.unified_groups.borrow_mut().push(UnifiedGroup {
                            members: rpc.members.clone(),
                            combined,
                            container,
                            values: RefCell::new(HashMap::new()),
                        });
                    } else {
                        let (row_widget, control) =
                            build_rpc_row(&rpc.rep, &rpc.members, &self.state, &self.controls, None);
                        self.content.append(&row_widget);
                        self.rpc_rows.borrow_mut().push(RpcRow {
                            members: rpc.members.clone(),
                            control,
                        });
                    }
                }
            }
        }
    }

    fn append_channel_row(&self, chan: &Channel) {
        if chan.keys.len() <= 1 {
            // Single device: checkbox + inline value.
            let plotted = chan.keys.first().map_or(false, |k| {
                let data = self.state.data.borrow();
                data.panes.iter().any(|p| p.columns.contains(k))
            });
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let check = gtk::CheckButton::with_label(&chan.label);
            check.set_hexpand(true);
            check.set_active(plotted); // before connect so it doesn't fire
            {
                let keys = chan.keys.clone();
                let state = self.state.clone();
                check.connect_toggled(move |c| state.toggle_columns(keys.clone(), c.is_active()));
            }
            if let Some(first) = chan.keys.first() {
                add_channel_drag(&row, first);
            }
            let value = gtk::Label::new(None);
            value.set_width_chars(11);
            value.set_xalign(1.0);
            value.add_css_class("numeric");
            row.append(&check);
            row.append(&value);
            self.content.append(&row);
            if let Some(first) = chan.keys.first() {
                self.value_labels.borrow_mut().insert(first.clone(), value);
            }
        } else {
            // Unified: a column header, then a checkbox + value per device, so
            // each sensor's column can be plotted independently.
            let header = gtk::Label::new(Some(&chan.label));
            header.set_xalign(0.0);
            header.set_margin_start(4);
            self.content.append(&header);
            for key in &chan.keys {
                let plotted = {
                    let data = self.state.data.borrow();
                    data.panes.iter().any(|p| p.columns.contains(key))
                };
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                row.set_margin_start(20);
                add_channel_drag(&row, key);
                let check = gtk::CheckButton::with_label(key.route.as_str());
                check.set_hexpand(true);
                check.set_active(plotted);
                {
                    let key = key.clone();
                    let state = self.state.clone();
                    check.connect_toggled(move |c| {
                        state.toggle_columns(vec![key.clone()], c.is_active())
                    });
                }
                let value = gtk::Label::new(None);
                value.set_width_chars(11);
                value.set_xalign(1.0);
                value.add_css_class("numeric");
                row.append(&check);
                row.append(&value);
                self.content.append(&row);
                self.value_labels.borrow_mut().insert(key.clone(), value);
            }
        }
    }
}

// ---- Grouping ---------------------------------------------------------------

struct Channel {
    label: String,
    keys: Vec<ColumnKeyDto>,
}

struct StreamGroup {
    title: String,
    channels: Vec<Channel>,
}

struct RpcGroup {
    rep: RpcInfo,
    members: Vec<(String, String)>,
    /// A device label per member (parallel to `members`), for per-device rows.
    member_labels: Vec<String>,
}

struct Group {
    label: String,
    serial: String,
    streams: Vec<StreamGroup>,
    rpcs: Vec<RpcGroup>,
}

/// True when at least two connected devices share a (non-empty) model name.
fn has_unifiable(devices: &[DeviceInfo]) -> bool {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for d in devices {
        if !d.meta.name.is_empty() {
            *counts.entry(d.meta.name.as_str()).or_default() += 1;
        }
    }
    counts.values().any(|&c| c >= 2)
}

fn build_groups(devices: &[DeviceInfo], unify: bool) -> Vec<Group> {
    if unify {
        let mut order: Vec<String> = Vec::new();
        let mut by_name: HashMap<String, Vec<&DeviceInfo>> = HashMap::new();
        for d in devices {
            let name = d.meta.name.clone();
            if !by_name.contains_key(&name) {
                order.push(name.clone());
            }
            by_name.entry(name).or_default().push(d);
        }
        order
            .into_iter()
            .filter_map(|name| by_name.remove(&name))
            .map(|members| build_group(&members, true))
            .collect()
    } else {
        devices.iter().map(|d| build_group(&[d], false)).collect()
    }
}

fn build_group(members: &[&DeviceInfo], unified: bool) -> Group {
    let first = members[0];
    let label = if first.meta.name.is_empty() {
        first.route.clone()
    } else {
        first.meta.name.clone()
    };
    let serial = if unified && members.len() > 1 {
        members
            .iter()
            .map(|d| {
                if d.meta.serial_number.is_empty() {
                    d.route.clone()
                } else {
                    format!("{} ({})", d.meta.serial_number, d.route)
                }
            })
            .collect::<Vec<_>>()
            .join("   ")
    } else {
        first.meta.serial_number.clone()
    };

    // Streams/channels: use the first device as the template, gathering each
    // member's matching column key (same stream id + column index).
    let mut streams = Vec::new();
    for stream in &first.streams {
        let title = format!("{}   ·   {:.1} Hz", stream.name, stream.effective_sampling_rate);
        let mut channels = Vec::new();
        for col in &stream.columns {
            let mut keys: Vec<ColumnKeyDto> = members
                .iter()
                .filter_map(|d| member_column_key(d, &col.key))
                .collect();
            if keys.is_empty() {
                keys.push(col.key.clone());
            }
            channels.push(Channel {
                label: channel_label(col),
                keys,
            });
        }
        streams.push(StreamGroup { title, channels });
    }

    // RPCs: union by name across members (visible only).
    let mut rpcs = Vec::new();
    for rpc in first.rpcs.iter().filter(|r| r.is_visible()) {
        let mut group_members: Vec<(String, String)> = Vec::new();
        let mut member_labels: Vec<String> = Vec::new();
        for d in members {
            if let Some(r) = d.rpcs.iter().find(|r| r.name == rpc.name && r.is_visible()) {
                group_members.push((r.route.clone(), r.name.clone()));
                member_labels.push(device_short_label(d));
            }
        }
        if group_members.is_empty() {
            group_members.push((rpc.route.clone(), rpc.name.clone()));
            member_labels.push(device_short_label(first));
        }
        rpcs.push(RpcGroup {
            rep: rpc.clone(),
            members: group_members,
            member_labels,
        });
    }

    Group {
        label,
        serial,
        streams,
        rpcs,
    }
}

/// Find device `d`'s column key matching `target`'s stream id + column index
/// (the route differs between unified members).
fn member_column_key(d: &DeviceInfo, target: &ColumnKeyDto) -> Option<ColumnKeyDto> {
    d.streams
        .iter()
        .flat_map(|s| &s.columns)
        .find(|c| {
            c.key.stream_id == target.stream_id && c.key.column_index == target.column_index
        })
        .map(|c| c.key.clone())
}

/// A short per-device label for unified per-device rows: the serial (with route)
/// or the route alone.
fn device_short_label(d: &DeviceInfo) -> String {
    if d.meta.serial_number.is_empty() {
        d.route.clone()
    } else {
        format!("{} ({})", d.meta.serial_number, d.route)
    }
}

/// Add a drag source carrying a column key (drop a channel onto a plot pane).
fn add_channel_drag(row: &gtk::Box, key: &ColumnKeyDto) {
    let drag = gtk::DragSource::new();
    drag.set_actions(gtk::gdk::DragAction::COPY);
    let key_json = serde_json::to_string(key).unwrap_or_default();
    drag.connect_prepare(move |_, _, _| {
        Some(gtk::gdk::ContentProvider::for_value(&key_json.to_value()))
    });
    row.add_controller(drag);
}

// ---- RPC control rows -------------------------------------------------------

fn build_rpc_row(
    rpc: &RpcInfo,
    members: &[(String, String)],
    state: &Rc<AppState>,
    controls: &Rc<MainControls>,
    label_override: Option<&str>,
) -> (gtk::Box, RpcControl) {
    let display = label_override.unwrap_or(&rpc.name).to_string();
    let name = gtk::Label::new(Some(&display));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.set_tooltip_text(Some(&format!("{} {}", rpc.permissions, rpc.arg_type)));

    // Writes go to every member device (one control drives all in unified mode).
    let write_all: Rc<dyn Fn(Option<Value>)> = {
        let state = state.clone();
        let members = members.to_vec();
        Rc::new(move |arg: Option<Value>| {
            for (r, n) in &members {
                state.call_rpc(r.clone(), n.clone(), arg.clone());
            }
        })
    };

    // Capture: a button that opens (or re-triggers) a capture graph in the main
    // view. The graph and its readout live there, not in the sidebar.
    if rpc.is_capture() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.set_margin_start(8);
        row.append(&name);
        let btn = gtk::Button::with_label("Capture");
        btn.set_valign(gtk::Align::Center);
        {
            let controls = controls.clone();
            let rpc = rpc.clone();
            let members = members.to_vec();
            let display = display.clone();
            btn.connect_clicked(move |_| controls.add_capture(&rpc, &members, &display));
        }
        row.append(&btn);
        return (row, RpcControl::None);
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_margin_top(2);
    row.set_margin_bottom(2);
    row.set_margin_start(8);
    row.append(&name);

    let control = if rpc.is_action() {
        let btn = gtk::Button::with_label("Run");
        btn.set_valign(gtk::Align::Center);
        let write_all = write_all.clone();
        btn.connect_clicked(move |_| write_all(None));
        row.append(&btn);
        RpcControl::None
    } else if rpc.is_bool() {
        let sw = gtk::Switch::new();
        sw.set_active(rpc.value.as_ref().and_then(Value::as_bool).unwrap_or(false));
        sw.set_sensitive(rpc.writable);
        sw.set_valign(gtk::Align::Center);
        let suppress = Rc::new(Cell::new(false));
        if rpc.writable {
            let write_all = write_all.clone();
            let suppress2 = suppress.clone();
            sw.connect_state_set(move |_, active| {
                if !suppress2.get() {
                    write_all(Some(Value::Bool(active)));
                }
                glib::Propagation::Proceed
            });
        }
        row.append(&sw);
        RpcControl::Toggle(sw, suppress)
    } else if rpc.is_numeric() || rpc.is_string() {
        let entry = gtk::Entry::new();
        entry.set_width_chars(10);
        let initial = rpc.value.as_ref().map(model::value_to_string).unwrap_or_default();
        entry.set_text(&initial);
        entry.set_editable(rpc.writable);
        let baseline = Rc::new(RefCell::new(initial));
        let focused = Rc::new(Cell::new(false));
        if rpc.writable {
            entry.set_tooltip_text(Some("Press Enter or tab away to apply"));
            let commit: Rc<dyn Fn()> = Rc::new({
                let entry = entry.clone();
                let rpc = rpc.clone();
                let baseline = baseline.clone();
                let write_all = write_all.clone();
                move || {
                    let current = entry.text().to_string();
                    if current == *baseline.borrow() {
                        return;
                    }
                    if let Some(arg) = arg_from_text(&rpc, &current) {
                        write_all(Some(arg));
                        *baseline.borrow_mut() = current;
                    }
                }
            });
            let on_activate = commit.clone();
            entry.connect_activate(move |_| on_activate());
            let focus = gtk::EventControllerFocus::new();
            {
                let focused = focused.clone();
                focus.connect_enter(move |_| focused.set(true));
            }
            {
                let focused = focused.clone();
                let on_leave = commit.clone();
                focus.connect_leave(move |_| {
                    focused.set(false);
                    on_leave();
                });
            }
            entry.add_controller(focus);

            // Up/Down step the digit at the caret by its place value and commit
            // immediately (numeric fields only); without this, the arrows just
            // move focus between fields.
            if rpc.is_numeric() {
                let key = gtk::EventControllerKey::new();
                let entry2 = entry.clone();
                let rpc2 = rpc.clone();
                let baseline2 = baseline.clone();
                let write_all2 = write_all.clone();
                key.connect_key_pressed(move |_, keyval, _, _| {
                    let dir = if keyval == gtk::gdk::Key::Up || keyval == gtk::gdk::Key::KP_Up {
                        1
                    } else if keyval == gtk::gdk::Key::Down || keyval == gtk::gdk::Key::KP_Down {
                        -1
                    } else {
                        return glib::Propagation::Proceed;
                    };
                    let text = entry2.text().to_string();
                    let caret = entry2.position().max(0) as usize;
                    if let Some((new_text, new_caret)) = step_digit(&text, caret, dir) {
                        entry2.set_text(&new_text);
                        entry2.set_position(new_caret as i32);
                        if let Some(arg) = arg_from_text(&rpc2, &new_text) {
                            write_all2(Some(arg));
                            *baseline2.borrow_mut() = new_text;
                        }
                    }
                    glib::Propagation::Stop
                });
                entry.add_controller(key);
            }
        }
        row.append(&entry);
        RpcControl::Field(entry, baseline, focused)
    } else {
        let label = gtk::Label::new(Some(
            &rpc.value.as_ref().map(model::value_to_string).unwrap_or_else(|| "—".into()),
        ));
        label.set_xalign(1.0);
        row.append(&label);
        RpcControl::Display(label)
    };

    // A slider-suitable numeric setting can be promoted to a main-view slider
    // via a right-click menu on its name.
    if slider_suitable(rpc) {
        attach_add_slider_menu(&name, controls, rpc, members, &display);
    }

    (row, control)
}

/// Attach a right-click "Add slider" menu to a setting's name label; choosing it
/// promotes the setting to a slider in the main view.
fn attach_add_slider_menu(
    name: &gtk::Label,
    controls: &Rc<MainControls>,
    rpc: &RpcInfo,
    members: &[(String, String)],
    display: &str,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    name.set_tooltip_text(Some(&format!(
        "{} {}  ·  right-click to add a slider",
        rpc.permissions, rpc.arg_type
    )));
    let name_for_menu = name.clone();
    let controls = controls.clone();
    let rpc = rpc.clone();
    let members = members.to_vec();
    let display = display.to_string();
    gesture.connect_pressed(move |_, _, x, y| {
        let popover = gtk::Popover::new();
        popover.set_parent(&name_for_menu);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let add = gtk::Button::with_label("Add slider");
        add.add_css_class("flat");
        menu.append(&add);
        popover.set_child(Some(&menu));
        {
            let controls = controls.clone();
            let rpc = rpc.clone();
            let members = members.clone();
            let display = display.clone();
            let popover = popover.clone();
            add.connect_clicked(move |_| {
                controls.add_slider(&rpc, &members, &display);
                popover.popdown();
            });
        }
        // Unparent when dismissed so no parented popover lingers on the label.
        let popover_close = popover.clone();
        popover.connect_closed(move |_| popover_close.unparent());
        popover.popup();
    });
    name.add_controller(gesture);
}

/// A ranged numeric RPC gets a slider: readable + writable numeric metadata that
/// isn't a bool switch or a ".max" companion (mirrors the SwiftUI app).
fn slider_suitable(rpc: &RpcInfo) -> bool {
    rpc.is_numeric()
        && rpc.readable
        && rpc.writable
        && rpc.has_metadata()
        && !rpc.is_bool()
        && !rpc.name.ends_with(".max")
}

fn apply_rpc_value(control: &RpcControl, value: &Value) {
    match control {
        RpcControl::Toggle(sw, suppress) => {
            if let Some(b) = value.as_bool() {
                suppress.set(true);
                sw.set_active(b);
                suppress.set(false);
            }
        }
        RpcControl::Field(entry, baseline, focused) => {
            // Skip the reload while the user is in the field, so the text and
            // caret stay put (this is what kept the caret jumping to the front
            // after an Up/Down step committed and the value echoed back).
            if focused.get() {
                return;
            }
            let text = model::value_to_string(value);
            entry.set_text(&text);
            *baseline.borrow_mut() = text;
        }
        RpcControl::Display(label) => label.set_text(&model::value_to_string(value)),
        RpcControl::None => {}
    }
}

/// Put a combined unified control into a "mixed" state (sensors disagree).
fn set_mixed(control: &RpcControl) {
    match control {
        RpcControl::Field(entry, baseline, focused) => {
            if focused.get() {
                return;
            }
            entry.set_text("");
            *baseline.borrow_mut() = String::new();
            entry.set_placeholder_text(Some("mixed"));
        }
        RpcControl::Display(label) => label.set_text("mixed"),
        RpcControl::Toggle(_, _) | RpcControl::None => {}
    }
}

/// A unified RPC across several sensors: a combined (write-all) control plus a
/// container of per-device rows revealed only when the sensors' values disagree.
struct UnifiedGroup {
    members: Vec<(String, String)>,
    combined: RpcControl,
    container: gtk::Box,
    values: RefCell<HashMap<String, Value>>,
}

impl UnifiedGroup {
    fn note(&self, route: &str, value: &Value) {
        self.values.borrow_mut().insert(route.to_string(), value.clone());
        let vals = self.values.borrow();
        let reported: Vec<&Value> = self
            .members
            .iter()
            .filter_map(|(r, _)| vals.get(r))
            .collect();
        let differ = reported.windows(2).any(|w| w[0] != w[1]);
        if std::env::var_os("TWINLEAF_SELFTEST_UNIFY").is_some() {
            let name = self.members.first().map(|(_, n)| n.as_str()).unwrap_or("");
            eprintln!(
                "[selftest] unified {name}: {}/{} reported, differ={differ}",
                reported.len(),
                self.members.len()
            );
        }
        if differ {
            set_mixed(&self.combined);
            self.container.set_visible(true);
        } else if let Some(v) = reported.first() {
            apply_rpc_value(&self.combined, v);
            self.container.set_visible(false);
        }
    }
}

/// Build an RPC argument `Value` from entry text, typed by the RPC's arg type.
pub(crate) fn arg_from_text(rpc: &RpcInfo, text: &str) -> Option<Value> {
    let t = text.trim();
    if rpc.is_string() {
        return Some(Value::String(t.to_string()));
    }
    match rpc.base_arg_type() {
        "f32" | "f64" => t
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        "u8" | "u16" | "u32" | "u64" => t.parse::<u64>().ok().map(|v| Value::Number(v.into())),
        "i8" | "i16" | "i32" | "i64" => t.parse::<i64>().ok().map(|v| Value::Number(v.into())),
        _ => None,
    }
}

/// Increment/decrement the digit at the caret by its place value (e.g. the
/// caret on the tens digit steps by ±10, on the hundredths by ±0.01), keeping
/// the displayed precision. Returns the new text + caret (char index), or None
/// if the field isn't a plain decimal. Mirrors the SwiftUI app's digit stepping.
pub(crate) fn step_digit(text: &str, caret: usize, dir: i64) -> Option<(String, usize)> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains(['e', 'E']) {
        return None;
    }
    let value: f64 = trimmed.parse().ok()?;
    let chars: Vec<char> = text.chars().collect();
    let caret = caret.min(chars.len());
    // Step the digit just left of the caret (or at it, or the last digit).
    let di = if caret > 0 && chars[caret - 1].is_ascii_digit() {
        caret - 1
    } else if caret < chars.len() && chars[caret].is_ascii_digit() {
        caret
    } else {
        chars.iter().rposition(|c| c.is_ascii_digit())?
    };
    let dot = chars.iter().position(|&c| c == '.').unwrap_or(chars.len());
    let power: i32 = if di < dot {
        (dot - 1 - di) as i32
    } else {
        -((di - dot) as i32)
    };
    let frac_digits = if dot < chars.len() { chars.len() - dot - 1 } else { 0 };
    let stepped = value + 10f64.powi(power) * dir as f64;
    let stepped = if stepped == 0.0 { 0.0 } else { stepped };
    let new_text = format!("{stepped:.frac_digits$}");
    // Keep the caret on the same place-value digit even if the length changed.
    let new_chars: Vec<char> = new_text.chars().collect();
    let new_dot = new_chars.iter().position(|&c| c == '.').unwrap_or(new_chars.len());
    let new_di: isize = if power >= 0 {
        new_dot as isize - 1 - power as isize
    } else {
        new_dot as isize - power as isize
    };
    let new_caret = (new_di + 1).clamp(0, new_chars.len() as isize) as usize;
    Some((new_text, new_caret))
}

/// A fingerprint of the device/stream/RPC structure — excludes live values and
/// the fluctuating sample rate, so a reset re-emitting identical structure is a
/// no-op for the sidebar (and keyboard focus survives).
fn metadata_signature(devices: &[DeviceInfo]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for d in devices {
        let _ = writeln!(s, "D{}|{}|{}", d.route, d.meta.name, d.meta.serial_number);
        for st in &d.streams {
            let _ = writeln!(s, "S{}:{}", st.stream_id, st.name);
            for c in &st.columns {
                let _ = writeln!(s, "C{}:{}:{}", c.key.column_index, c.name, c.units);
            }
        }
        for r in &d.rpcs {
            let _ = writeln!(s, "R{}:{}:{}", r.name, r.arg_type, r.permissions);
        }
    }
    s
}

// ---- Small helpers ----------------------------------------------------------

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

fn channel_label(col: &ColumnInfo) -> String {
    let name = if col.name.is_empty() {
        format!("{}:{}", col.key.stream_id, col.key.column_index)
    } else {
        col.name.clone()
    };
    if col.units.is_empty() {
        name
    } else {
        format!("{name} [{}]", col.units)
    }
}

fn fmt_value(v: f64) -> String {
    if !v.is_finite() {
        "—".to_string()
    } else {
        let a = v.abs();
        if v == 0.0 {
            "0".to_string()
        } else if a >= 1e6 || a < 1e-3 {
            format!("{v:.4e}")
        } else {
            format!("{v:.4}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::step_digit;

    #[test]
    fn steps_the_digit_under_the_caret() {
        // Caret after the tens digit → ±10, keeping one fractional digit.
        assert_eq!(step_digit("10.0", 1, 1), Some(("20.0".to_string(), 1)));
        // Caret after the units digit → ±1.
        assert_eq!(step_digit("10.0", 2, 1), Some(("11.0".to_string(), 2)));
        // Caret in the tenths → ±0.1.
        assert_eq!(step_digit("10.0", 4, -1), Some(("9.9".to_string(), 3)));
        // Integers keep zero fractional digits; length growth keeps the caret.
        assert_eq!(step_digit("10", 2, 1), Some(("11".to_string(), 2)));
        assert_eq!(step_digit("9", 1, 1), Some(("10".to_string(), 2)));
        // Negative values.
        assert_eq!(step_digit("-1.5", 4, 1), Some(("-1.4".to_string(), 4)));
    }

    #[test]
    fn rejects_non_decimal_input() {
        assert_eq!(step_digit("1.5e3", 0, 1), None);
        assert_eq!(step_digit("abc", 1, 1), None);
        assert_eq!(step_digit("", 0, 1), None);
    }
}
