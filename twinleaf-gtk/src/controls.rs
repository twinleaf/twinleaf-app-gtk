// SPDX-License-Identifier: Apache-2.0
//! Main-view RPC controls: setting *sliders* and *capture* graphs live here, in
//! a tray below the plot panes — not in the sidebar. Mirrors the SwiftUI app,
//! where a slider is promoted from a setting (its name's context menu) and a
//! capture graph appears when the sidebar's "Capture" button is clicked. The
//! sidebar keeps a compact value field for every setting; these are the
//! on-demand, graph-sized views.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use serde_json::Value;

use crate::app::AppState;
use crate::model::RpcInfo;
use crate::persistence::Preferences;
use crate::plot::{draw_text, fmt_tick, nice_ticks, text_layout, text_w};
use crate::sidebar::{arg_from_text, step_digit};

/// One promoted slider: the (route, name) members it drives, its shared
/// adjustment, a suppression flag for programmatic updates, and its root row
/// widget (for removal).
struct SliderCtl {
    members: Vec<(String, String)>,
    adj: gtk::Adjustment,
    suppress: Rc<Cell<bool>>,
    root: gtk::Widget,
}

/// One capture graph: its members, the readout points, the drawing area, and
/// its root widget.
struct CaptureCtl {
    members: Vec<(String, String)>,
    data: Rc<RefCell<Vec<(f64, f64)>>>,
    area: gtk::DrawingArea,
    root: gtk::Widget,
    /// Auto re-trigger toggle, plus a guard so only one re-trigger is ever
    /// pending (avoids the loop doubling if a manual trigger lands mid-cycle).
    auto: Rc<Cell<bool>>,
    scheduled: Rc<Cell<bool>>,
    /// Axis titles ("name (units)") from the capture metadata.
    x_title: Rc<RefCell<String>>,
    y_title: Rc<RefCell<String>>,
}

pub struct MainControls {
    /// The tray, appended to the plot column below the panes.
    pub widget: gtk::Box,
    state: Rc<AppState>,
    has_plot_panes: Cell<bool>,
    sliders: RefCell<Vec<SliderCtl>>,
    captures: RefCell<Vec<CaptureCtl>>,
}

