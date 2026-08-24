// SPDX-License-Identifier: Apache-2.0
//! Plot surface: a `gtk::DrawingArea` rendered with Cairo from the core's
//! already-decimated [`PlotFrame`]s. Timeseries uses linear axes; FFT uses
//! log-log axes (amplitude spectral density vs frequency). Colours and font are
//! taken from the active GTK theme (`area.color()` + the system UI font), so
//! the plot follows the system light/dark scheme automatically. One
//! `PlotCanvas` per pane.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk::cairo;
use gtk::pango;
use gtk::prelude::*;
use gtk::DrawingArea;
use twinleaf_core::{ColumnKeyDto, PlotFrame, PlotMode, PlotSeriesData, Point};

/// Trace palette (works on light and dark backgrounds), mirroring the SwiftUI
/// app's 5-color set.
const PALETTE: [(f64, f64, f64); 5] = [
    (0.20, 0.51, 0.93),
    (0.86, 0.25, 0.22),
    (0.18, 0.66, 0.38),
    (0.85, 0.55, 0.10),
    (0.55, 0.36, 0.82),
];

#[derive(Default)]
struct PlotState {
    frame: Option<Arc<PlotFrame>>,
    window_seconds: f64,
    labels: HashMap<ColumnKeyDto, (String, String)>,
    fft_log_x: bool,
    fft_log_y: bool,
    cursor_x: Option<f64>,
    /// Axis-range hysteresis memory: the scale context plus the last applied
    /// bounds (in transformed/log space) for x and y, so the axes don't jump
    /// frame to frame. Reset when the context (mode / log flags) changes.
    axis_ctx: Cell<u8>,
    last_x: Cell<Option<(f64, f64)>>,
    last_y: Cell<Option<(f64, f64)>>,
    /// Absolute timestamp of the first sample since connect (the live plot time
    /// origin). The lower-right "+ N s" offset under a timeseries plot is
    /// measured from here; reset on reconnect via [`PlotCanvas::reset_origin`].
    time_origin: Cell<Option<f64>>,
}

/// A single plot pane.
pub struct PlotCanvas {
    pub widget: DrawingArea,
    state: Rc<RefCell<PlotState>>,
}

impl PlotCanvas {
    pub fn new() -> Self {
        let widget = DrawingArea::new();
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        widget.set_content_width(480);
        widget.set_content_height(220);

        let state = Rc::new(RefCell::new(PlotState {
            window_seconds: 10.0,
            fft_log_x: true,
            fft_log_y: true,
            ..PlotState::default()
        }));

        let draw_state = state.clone();
        widget.set_draw_func(move |area, cr, width, height| {
            draw(area, cr, width, height, &draw_state.borrow());
        });

        let motion = gtk::EventControllerMotion::new();
        let motion_state = state.clone();
        let motion_widget = widget.clone();
        motion.connect_motion(move |_, x, _y| {
            motion_state.borrow_mut().cursor_x = Some(x);
            motion_widget.queue_draw();
        });
        let leave_state = state.clone();
        let leave_widget = widget.clone();
        motion.connect_leave(move |_| {
            leave_state.borrow_mut().cursor_x = None;
            leave_widget.queue_draw();
        });
        widget.add_controller(motion);

        Self { widget, state }
    }

    pub fn set_frame(&self, frame: Arc<PlotFrame>) {
        self.state.borrow_mut().frame = Some(frame);
        self.widget.queue_draw();
    }

    pub fn set_labels(&self, labels: HashMap<ColumnKeyDto, (String, String)>) {
        self.state.borrow_mut().labels = labels;
        self.widget.queue_draw();
    }

    pub fn set_window_seconds(&self, seconds: f64) {
        self.state.borrow_mut().window_seconds = seconds;
        self.widget.queue_draw();
    }

    /// Set whether FFT mode uses log x / log y axes.
    pub fn set_fft_log(&self, log_x: bool, log_y: bool) {
        {
            let mut s = self.state.borrow_mut();
            s.fft_log_x = log_x;
            s.fft_log_y = log_y;
        }
        self.widget.queue_draw();
    }

    /// The latest frame's view-end time (right edge), for pan gesture math.
    pub fn view_end(&self) -> Option<f64> {
        self.state
            .borrow()
            .frame
            .as_ref()
            .and_then(|f| f.viewport_end)
    }

    /// Forget the live time origin so the next frame re-seeds it. Called on a
    /// new connection, mirroring the mac's `resetLivePlotTiming`.
    pub fn reset_origin(&self) {
        self.state.borrow().time_origin.set(None);
    }
}

const ML: f64 = 74.0;
const MR: f64 = 14.0;
const MT: f64 = 14.0;

/// Theme-derived colours, computed from the widget's current foreground colour.
struct Theme {
    fg: (f64, f64, f64),
    bg: (f64, f64, f64),
}

fn theme(area: &DrawingArea) -> Theme {
    let c = area.color();
    let fg = (c.red() as f64, c.green() as f64, c.blue() as f64);
    let lum = 0.299 * fg.0 + 0.587 * fg.1 + 0.114 * fg.2;
    // Light foreground text => dark theme, and vice versa.
    let dark = lum > 0.5;
    let bg = if dark {
        (0.11, 0.12, 0.14)
    } else {
        (0.99, 0.99, 0.99)
    };
    Theme { fg, bg }
}

/// Forward axis transform: identity for linear, log10 for log (None if <= 0).
fn fwd(v: f64, log: bool) -> Option<f64> {
    if !v.is_finite() {
        None
    } else if log {
        (v > 0.0).then(|| v.log10())
    } else {
        Some(v)
    }
}

/// A Pango layout that renders with the widget's resolved font — matching the
/// rest of the UI, including the macOS system font (`.AppleSystemUIFont`), which
/// Cairo's toy font API can't resolve.
pub(crate) fn text_layout(area: &DrawingArea, cr: &cairo::Context) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(area.pango_context().font_description().as_ref());
    layout
}

/// Pixel width of `s` in the layout's font.
pub(crate) fn text_w(layout: &pango::Layout, s: &str) -> f64 {
    layout.set_text(s);
    layout.pixel_size().0 as f64
}

/// Draw `s` with its baseline at (x, baseline_y) — matching Cairo's `show_text`
/// placement — but rendered through Pango so the system font resolves.
pub(crate) fn draw_text(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    baseline_y: f64,
    s: &str,
) {
    layout.set_text(s);
    let baseline = layout.baseline() as f64 / pango::SCALE as f64;
    cr.move_to(x, baseline_y - baseline);
    pangocairo::functions::show_layout(cr, layout);
}