impl MainControls {
    pub fn new(state: &Rc<AppState>) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.set_margin_start(6);
        widget.set_margin_end(6);
        widget.set_margin_bottom(6);
        Rc::new(Self {
            widget,
            state: state.clone(),
            has_plot_panes: Cell::new(false),
            sliders: RefCell::new(Vec::new()),
            captures: RefCell::new(Vec::new()),
        })
    }

    pub fn set_has_plot_panes(&self, has_plot_panes: bool) {
        if self.has_plot_panes.get() == has_plot_panes {
            return;
        }
        self.has_plot_panes.set(has_plot_panes);
        self.update_tray_expansion();
    }

    fn update_tray_expansion(&self) {
        let has_captures = !self.captures.borrow().is_empty();
        if has_captures {
            self.widget.set_vexpand(true);
            self.widget.set_valign(gtk::Align::Fill);
        } else if self.has_plot_panes.get() {
            self.widget.set_vexpand(false);
            self.widget.set_valign(gtk::Align::Fill);
        } else {
            self.widget.set_vexpand(true);
            self.widget.set_valign(gtk::Align::Center);
        }
    }

    /// Writes the given arg to every member device (one control drives all in
    /// unified mode).
    fn write_all(&self, members: &[(String, String)], arg: Option<Value>) {
        for (r, n) in members {
            self.state.call_rpc(r.clone(), n.clone(), arg.clone());
        }
    }

    /// Promote a setting to a slider in the main view (from the sidebar name's
    /// context menu). A second request for the same members is a no-op.
    pub fn add_slider(self: &Rc<Self>, rpc: &RpcInfo, members: &[(String, String)], label: &str) {
        if self.sliders.borrow().iter().any(|s| s.members == members) {
            return;
        }
        let value = rpc.value.as_ref().and_then(Value::as_f64).unwrap_or(0.0);
        let (mut lo, mut hi) = slider_bounds(rpc, value);
        lo = lo.min(value);
        hi = hi.max(value);
        if hi <= lo {
            hi = lo + 1.0;
        }
        let integer = is_integer_rpc(rpc);
        let step = if integer {
            1.0
        } else {
            ((hi - lo) / 200.0).max(f64::MIN_POSITIVE)
        };
        let adj = gtk::Adjustment::new(value, lo, hi, step, step * 10.0, 0.0);

        // macOS-style layout: the setting name (with a remove button), the slider
        // full-width, then min / value / max editable fields beneath it.
        let container = gtk::Box::new(gtk::Orientation::Vertical, 2);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Label::new(Some(label));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        let remove = gtk::Button::with_label("✕");
        remove.add_css_class("flat");
        remove.set_tooltip_text(Some("Remove slider"));
        header.append(&name);
        header.append(&remove);
        container.append(&header);

        let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adj));
        scale.set_hexpand(true);
        scale.set_draw_value(false);
        container.append(&scale);

        // Value field: a plain editable entry (no +/- stepper); Up/Down step the
        // digit at the caret instead of moving it.
        let entry = gtk::Entry::new();
        entry.set_width_chars(8);
        gtk::prelude::EditableExt::set_alignment(&entry, 0.5);
        entry.set_text(&fmt_slider(value, integer));

        let suppress = Rc::new(Cell::new(false));
        let scheduled = Rc::new(Cell::new(false));
        let focused = Rc::new(Cell::new(false));

        // Scale drag → rate-limited write; mirror the value into the field unless
        // the user is editing it.
        {
            let this = self.clone();
            let members = members.to_vec();
            let rpc = rpc.clone();
            let suppress = suppress.clone();
            let scheduled = scheduled.clone();
            let entry = entry.clone();
            let focused = focused.clone();
            adj.connect_value_changed(move |a| {
                if !focused.get() {
                    entry.set_text(&fmt_slider(a.value(), integer));
                }
                if suppress.get() || scheduled.get() {
                    return;
                }
                scheduled.set(true);
                let this = this.clone();
                let members = members.clone();
                let rpc = rpc.clone();
                let a = a.clone();
                let scheduled = scheduled.clone();
                glib::timeout_add_local_once(Duration::from_millis(100), move || {
                    scheduled.set(false);
                    if let Some(arg) = slider_arg(&rpc, a.value()) {
                        this.write_all(&members, Some(arg));
                    }
                });
            });
        }

        // Field → device: grow the range if needed, move the scale (suppressed so
        // it doesn't double-write), and write immediately.
        let set_and_write: Rc<dyn Fn(f64)> = {
            let this = self.clone();
            let members = members.to_vec();
            let rpc = rpc.clone();
            let adj = adj.clone();
            let suppress = suppress.clone();
            Rc::new(move |v: f64| {
                suppress.set(true);
                if v > adj.upper() {
                    adj.set_upper(v);
                }
                if v < adj.lower() {
                    adj.set_lower(v);
                }
                adj.set_value(v);
                suppress.set(false);
                if let Some(arg) = slider_arg(&rpc, v) {
                    this.write_all(&members, Some(arg));
                }
            })
        };

        // Commit on Enter and on focus-out.
        {
            let entry2 = entry.clone();
            let set_and_write = set_and_write.clone();
            entry.connect_activate(move |_| {
                if let Ok(v) = entry2.text().trim().parse::<f64>() {
                    set_and_write(v);
                }
            });
        }
        {
            let focus = gtk::EventControllerFocus::new();
            {
                let focused = focused.clone();
                focus.connect_enter(move |_| focused.set(true));
            }
            {
                let focused = focused.clone();
                let entry2 = entry.clone();
                let set_and_write = set_and_write.clone();
                focus.connect_leave(move |_| {
                    focused.set(false);
                    if let Ok(v) = entry2.text().trim().parse::<f64>() {
                        set_and_write(v);
                    }
                });
            }
            entry.add_controller(focus);
        }

        // Up/Down step the digit at the caret and commit, without moving focus.
        {
            let key = gtk::EventControllerKey::new();
            let entry2 = entry.clone();
            let set_and_write = set_and_write.clone();
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
                    if let Ok(v) = new_text.trim().parse::<f64>() {
                        set_and_write(v);
                    }
                }
                glib::Propagation::Stop
            });
            entry.add_controller(key);
        }

        // Min / value / max beneath the slider: min flush-left, value centered,
        // max flush-right (a CenterBox gives exactly that). Editing min/max moves
        // the slider's bounds; the fields refresh when the range changes.
        let (min_entry, min_focused) = bound_field(&adj, integer, true);
        let (max_entry, max_focused) = bound_field(&adj, integer, false);
        {
            let min_entry = min_entry.clone();
            let max_entry = max_entry.clone();
            adj.connect_changed(move |a| {
                if !min_focused.get() {
                    min_entry.set_text(&fmt_slider(a.lower(), integer));
                }
                if !max_focused.get() {
                    max_entry.set_text(&fmt_slider(a.upper(), integer));
                }
            });
        }
        let fields = gtk::CenterBox::new();
        fields.set_hexpand(true);
        fields.set_start_widget(Some(&min_entry));
        fields.set_center_widget(Some(&entry));
        fields.set_end_widget(Some(&max_entry));
        container.append(&fields);

        self.widget.append(&container);
        let root: gtk::Widget = container.upcast();
        {
            let this = Rc::downgrade(self);
            let root = root.clone();
            remove.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.widget.remove(&root);
                    this.sliders.borrow_mut().retain(|s| s.root != root);
                }
            });
        }
        self.sliders.borrow_mut().push(SliderCtl {
            members: members.to_vec(),
            adj,
            suppress,
            root,
        });
        // Read a fresh value so the slider starts where the device is.
        if let Some((r, n)) = members.first() {
            self.state.call_rpc(r.clone(), n.clone(), None);
        }
    }

    /// Add (or re-trigger) a capture graph in the main view (from the sidebar's
    /// "Capture" button).
    pub fn add_capture(self: &Rc<Self>, _rpc: &RpcInfo, members: &[(String, String)], label: &str) {
        if self.captures.borrow().iter().any(|c| c.members == members) {
            self.write_all(members, None); // already shown — just trigger again
            return;
        }
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);
        outer.set_vexpand(true);
        outer.set_valign(gtk::Align::Fill);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Label::new(Some(label));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        let trigger = gtk::Button::with_label("Trigger");
        trigger.set_valign(gtk::Align::Center);
        let auto_btn = gtk::ToggleButton::with_label("Auto");
        auto_btn.set_valign(gtk::Align::Center);
        auto_btn.set_tooltip_text(Some("Re-trigger automatically after each capture"));
        let close = gtk::Button::with_label("✕");
        close.add_css_class("flat");
        close.set_tooltip_text(Some("Close capture"));
        head.append(&name);
        head.append(&trigger);
        head.append(&auto_btn);
        head.append(&close);

        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_valign(gtk::Align::Fill);
        area.set_content_height(220);
        let data: Rc<RefCell<Vec<(f64, f64)>>> = Rc::new(RefCell::new(Vec::new()));
        let x_title: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let y_title: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        {
            let data = data.clone();
            let x_title = x_title.clone();
            let y_title = y_title.clone();
            area.set_draw_func(move |area, cr, w, h| {
                draw_capture(
                    area,
                    cr,
                    w as f64,
                    h as f64,
                    &data.borrow(),
                    &x_title.borrow(),
                    &y_title.borrow(),
                );
            });
        }
        outer.append(&head);
        outer.append(&area);
        self.widget.append(&outer);
        let root: gtk::Widget = outer.upcast();

        let auto = Rc::new(Cell::new(false));
        let scheduled = Rc::new(Cell::new(false));

        {
            let this = self.clone();
            let members = members.to_vec();
            trigger.connect_clicked(move |_| this.write_all(&members, None));
        }
        {
            // Toggling Auto on fires a capture; the result-driven loop in
            // update_rpc_value keeps it going, paced by the delay preference.
            let this = self.clone();
            let members = members.to_vec();
            let auto = auto.clone();
            auto_btn.connect_toggled(move |b| {
                auto.set(b.is_active());
                if b.is_active() {
                    this.write_all(&members, None);
                }
            });
        }
        {
            let this = Rc::downgrade(self);
            let root = root.clone();
            close.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.widget.remove(&root);
                    this.captures.borrow_mut().retain(|c| c.root != root);
                    this.update_tray_expansion();
                }
            });
        }
        self.captures.borrow_mut().push(CaptureCtl {
            members: members.to_vec(),
            data,
            area,
            root,
            auto,
            scheduled,
            x_title,
            y_title,
        });
        self.update_tray_expansion();
        self.write_all(members, None); // trigger the first capture now
    }

    /// Route an `rpcResult` value to any slider/capture that owns this member.
    pub fn update_rpc_value(self: &Rc<Self>, route: &str, name: &str, value: &Value) {
        for s in self.sliders.borrow().iter() {
            if s.members.iter().any(|(r, n)| r == route && n == name) {
                if let Some(v) = value.as_f64() {
                    s.suppress.set(true);
                    if v > s.adj.upper() {
                        s.adj.set_upper(v);
                    }
                    if v < s.adj.lower() {
                        s.adj.set_lower(v);
                    }
                    s.adj.set_value(v);
                    s.suppress.set(false);
                }
            }
        }
        for c in self.captures.borrow().iter() {
            if c.members.iter().any(|(r, n)| r == route && n == name) {
                *c.data.borrow_mut() = parse_points(value);
                let (xt, yt) = capture_titles(value);
                *c.x_title.borrow_mut() = xt;
                *c.y_title.borrow_mut() = yt;
                c.area.queue_draw();
                // Auto mode: this capture completed — schedule the next trigger
                // after the configured delay (guarded so only one is pending).
                if c.auto.get() && !c.scheduled.get() {
                    c.scheduled.set(true);
                    let delay = Preferences::load()
                        .capture_auto_delay_seconds
                        .clamp(0.05, 10.0);
                    let this = Rc::downgrade(self);
                    let members = c.members.clone();
                    let auto = c.auto.clone();
                    let scheduled = c.scheduled.clone();
                    glib::timeout_add_local_once(Duration::from_secs_f64(delay), move || {
                        scheduled.set(false);
                        if auto.get() {
                            if let Some(this) = this.upgrade() {
                                this.write_all(&members, None);
                            }
                        }
                    });
                }
            }
        }
    }

    /// Test hook: turn Auto on for every capture and fire an initial trigger, as
    /// if each "Auto" toggle were clicked.
    pub fn selftest_enable_auto(self: &Rc<Self>) {
        let members: Vec<Vec<(String, String)>> = {
            let caps = self.captures.borrow();
            for c in caps.iter() {
                c.auto.set(true);
            }
            caps.iter().map(|c| c.members.clone()).collect()
        };
        for m in &members {
            self.write_all(m, None);
        }
    }

    /// Remove every control (a fresh connection starts clean).
    pub fn clear(&self) {
        // Stop any pending auto re-trigger before dropping the captures.
        for c in self.captures.borrow().iter() {
            c.auto.set(false);
        }
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
        self.sliders.borrow_mut().clear();
        self.captures.borrow_mut().clear();
        self.update_tray_expansion();
    }
}