fn draw(area: &DrawingArea, cr: &cairo::Context, width: i32, height: i32, st: &PlotState) {
    let th = theme(area);
    // Render text via Pango using the widget's resolved font, so it matches the
    // rest of the UI (the macOS system font's `.AppleSystemUIFont` can't be
    // resolved by Cairo's "toy" font API).
    let layout = text_layout(area, cr);
    let (fr, fg, fb) = th.fg;
    let w = width as f64;
    let h = height as f64;

    cr.set_source_rgb(th.bg.0, th.bg.1, th.bg.2);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    let is_fft = st
        .frame
        .as_ref()
        .map(|f| f.mode == PlotMode::Fft)
        .unwrap_or(false);
    // Timeseries reserves an extra label row under the axis for the centered
    // "Time (s)" title plus the lower-right recording offset.
    let mb = if is_fft { 34.0 } else { 48.0 };
    let ax = ML;
    let ay = MT;
    let aw = (w - ML - MR).max(1.0);
    let ah = (h - MT - mb).max(1.0);

    cr.set_source_rgba(fr, fg, fb, 0.35);
    cr.set_line_width(1.0);
    cr.rectangle(ax, ay, aw, ah);
    let _ = cr.stroke();

    let Some(frame) = st.frame.clone() else {
        centered(
            cr,
            &layout,
            &th,
            w,
            h,
            "No data — connect a device and select a channel",
        );
        return;
    };

    let x_log = is_fft && st.fft_log_x;
    let y_log = is_fft && st.fft_log_y;

    let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    for s in &frame.series {
        for p in &s.points {
            if let (Some(tx), Some(ty)) = (fwd(p.x, x_log), fwd(p.y, y_log)) {
                xmin = xmin.min(tx);
                xmax = xmax.max(tx);
                ymin = ymin.min(ty);
                ymax = ymax.max(ty);
            }
        }
    }
    // Seed the live time origin from the first frame's earliest sample, so the
    // recording offset under the plot counts seconds since streaming began.
    // Re-seed if timestamps jump backward (a new device/log); within a session
    // the earliest sample only advances as the buffer trims, so this is stable.
    if !is_fft && xmin.is_finite() {
        let prev = st.time_origin.get();
        if prev.map_or(true, |o| xmin < o - 1e-6) {
            st.time_origin.set(Some(xmin));
        }
    }
    if !is_fft {
        if let Some(end) = frame.viewport_end {
            xmax = end;
            xmin = end - st.window_seconds.max(1e-6);
        }
    }
    if !xmin.is_finite() || !xmax.is_finite() || xmax <= xmin {
        centered(cr, &layout, &th, w, h, "Waiting for samples…");
        return;
    }
    if !ymin.is_finite() || !ymax.is_finite() {
        ymin = -1.0;
        ymax = 1.0;
    }
    if (ymax - ymin).abs() < 1e-12 {
        ymin -= 1.0;
        ymax += 1.0;
    }

    // Stabilize the axes with hysteresis so they don't jump frame to frame:
    // expand immediately, but only contract once the data recedes past a margin.
    // Bounds are in transformed space, so log axes snap to whole decades.
    let (raw_xmin, raw_xmax, raw_ymin, raw_ymax) = (xmin, xmax, ymin, ymax);
    let ctx = (is_fft as u8) | ((x_log as u8) << 1) | ((y_log as u8) << 2);
    if st.axis_ctx.get() != ctx {
        st.axis_ctx.set(ctx);
        st.last_x.set(None);
        st.last_y.set(None);
    }

    let ideal_y = if y_log {
        nice_log(ymin, ymax)
    } else {
        let pad = (ymax - ymin) * 0.06;
        (ymin - pad, ymax + pad)
    };
    let y_margin = if y_log {
        0.5
    } else {
        (ideal_y.1 - ideal_y.0) * 0.12
    };
    let (ny0, ny1) = apply_hysteresis(st.last_y.get(), ideal_y, (raw_ymin, raw_ymax), y_margin);
    st.last_y.set(Some((ny0, ny1)));
    ymin = ny0;
    ymax = ny1;

    // FFT frequency (x) gets the same treatment; timeseries keeps its fixed
    // time window.
    if is_fft {
        let ideal_x = if x_log {
            nice_log(xmin, xmax)
        } else {
            let pad = (xmax - xmin) * 0.06;
            (xmin - pad, xmax + pad)
        };
        let x_margin = if x_log {
            0.5
        } else {
            (ideal_x.1 - ideal_x.0) * 0.12
        };
        let (nx0, nx1) = apply_hysteresis(st.last_x.get(), ideal_x, (raw_xmin, raw_xmax), x_margin);
        st.last_x.set(Some((nx0, nx1)));
        xmin = nx0;
        xmax = nx1;
    }

    // Timeseries shows time relative to the right edge — 0 on the right, negative
    // to the left — like the mac. `x0` is the absolute right-edge time (used for
    // the recording offset); FFT keeps absolute frequency.
    let x0 = if is_fft { 0.0 } else { xmax };
    let (xmin, xmax) = (xmin - x0, xmax - x0);

    let px = |x: f64| ax + (x - xmin) / (xmax - xmin) * aw;
    let py = |y: f64| ay + ah - (y - ymin) / (ymax - ymin) * ah;
    let xunit = if is_fft { "Hz" } else { "s" };

    // Y gridlines + labels.
    for (pos, label) in axis_ticks(ymin, ymax, y_log) {
        let y = py(pos);
        cr.set_source_rgba(fr, fg, fb, if label.is_some() { 0.18 } else { 0.08 });
        cr.set_line_width(0.5);
        cr.move_to(ax, y);
        cr.line_to(ax + aw, y);
        let _ = cr.stroke();
        if let Some(text) = label {
            let lw = text_w(&layout, &text);
            cr.set_source_rgba(fr, fg, fb, 0.75);
            draw_text(cr, &layout, (ax - 6.0 - lw).max(18.0), y + 3.0, &text);
        }
    }
    // X gridlines + labels.
    for (pos, label) in axis_ticks(xmin, xmax, x_log) {
        let x = px(pos);
        cr.set_source_rgba(fr, fg, fb, if label.is_some() { 0.15 } else { 0.07 });
        cr.set_line_width(0.5);
        cr.move_to(x, ay);
        cr.line_to(x, ay + ah);
        let _ = cr.stroke();
        if let Some(text) = label {
            cr.set_source_rgba(fr, fg, fb, 0.75);
            draw_text(cr, &layout, x - 10.0, ay + ah + 14.0, &text);
        }
    }
    // Centered horizontal-axis title; for timeseries the lower-right shows the
    // recording offset — the absolute time at the right edge (t = 0).
    let xtitle = if is_fft { "frequency (Hz)" } else { "Time (s)" };
    let xtw = text_w(&layout, xtitle);
    cr.set_source_rgba(fr, fg, fb, 0.7);
    draw_text(cr, &layout, ax + (aw - xtw) / 2.0, ay + ah + 28.0, xtitle);
    if !is_fft {
        if let Some(origin) = st.time_origin.get() {
            // When the timestamps are absolute (Unix epoch), show the date/time at
            // the right edge; otherwise the elapsed offset since recording start.
            let label = unix_datetime_label(x0)
                .unwrap_or_else(|| format!("+ {:.2} s", (x0 - origin).max(0.0)));
            let ow = text_w(&layout, &label);
            cr.set_source_rgba(fr, fg, fb, 0.6);
            draw_text(cr, &layout, ax + aw - ow, ay + ah + 28.0, &label);
        }
    }

    // Vertical-axis unit title, rotated along the left margin (mirrors the
    // SwiftUI app's y-axis title).
    let ytitle = vertical_axis_title(st, &frame.series, is_fft);
    if !ytitle.is_empty() {
        let tw = text_w(&layout, &ytitle);
        let _ = cr.save();
        cr.set_source_rgba(fr, fg, fb, 0.7);
        cr.translate(13.0, ay + ah / 2.0 + tw / 2.0);
        cr.rotate(-std::f64::consts::FRAC_PI_2);
        draw_text(cr, &layout, 0.0, 0.0, &ytitle);
        let _ = cr.restore();
    }

    // Cursor x in data space, when hovering inside the plot.
    let cursor_x = st.cursor_x.filter(|&cx| cx >= ax && cx <= ax + aw);
    // Displayed cursor x: relative time for timeseries, absolute frequency for FFT.
    let cursor_data_x = cursor_x.map(|cx| {
        let tx = xmin + (cx - ax) / aw * (xmax - xmin);
        if x_log {
            10f64.powf(tx)
        } else {
            tx
        }
    });
    // Absolute x for sample lookup (points are stored absolute; timeseries x is
    // shifted by `x0`).
    let cursor_search_x = cursor_data_x.map(|x| if is_fft { x } else { x + x0 });

    // Series polylines.
    for (i, s) in frame.series.iter().enumerate() {
        let (r, g, b) = PALETTE[i % PALETTE.len()];
        cr.set_source_rgb(r, g, b);
        cr.set_line_width(1.4);
        let mut started = false;
        for p in &s.points {
            match (fwd(p.x, x_log), fwd(p.y, y_log)) {
                (Some(tx), Some(ty)) => {
                    let (cx, cy) = (px(tx - x0), py(ty));
                    if started {
                        cr.line_to(cx, cy);
                    } else {
                        cr.move_to(cx, cy);
                        started = true;
                    }
                }
                _ => started = false,
            }
        }
        let _ = cr.stroke();
    }

    // Cursor crosshair + a dot on each trace (the readout text is folded into
    // the legend below — no separate overlay box).
    if let (Some(cx), Some(search_x)) = (cursor_x, cursor_search_x) {
        cr.set_source_rgba(fr, fg, fb, 0.5);
        cr.set_line_width(0.7);
        cr.move_to(cx, ay);
        cr.line_to(cx, ay + ah);
        let _ = cr.stroke();
        for (i, s) in frame.series.iter().enumerate() {
            if let Some(p) = nearest_point(&s.points, search_x) {
                if let (Some(tx), Some(ty)) = (fwd(p.x, x_log), fwd(p.y, y_log)) {
                    let (r, g, b) = PALETTE[i % PALETTE.len()];
                    cr.set_source_rgb(r, g, b);
                    cr.arc(px(tx - x0), py(ty), 2.5, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                }
            }
        }
    }

    // Legend (top-left): colour swatch + label (with route). When the cursor is
    // active, each entry shows its value there and a footer line below shows the
    // cursor's x.
    let mut ly = ay + 14.0;
    for (i, s) in frame.series.iter().enumerate() {
        let (r, g, b) = PALETTE[i % PALETTE.len()];
        cr.set_source_rgb(r, g, b);
        cr.rectangle(ax + 10.0, ly - 8.0, 10.0, 10.0);
        let _ = cr.fill();
        let mut text = series_label(st, s);
        if let Some(search_x) = cursor_search_x {
            if let Some(p) = nearest_point(&s.points, search_x) {
                text = format!("{text}   {}", fmt_num(p.y));
            }
        } else if let Some(floor) = s.noise_floor {
            // With no cursor to read, an FFT trace shows its broadband noise
            // floor instead — the number the plot is usually being consulted
            // for. Matches the SwiftUI app's legend.
            text = format!("{text}   {}", noise_floor_text(st, s, floor));
        }
        cr.set_source_rgba(fr, fg, fb, 0.95);
        draw_text(cr, &layout, ax + 25.0, ly, &text);
        ly += 15.0;
    }
    if let Some(data_x) = cursor_data_x {
        let xlabel = if is_fft { "f" } else { "t" };
        cr.set_source_rgba(fr, fg, fb, 0.9);
        draw_text(
            cr,
            &layout,
            ax + 10.0,
            ly,
            &format!("{xlabel} = {} {xunit}", fmt_num(data_x)),
        );
    }
}

/// Legend text for a trace's white-noise floor, in `units/sqrt(Hz)`.
fn noise_floor_text(st: &PlotState, s: &PlotSeriesData, floor: f64) -> String {
    let units = st.labels.get(&s.key).map(|(_, u)| u.as_str()).unwrap_or("");
    if units.is_empty() {
        format!("floor {} ASD", fmt_num(floor))
    } else {
        format!("floor {} {units}/\u{221a}Hz", fmt_num(floor))
    }
}

fn series_label(st: &PlotState, s: &PlotSeriesData) -> String {
    let base = st
        .labels
        .get(&s.key)
        .map(|(n, u)| {
            if u.is_empty() {
                n.clone()
            } else {
                format!("{n} [{u}]")
            }
        })
        .unwrap_or_else(|| format!("{}:{}", s.key.stream_id, s.key.column_index));
    // Prefix the device route when it's meaningful (multi-device sessions), so
    // each trace is attributable to its sensor.
    let route = s.key.route.trim();
    if route.is_empty() || route == "/" {
        base
    } else {
        format!("{route}  {base}")
    }
}

/// Vertical-axis title from the plotted series' units, following the SwiftUI
/// app: a shared unit yields "value (u)" (timeseries) or "u/sqrt(Hz)" (FFT);
/// differing units read "mixed units"; a lone series uses its channel name.
fn vertical_axis_title(st: &PlotState, series: &[PlotSeriesData], is_fft: bool) -> String {
    let mut units: Vec<&str> = Vec::new();
    for s in series {
        if let Some((_, u)) = st.labels.get(&s.key) {
            if !u.is_empty() && !units.contains(&u.as_str()) {
                units.push(u.as_str());
            }
        }
    }
    let unit = (units.len() == 1).then(|| units[0]);
    let name = if series.len() == 1 {
        st.labels
            .get(&series[0].key)
            .map(|(n, _)| n.as_str())
            .filter(|n| !n.is_empty())
    } else {
        None
    };
    if is_fft {
        match (name, unit) {
            (Some(n), Some(u)) => format!("{n} ASD ({u}/sqrt(Hz))"),
            (Some(n), None) => format!("{n} ASD"),
            (None, Some(u)) => format!("{u}/sqrt(Hz)"),
            (None, None) if units.is_empty() => "ASD".to_string(),
            (None, None) => "ASD (mixed units)".to_string(),
        }
    } else {
        match (name, unit) {
            (Some(n), Some(u)) => format!("{n} ({u})"),
            (Some(n), None) => n.to_string(),
            (None, Some(u)) => format!("value ({u})"),
            (None, None) if units.is_empty() => "value".to_string(),
            (None, None) => "value (mixed units)".to_string(),
        }
    }
}

fn nearest_point(points: &[Point], x: f64) -> Option<Point> {
    points
        .iter()
        .filter(|p| p.x.is_finite() && p.y.is_finite())
        .min_by(|a, b| {
            (a.x - x)
                .abs()
                .partial_cmp(&(b.x - x).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
}

fn centered(cr: &cairo::Context, layout: &pango::Layout, th: &Theme, w: f64, h: f64, text: &str) {
    cr.set_source_rgba(th.fg.0, th.fg.1, th.fg.2, 0.6);
    let tw = text_w(layout, text);
    draw_text(cr, layout, (w - tw) / 2.0, h / 2.0, text);
}

/// Snap an already-log10 range out to whole decades (stable log axes).
fn nice_log(lo: f64, hi: f64) -> (f64, f64) {
    let a = lo.floor();
    let mut b = hi.ceil();
    if b <= a {
        b = a + 1.0;
    }
    (a, b)
}

/// Axis-range hysteresis (transformed space): keep `last`, expand to `ideal`
/// immediately, but only contract once the raw extent recedes by `margin`. This
/// is what stops the axes — log axes especially — jumping around frame to frame.
fn apply_hysteresis(
    last: Option<(f64, f64)>,
    ideal: (f64, f64),
    raw: (f64, f64),
    margin: f64,
) -> (f64, f64) {
    let (ilo, ihi) = ideal;
    let (rlo, rhi) = raw;
    let Some((lo, hi)) = last else {
        return ideal;
    };
    if !(ilo < ihi) || !(lo < hi) {
        return ideal;
    }
    let hi2 = if ihi > hi {
        ihi // grew → expand immediately
    } else if rhi.is_finite() && rhi <= hi - margin {
        ihi // receded past the margin → contract
    } else {
        hi // hold
    };
    let lo2 = if ilo < lo {
        ilo
    } else if rlo.is_finite() && rlo >= lo + margin {
        ilo
    } else {
        lo
    };
    if lo2 < hi2 {
        (lo2, hi2)
    } else {
        ideal
    }
}

/// Tick positions (in transformed space) and optional labels for an axis.
fn axis_ticks(min: f64, max: f64, log: bool) -> Vec<(f64, Option<String>)> {
    let mut ticks = Vec::new();
    if !(min.is_finite() && max.is_finite()) || max <= min {
        return ticks;
    }
    if log {
        let lo = min.floor() as i32;
        let hi = max.ceil() as i32;
        for k in lo..=hi {
            let major = k as f64;
            if major >= min && major <= max {
                ticks.push((major, Some(fmt_decade(k))));
            }
            for m in 2..10 {
                let pos = major + (m as f64).log10();
                if pos > min && pos < max {
                    ticks.push((pos, None));
                }
            }
        }
    } else {
        for v in nice_ticks(min, max, 6) {
            ticks.push((v, Some(fmt_tick(v))));
        }
    }
    ticks
}

fn fmt_decade(k: i32) -> String {
    if (-3..=5).contains(&k) {
        fmt_num(10f64.powi(k))
    } else {
        format!("1e{k}")
    }
}

pub(crate) fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    if !(lo.is_finite() && hi.is_finite()) || hi <= lo || target == 0 {
        return Vec::new();
    }
    let raw = (hi - lo) / target as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = if norm < 1.5 {
        1.0
    } else if norm < 3.0 {
        2.0
    } else if norm < 7.0 {
        5.0
    } else {
        10.0
    } * mag;
    let mut ticks = Vec::new();
    let mut v = (lo / step).ceil() * step;
    while v <= hi + step * 0.5 && ticks.len() < 200 {
        ticks.push(v);
        v += step;
    }
    ticks
}

/// If `t` is a plausible Unix-epoch timestamp (seconds since 1970, ~2001 onward
/// — relative/zero-based recordings stay far below this), format it as a local
/// date/time. Returns `None` otherwise, so the caller falls back to the elapsed
/// offset. This distinguishes absolute (Unix) time from relative device time
/// without the core having to tag every frame with its epoch.
fn unix_datetime_label(t: f64) -> Option<String> {
    if !(1.0e9..1.0e11).contains(&t) {
        return None;
    }
    gtk::glib::DateTime::from_unix_local(t as i64)
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M:%S"))
        .ok()
        .map(|s| s.to_string())
}

fn fmt_num(v: f64) -> String {
    let a = v.abs();
    if v == 0.0 {
        "0".to_string()
    } else if a >= 1e5 || a < 1e-3 {
        format!("{v:.2e}")
    } else {
        format!("{v:.3}")
    }
}

/// Tick-label formatter with adaptive precision (mirrors the mac's
/// `plotFormatTick`): more decimals for small magnitudes, fewer for large, so
/// the relative-time axis reads `-10.0`, `-2.00`, `0` rather than `-10.000`.
pub(crate) fn fmt_tick(v: f64) -> String {
    let a = v.abs();
    if v == 0.0 {
        "0".to_string()
    } else if a >= 1e5 || a < 1e-3 {
        format!("{v:.2e}")
    } else if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else if a >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_hysteresis, nice_log, unix_datetime_label};

    #[test]
    fn unix_datetime_label_only_for_absolute_epochs() {
        // Relative / zero-based device time → no date (caller shows the offset).
        assert!(unix_datetime_label(0.0).is_none());
        assert!(unix_datetime_label(42.5).is_none());
        assert!(unix_datetime_label(9.9e8).is_none()); // just before ~2001
                                                       // Implausibly large → no date.
        assert!(unix_datetime_label(1.0e12).is_none());
        // A real Unix timestamp (2026) → a formatted local date/time.
        let label = unix_datetime_label(1.782563e9).expect("a date label");
        assert!(label.contains('-') && label.contains(':'), "got {label:?}");
    }

    #[test]
    fn log_range_snaps_to_decades() {
        assert_eq!(nice_log(-6.8, 0.2), (-7.0, 1.0));
        assert_eq!(nice_log(0.0, 0.0), (0.0, 1.0));
        assert_eq!(nice_log(-3.0, -1.0), (-3.0, -1.0));
    }

    #[test]
    fn hysteresis_expands_immediately_but_contracts_only_past_margin() {
        // No memory yet → take the ideal.
        assert_eq!(
            apply_hysteresis(None, (0.0, 1.0), (0.0, 1.0), 0.2),
            (0.0, 1.0)
        );
        // Data grew → expand right away.
        assert_eq!(
            apply_hysteresis(Some((0.0, 1.0)), (0.0, 2.0), (0.0, 2.0), 0.2),
            (0.0, 2.0)
        );
        // Data dipped only slightly (within the margin) → hold the old bound.
        assert_eq!(
            apply_hysteresis(Some((0.0, 1.0)), (0.0, 0.9), (0.0, 0.95), 0.2),
            (0.0, 1.0)
        );
        // Data receded past the margin → contract.
        assert_eq!(
            apply_hysteresis(Some((0.0, 1.0)), (0.0, 0.5), (0.0, 0.5), 0.2),
            (0.0, 0.5)
        );
    }
}