fn parse_points(value: &Value) -> Vec<(f64, f64)> {
    value
        .get("points")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|pt| {
                    let a = pt.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---- Slider/capture helpers (moved here from the sidebar) -------------------

/// Format a slider value for its editable field: no decimals for integer RPCs,
/// three otherwise.
fn fmt_slider(v: f64, integer: bool) -> String {
    if integer {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.3}")
    }
}

/// Build an editable min/max bound field for a slider. Committing it moves the
/// adjustment's lower (`is_min`) or upper bound (kept ordered). Returns the entry
/// and its focus flag, so the range-refresh handler can skip it while editing.
fn bound_field(adj: &gtk::Adjustment, integer: bool, is_min: bool) -> (gtk::Entry, Rc<Cell<bool>>) {
    let entry = gtk::Entry::new();
    entry.set_width_chars(6);
    gtk::prelude::EditableExt::set_alignment(&entry, if is_min { 0.0 } else { 1.0 });
    entry.set_text(&fmt_slider(
        if is_min { adj.lower() } else { adj.upper() },
        integer,
    ));
    let focused = Rc::new(Cell::new(false));

    let commit: Rc<dyn Fn()> = {
        let adj = adj.clone();
        let entry = entry.clone();
        Rc::new(move || {
            let Ok(v) = entry.text().trim().parse::<f64>() else {
                return;
            };
            if is_min {
                if v < adj.upper() {
                    adj.set_lower(v);
                }
            } else if v > adj.lower() {
                adj.set_upper(v);
            }
        })
    };
    {
        let commit = commit.clone();
        entry.connect_activate(move |_| commit());
    }
    {
        let focus = gtk::EventControllerFocus::new();
        {
            let focused = focused.clone();
            focus.connect_enter(move |_| focused.set(true));
        }
        {
            let focused = focused.clone();
            let commit = commit.clone();
            focus.connect_leave(move |_| {
                focused.set(false);
                commit();
            });
        }
        entry.add_controller(focus);
    }
    (entry, focused)
}

fn is_integer_rpc(rpc: &RpcInfo) -> bool {
    matches!(
        rpc.base_arg_type(),
        "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64"
    )
}

fn is_signed_rpc(rpc: &RpcInfo) -> bool {
    let b = rpc.base_arg_type();
    b.starts_with('i') || b == "f32" || b == "f64"
}

/// Slider bounds: a `<lo..hi>` range in the arg type if present, else a default
/// scaled to the current value (0 or −M .. M, with M the next power of ten).
fn slider_bounds(rpc: &RpcInfo, value: f64) -> (f64, f64) {
    if let Some(range) = parse_range(&rpc.arg_type) {
        return range;
    }
    let m = value.abs();
    let hi = if m > 0.0 {
        10f64.powf(m.log10().ceil()).max(1.0)
    } else {
        1.0
    };
    let lo = if is_signed_rpc(rpc) { -hi } else { 0.0 };
    (lo, hi)
}

/// Parse a `<lo..hi>` range suffix from an arg type, if present.
fn parse_range(arg_type: &str) -> Option<(f64, f64)> {
    let inner = arg_type.split_once('<')?.1;
    let inner = inner.strip_suffix('>').unwrap_or(inner);
    let (lo, hi) = inner.split_once("..")?;
    Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?))
}

/// A typed RPC argument from a slider value.
fn slider_arg(rpc: &RpcInfo, v: f64) -> Option<Value> {
    let s = if is_integer_rpc(rpc) {
        (v.round() as i64).to_string()
    } else {
        format!("{v}")
    };
    arg_from_text(rpc, &s)
}

/// Combine an axis name and units into "name (units)" (or just the name).
fn axis_title(name: &str, units: &str) -> String {
    match (name.is_empty(), units.is_empty()) {
        (true, _) => String::new(),
        (false, true) => name.to_string(),
        (false, false) => format!("{name} ({units})"),
    }
}

/// Read the (x, y) axis titles from a capture result's metadata.
fn capture_titles(value: &Value) -> (String, String) {
    let m = value.get("metadata");
    let get = |k: &str| {
        m.and_then(|m| m.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    };
    (
        axis_title(get("xName"), get("xUnits")),
        axis_title(get("name"), get("units")),
    )
}

/// Draw a capture's waveform with linear x/y axes — ticks, value labels, and
/// axis titles (from the capture metadata), mirroring the macOS capture plot.
fn draw_capture(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    w: f64,
    h: f64,
    points: &[(f64, f64)],
    x_title: &str,
    y_title: &str,
) {
    let layout = text_layout(area, cr);
    let c = area.color();
    let (fr, fg, fb) = (c.red() as f64, c.green() as f64, c.blue() as f64);

    let ml = 60.0;
    let mr = 12.0;
    let mt = 8.0;
    let mb = 34.0;
    let ax = ml;
    let ay = mt;
    let aw = (w - ml - mr).max(1.0);
    let ah = (h - mt - mb).max(1.0);

    cr.set_source_rgba(fr, fg, fb, 0.35);
    cr.set_line_width(1.0);
    cr.rectangle(ax, ay, aw, ah);
    let _ = cr.stroke();

    let (mut x0, mut x1, mut y0, mut y1) = (
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in points {
        if x.is_finite() && y.is_finite() {
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if !(x1 >= x0) || !y0.is_finite() || !y1.is_finite() {
        cr.set_source_rgba(fr, fg, fb, 0.55);
        let msg = "No capture data";
        let tw = text_w(&layout, msg);
        draw_text(cr, &layout, ax + (aw - tw) / 2.0, ay + ah / 2.0, msg);
        return;
    }
    if (x1 - x0).abs() < 1e-12 {
        x0 -= 0.5;
        x1 += 0.5;
    }
    let ypad = if (y1 - y0).abs() < 1e-12 {
        1.0
    } else {
        (y1 - y0) * 0.08
    };
    y0 -= ypad;
    y1 += ypad;

    let px = |x: f64| ax + (x - x0) / (x1 - x0) * aw;
    let py = |y: f64| ay + ah - (y - y0) / (y1 - y0) * ah;

    let yt = ((ah / 50.0) as usize).clamp(2, 6);
    let xt = ((aw / 80.0) as usize).clamp(2, 6);

    // Y gridlines + labels (left).
    for v in nice_ticks(y0, y1, yt) {
        if v < y0 || v > y1 {
            continue;
        }
        let y = py(v);
        cr.set_source_rgba(fr, fg, fb, 0.12);
        cr.set_line_width(0.5);
        cr.move_to(ax, y);
        cr.line_to(ax + aw, y);
        let _ = cr.stroke();
        let label = fmt_tick(v);
        let lw = text_w(&layout, &label);
        cr.set_source_rgba(fr, fg, fb, 0.75);
        draw_text(cr, &layout, (ax - 6.0 - lw).max(2.0), y + 4.0, &label);
    }
    // X gridlines + labels (bottom).
    for v in nice_ticks(x0, x1, xt) {
        if v < x0 || v > x1 {
            continue;
        }
        let x = px(v);
        cr.set_source_rgba(fr, fg, fb, 0.12);
        cr.set_line_width(0.5);
        cr.move_to(x, ay);
        cr.line_to(x, ay + ah);
        let _ = cr.stroke();
        let label = fmt_tick(v);
        let lw = text_w(&layout, &label);
        cr.set_source_rgba(fr, fg, fb, 0.75);
        draw_text(cr, &layout, x - lw / 2.0, ay + ah + 14.0, &label);
    }

    // Axis titles.
    if !x_title.is_empty() {
        let tw = text_w(&layout, x_title);
        cr.set_source_rgba(fr, fg, fb, 0.7);
        draw_text(cr, &layout, ax + (aw - tw) / 2.0, ay + ah + 28.0, x_title);
    }
    if !y_title.is_empty() {
        let tw = text_w(&layout, y_title);
        let _ = cr.save();
        cr.set_source_rgba(fr, fg, fb, 0.7);
        cr.translate(12.0, ay + ah / 2.0 + tw / 2.0);
        cr.rotate(-std::f64::consts::FRAC_PI_2);
        draw_text(cr, &layout, 0.0, 0.0, y_title);
        let _ = cr.restore();
    }

    // Waveform.
    cr.set_source_rgb(0.20, 0.51, 0.93);
    cr.set_line_width(1.4);
    let mut started = false;
    for &(x, y) in points {
        if !x.is_finite() || !y.is_finite() {
            started = false;
            continue;
        }
        let (cx, cy) = (px(x), py(y));
        if started {
            cr.line_to(cx, cy);
        } else {
            cr.move_to(cx, cy);
            started = true;
        }
    }
    let _ = cr.stroke();
}
