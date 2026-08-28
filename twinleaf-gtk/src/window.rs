// SPDX-License-Identifier: Apache-2.0
//! Main window assembly and the core-event pump. Layout: stream sidebar |
//! (stacked plot panes over a status bar) | RPC sidebar. Up to 5 plot panes
//! stack vertically; channels are added to a pane by ticking them (pane 0) or
//! dragging a channel row onto a pane. Plot frames are routed to the pane
//! canvas matching `frame.pane_id`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use twinleaf_core::{ColumnKeyDto, CoreEvent, ExportFormat, PlotMode};

use crate::app::AppState;
use crate::bridge;
use crate::controls::MainControls;
use crate::model;
use crate::plot::PlotCanvas;
use crate::sidebar::Sidebar;

/// Shared list of pane canvases, indexed by pane id (== position).
type Canvases = Rc<RefCell<Vec<Rc<PlotCanvas>>>>;

/// Create a pane canvas with its channel drop target, per-graph settings
/// popover (right-click), and scroll-to-zoom, then append it. Each pane reads
/// and writes its own [`twinleaf_core::ViewConfig`].
/// Clamp a panned viewport-end to the inspected log's data range so the window
/// can't scroll past where samples exist. Returns the value unchanged if no
/// range is known.
fn clamp_viewport(state: &Rc<AppState>, viewport_end: f64, window: f64) -> f64 {
    match state.data.borrow().playback_range {
        Some((start, end)) if end > start => clamp_to_range(viewport_end, window, start, end),
        _ => viewport_end,
    }
}

/// Keep a panned window within `[start, end]`: its left edge stays at or after
/// `start`, its right edge at or before `end`. If the log is shorter than the
/// window, pin to `end` (show the whole thing).
fn clamp_to_range(viewport_end: f64, window: f64, start: f64, end: f64) -> f64 {
    let lo = (start + window.max(0.0)).min(end);
    viewport_end.clamp(lo, end)
}

/// Keep the graph stack and controls tray aligned: when there are no plot panes,
/// the empty stack should not consume the vertical expansion the sliders need.
fn update_graph_layout(canvases: &Canvases, panes_box: &gtk::Box, controls: &Rc<MainControls>) {
    let has_plot_panes = !canvases.borrow().is_empty();
    panes_box.set_visible(has_plot_panes);
    panes_box.set_vexpand(has_plot_panes);
    controls.set_has_plot_panes(has_plot_panes);
}

/// Rebuild every pane canvas from `state.panes`. Used after removing a pane so
/// the per-pane indices captured in each canvas's gesture closures stay correct.
fn rebuild_panes(
    state: &Rc<AppState>,
    canvases: &Canvases,
    panes_box: &gtk::Box,
    controls: &Rc<MainControls>,
) {
    while let Some(child) = panes_box.first_child() {
        panes_box.remove(&child);
    }
    canvases.borrow_mut().clear();
    let n = state.data.borrow().panes.len();
    for _ in 0..n {
        append_pane(state, canvases, panes_box, controls);
    }
    update_graph_layout(canvases, panes_box, controls);
}

/// Ensure a canvas exists for every pane in `state.panes`, appending any that are
/// missing — e.g. when the first channel is checked after all graphs were closed,
/// a pane reappears in state and its plot frames need somewhere to draw.
fn sync_canvases(
    state: &Rc<AppState>,
    canvases: &Canvases,
    panes_box: &gtk::Box,
    controls: &Rc<MainControls>,
) {
    let target = state.data.borrow().panes.len();
    while canvases.borrow().len() < target {
        append_pane(state, canvases, panes_box, controls);
    }
    update_graph_layout(canvases, panes_box, controls);
}

/// Close the graph at `idx` and rebuild so the survivors reindex. Zero graphs is
/// allowed (the main view can then show only captures/sliders).
fn remove_one_pane(
    state: &Rc<AppState>,
    canvases: &Canvases,
    panes_box: &gtk::Box,
    controls: &Rc<MainControls>,
    idx: usize,
) {
    if state.remove_pane(idx) {
        rebuild_panes(state, canvases, panes_box, controls);
    }
}

fn append_pane(
    state: &Rc<AppState>,
    canvases: &Canvases,
    panes_box: &gtk::Box,
    controls: &Rc<MainControls>,
) {
    let idx = canvases.borrow().len();
    let canvas = Rc::new(PlotCanvas::new());
    let view = state.pane_view(idx);
    canvas.set_labels(model::column_labels(&state.data.borrow().devices));
    canvas.set_window_seconds(view.window_seconds);
    canvas.set_fft_log(view.fft_log_x, view.fft_log_y);

    // Drag a channel row from the sidebar onto the pane to add it.
    let drop = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::COPY);
    {
        let state = state.clone();
        drop.connect_drop(move |_, value, _, _| {
            if let Ok(text) = value.get::<String>() {
                if let Ok(key) = serde_json::from_str::<ColumnKeyDto>(&text) {
                    state.add_column_to_pane(idx, key);
                    return true;
                }
            }
            false
        });
    }
    canvas.widget.add_controller(drop);

    // Right-click menu: this graph's settings (mode / window / FFT log axes),
    // then copy and remove.
    let menu = gtk::Popover::new();
    menu.set_parent(&canvas.widget);
    menu.set_has_arrow(false);
    menu.set_halign(gtk::Align::Start);
    let mbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    mbox.set_margin_top(8);
    mbox.set_margin_bottom(8);
    mbox.set_margin_start(8);
    mbox.set_margin_end(8);

    let heading = gtk::Label::new(Some("Graph settings"));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");
    mbox.append(&heading);

    let mode_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let mode_label = gtk::Label::new(Some("Mode"));
    mode_label.set_xalign(0.0);
    mode_label.set_hexpand(true);
    let mode_dd = gtk::DropDown::from_strings(&["Timeseries", "FFT"]);
    mode_dd.set_selected(if view.mode == PlotMode::Fft { 1 } else { 0 });
    {
        let state = state.clone();
        let menu = menu.clone();
        mode_dd.connect_selected_notify(move |dd| {
            let mode = if dd.selected() == 1 {
                PlotMode::Fft
            } else {
                PlotMode::Timeseries
            };
            state.set_pane_mode(idx, mode);
            // Close the settings popover so the switch feels like a committed action.
            menu.popdown();
        });
    }
    mode_row.append(&mode_label);
    mode_row.append(&mode_dd);
    mbox.append(&mode_row);

    let win_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let win_label = gtk::Label::new(Some("Window (s)"));
    win_label.set_xalign(0.0);
    win_label.set_hexpand(true);
    let win_spin = gtk::SpinButton::with_range(1.0, 1000.0, 1.0);
    win_spin.set_value(view.window_seconds);
    {
        let state = state.clone();
        let canvas = canvas.clone();
        win_spin.connect_value_changed(move |sp| {
            state.set_pane_window(idx, sp.value());
            canvas.set_window_seconds(sp.value());
        });
    }
    win_row.append(&win_label);
    win_row.append(&win_spin);
    mbox.append(&win_row);

    let log_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let logx = gtk::CheckButton::with_label("log f");
    logx.set_active(view.fft_log_x);
    let logy = gtk::CheckButton::with_label("log A");
    logy.set_active(view.fft_log_y);
    {
        let state = state.clone();
        let canvas = canvas.clone();
        let logy2 = logy.clone();
        logx.connect_toggled(move |b| {
            state.set_pane_fft_log(idx, b.is_active(), logy2.is_active());
            canvas.set_fft_log(b.is_active(), logy2.is_active());
        });
    }
    {
        let state = state.clone();
        let canvas = canvas.clone();
        let logx2 = logx.clone();
        logy.connect_toggled(move |b| {
            state.set_pane_fft_log(idx, logx2.is_active(), b.is_active());
            canvas.set_fft_log(logx2.is_active(), b.is_active());
        });
    }
    log_row.append(&logx);
    log_row.append(&logy);
    mbox.append(&log_row);

    mbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let copy_item = gtk::Button::with_label("Copy data");
    copy_item.add_css_class("flat");
    {
        let state = state.clone();
        let menu = menu.clone();
        copy_item.connect_clicked(move |_| {
            state.copy_view_data_pane(idx);
            menu.popdown();
        });
    }
    mbox.append(&copy_item);

    let remove_item = gtk::Button::with_label("Remove graph");
    remove_item.add_css_class("flat");
    {
        let state = state.clone();
        let canvases = canvases.clone();
        let panes_box = panes_box.clone();
        let controls = controls.clone();
        let menu = menu.clone();
        remove_item.connect_clicked(move |_| {
            menu.popdown();
            remove_one_pane(&state, &canvases, &panes_box, &controls, idx);
        });
    }
    mbox.append(&remove_item);

    menu.set_child(Some(&mbox));
    let secondary = gtk::GestureClick::new();
    secondary.set_button(3); // GDK secondary (right) button
    {
        let menu = menu.clone();
        secondary.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            menu.popup();
        });
    }
    canvas.widget.add_controller(secondary);

    // Two-finger horizontal scroll scrubs the playback position — opened files
    // only (no panning of live data), clamped by the core to the data's time
    // range. Vertical scroll does nothing (zoom is the pinch / window control).
    {
        let state = state.clone();
        let canvas_scroll = canvas.clone();
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.connect_scroll(move |_, dx, dy| {
            if dx.abs() > dy.abs() && state.data.borrow().inspection {
                if let Some(end) = canvas_scroll.view_end() {
                    let window = state.pane_view(idx).window_seconds;
                    let next = clamp_viewport(&state, end + dx * 0.05 * window, window);
                    state.set_playback(next);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        canvas.widget.add_controller(scroll);
    }

    // Pinch (trackpad) to zoom the time window, anchored at the gesture's start.
    {
        let zoom = gtk::GestureZoom::new();
        let begin = Rc::new(Cell::new(0.0_f64));
        {
            let state = state.clone();
            let begin = begin.clone();
            zoom.connect_begin(move |_, _| begin.set(state.pane_view(idx).window_seconds));
        }
        {
            let state = state.clone();
            let canvas = canvas.clone();
            zoom.connect_scale_changed(move |_, scale| {
                if scale > 0.0 {
                    // Pinch out (scale > 1) → smaller window → zoom in.
                    let next = (begin.get() / scale).clamp(1.0, 1000.0);
                    state.set_pane_window(idx, next);
                    canvas.set_window_seconds(next);
                }
            });
        }
        canvas.widget.add_controller(zoom);
    }

    // Drag also scrubs the playback position (opened files only).
    {
        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new(None::<f64>));
        {
            let canvas = canvas.clone();
            let start = start.clone();
            drag.connect_drag_begin(move |_, _, _| start.set(canvas.view_end()));
        }
        {
            let state = state.clone();
            let canvas = canvas.clone();
            drag.connect_drag_update(move |_, dx, _| {
                if !state.data.borrow().inspection {
                    return; // pan disabled for live data, for now
                }
                if let Some(begin_end) = start.get() {
                    let w = canvas.widget.width().max(1) as f64;
                    let window = state.pane_view(idx).window_seconds;
                    let next = clamp_viewport(&state, begin_end - dx / w * window, window);
                    state.set_playback(next);
                }
            });
        }
        canvas.widget.add_controller(drag);
    }

    // Double-click to resume the live (latest) view after panning.
    {
        let state = state.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, n_press, _, _| {
            if n_press >= 2 {
                state.set_pane_viewport(idx, None);
            }
        });
        canvas.widget.add_controller(click);
    }

    canvas.widget.set_vexpand(true);

    // Wrap the plot in an overlay so a close (✕) button can sit in the upper-right
    // corner; clicking it removes this graph. (Stock icon names render blank with
    // no system icon theme, so use a text glyph.)
    let overlay = gtk::Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.set_child(Some(&canvas.widget));
    let close = gtk::Button::with_label("✕");
    close.add_css_class("flat");
    close.add_css_class("circular");
    close.set_halign(gtk::Align::End);
    close.set_valign(gtk::Align::Start);
    close.set_margin_top(4);
    close.set_margin_end(4);
    close.set_tooltip_text(Some("Close graph"));
    {
        let state = state.clone();
        let canvases = canvases.clone();
        let panes_box = panes_box.clone();
        let controls = controls.clone();
        close.connect_clicked(move |_| {
            remove_one_pane(&state, &canvases, &panes_box, &controls, idx);
        });
    }
    overlay.add_overlay(&close);
    panes_box.append(&overlay);
    canvases.borrow_mut().push(canvas);
    update_graph_layout(canvases, panes_box, controls);
}

pub fn build_main_window(app: &adw::Application) {
    // Make the bundled symbolic icons resolvable by name (no system icon theme
    // is required).
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_resource_path("/com/twinleaf/AppGtk/icons");
        // Tint the firmware arrow green and the record dot red while active.
        let css = gtk::CssProvider::new();
        css.load_from_data(".tl-update { color: #2ec27e; } .tl-record:checked { color: #e01b24; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        if std::env::var_os("TWINLEAF_SELFTEST_ICONS").is_some() {
            let theme = gtk::IconTheme::for_display(&display);
            for name in [
                "tl-add-symbolic",
                "tl-pause-symbolic",
                "tl-record-symbolic",
                "tl-connect-symbolic",
                "tl-settings-symbolic",
                "tl-log-symbolic",
                "tl-menu-symbolic",
                "tl-update-symbolic",
                "tl-health-symbolic",
                "tl-sidebar-symbolic",
                "tl-unify-symbolic",
            ] {
                eprintln!("[selftest] icon {name}: {}", theme.has_icon(name));
            }
        }
    }

    let (core, rx) = bridge::spawn_core();
    let state = Rc::new(AppState::new(core));
    let prefs = crate::persistence::Preferences::load();
    state.data.borrow_mut().panes[0].view.window_seconds = prefs.window_seconds;

    // The main-view tray hosts promoted sliders and capture graphs; the sidebar
    // spawns them into it (Capture button / setting-name context menu).
    let main_controls = MainControls::new(&state);
    let sidebar = Sidebar::new(&state, &main_controls);
    let status = gtk::Label::new(Some("Starting…"));
    status.set_xalign(0.0);
    status.set_hexpand(true);
    let log_label = gtk::Label::new(None);
    log_label.set_xalign(1.0);
    let health = crate::popovers::HealthIndicator::new();

    let canvases: Canvases = Rc::new(RefCell::new(Vec::new()));
    let panes_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    panes_box.set_vexpand(true);
    append_pane(&state, &canvases, &panes_box, &main_controls);
    // No global plot toolbar: per-graph settings (mode / window / FFT log axes)
    // and scroll-to-zoom live in each graph's right-click menu; pause / add /
    // remove live on the header bar.

    // Playback scrubber (shown in inspection mode, driven by `playback` events).
    let scrubber = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    scrubber.set_hexpand(true);
    scrubber.set_visible(false);
    let scrubber_suppress = Rc::new(Cell::new(false));
    {
        let state = state.clone();
        let suppress = scrubber_suppress.clone();
        scrubber.connect_value_changed(move |s| {
            if !suppress.get() {
                state.set_playback(s.value());
            }
        });
    }

    // Device-log panel at the bottom of the plot area, revealed by the header
    // "log" button. (RPC settings now live in the left sidebar.)
    let log_view = gtk::TextView::new();
    log_view.set_editable(false);
    log_view.set_cursor_visible(false);
    log_view.set_monospace(true);
    log_view.set_wrap_mode(gtk::WrapMode::WordChar);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_child(Some(&log_view));
    log_scroll.set_vexpand(true);
    let log_inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    log_inner.set_size_request(300, -1);
    let log_heading = gtk::Label::new(Some("Device log"));
    log_heading.set_xalign(0.0);
    log_heading.add_css_class("dim-label");
    log_heading.set_margin_start(8);
    log_heading.set_margin_top(6);
    log_heading.set_margin_bottom(3);
    log_inner.append(&log_heading);
    log_inner.append(&log_scroll);
    // A right-docked utility pane (vertical rule separates it from the plot).
    let log_panel = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    log_panel.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    log_panel.append(&log_inner);
    let log_revealer = gtk::Revealer::new();
    log_revealer.set_child(Some(&log_panel));
    log_revealer.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
    log_revealer.set_reveal_child(false);

    let plot_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    plot_box.set_hexpand(true);
    plot_box.append(&panes_box);
    plot_box.append(&main_controls.widget);
    plot_box.append(&scrubber);

    // Plot fills the width; the device-log utility pane slides in from the right.
    let content_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content_row.append(&plot_box);
    content_row.append(&log_revealer);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar.widget));
    paned.set_end_child(Some(&content_row));
    paned.set_position(prefs.sidebar_width);
    paned.set_shrink_start_child(false);
    paned.set_resize_start_child(false);
    paned.set_vexpand(true);

    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_bar.set_margin_start(8);
    status_bar.set_margin_end(8);
    status_bar.set_margin_top(3);
    status_bar.set_margin_bottom(3);
    status_bar.append(&status);
    status_bar.append(&log_label);

    // Bottom status bar — off by default, toggled in Preferences.
    let status_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
    status_area.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    status_area.append(&status_bar);
    status_area.set_visible(prefs.show_status_bar);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&paned);
    root.append(&status_area);

    // Follow the system light/dark scheme, honoring the saved theme override.
    let style = adw::StyleManager::default();
    style.set_color_scheme(match prefs.theme.as_str() {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(prefs.window_width)
        .default_height(prefs.window_height)
        .build();
    window.set_title(Some("Twinleaf"));
    let header = adw::HeaderBar::new();

    // ---- Left cluster: connect · add-graph · pause · record · log ----
    let device_popover = crate::popovers::DevicePopover::new(&state);

    let add_graph_btn = gtk::Button::from_icon_name("tl-add-symbolic");
    add_graph_btn.set_tooltip_text(Some("Add a graph"));
    {
        let state = state.clone();
        let canvases = canvases.clone();
        let panes_box = panes_box.clone();
        let controls = main_controls.clone();
        add_graph_btn.connect_clicked(move |_| {
            if state.add_pane().is_some() {
                append_pane(&state, &canvases, &panes_box, &controls);
            }
        });
    }

    let pause_btn = gtk::ToggleButton::new();
    pause_btn.set_icon_name("tl-pause-symbolic");
    pause_btn.set_tooltip_text(Some("Pause the live display"));
    {
        let state = state.clone();
        pause_btn.connect_toggled(move |b| state.set_paused(b.is_active()));
    }

    let record_btn = gtk::ToggleButton::new();
    record_btn.set_icon_name("tl-record-symbolic");
    record_btn.add_css_class("tl-record");
    record_btn.set_tooltip_text(Some("Record the session to a .tio file"));
    {
        let state = state.clone();
        record_btn.connect_toggled(move |b| state.set_logging(b.is_active()));
    }

    let log_btn = gtk::ToggleButton::new();
    log_btn.set_icon_name("tl-log-symbolic");
    log_btn.set_tooltip_text(Some("Show the device log"));
    {
        let revealer = log_revealer.clone();
        log_btn.connect_toggled(move |b| revealer.set_reveal_child(b.is_active()));
    }

    // The sidebar toggle is the one leading control; all other buttons are
    // right-aligned (packed below).
    let sidebar_btn = gtk::ToggleButton::new();
    sidebar_btn.set_icon_name("tl-sidebar-symbolic");
    sidebar_btn.set_tooltip_text(Some("Show or hide the sidebar"));
    sidebar_btn.set_active(true);
    {
        let sidebar_widget = sidebar.widget.clone();
        sidebar_btn.connect_toggled(move |b| sidebar_widget.set_visible(b.is_active()));
    }
    header.pack_start(&sidebar_btn);
    header.pack_start(sidebar.unify_button());

    // ---- Right cluster: menu · settings · firmware-update arrow ----
    let settings_btn = gtk::Button::from_icon_name("tl-settings-symbolic");
    settings_btn.set_tooltip_text(Some("Preferences"));
    {
        let win = window.clone();
        let style = style.clone();
        let status_area = status_area.clone();
        settings_btn.connect_clicked(move |_| {
            crate::prefs_ui::show_preferences(&win, &style, &status_area)
        });
    }

    // Open… / Export… live in a hamburger menu (GTK4 cannot drive the native
    // macOS menu bar, so this is used on every platform).
    let actions = gio::SimpleActionGroup::new();
    let open_action = gio::SimpleAction::new("open", None);
    {
        let state = state.clone();
        let win = window.clone();
        open_action.connect_activate(move |_, _| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Open .tio log");
            let state = state.clone();
            dialog.open(Some(&win), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        state.open_log(path.to_string_lossy().into_owned());
                    }
                }
            });
        });
    }
    actions.add_action(&open_action);
    let export_action = gio::SimpleAction::new("export", None);
    {
        let state = state.clone();
        let win = window.clone();
        export_action.connect_activate(move |_, _| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Export CSV");
            dialog.set_initial_name(Some("twinleaf.csv"));
            let state = state.clone();
            dialog.save(Some(&win), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        state.export_log(path.to_string_lossy().into_owned(), ExportFormat::Csv);
                    }
                }
            });
        });
    }
    actions.add_action(&export_action);

    // "Save As…": choose the recording's file path (and move an in-progress
    // recording there, continuing seamlessly).
    let save_as_action = gio::SimpleAction::new("save-as", None);
    {
        let state = state.clone();
        let win = window.clone();
        save_as_action.connect_activate(move |_, _| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Save log as");
            dialog.set_initial_name(Some("recording.tio"));
            let state = state.clone();
            dialog.save(Some(&win), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        state.save_log_as(path);
                    }
                }
            });
        });
    }
    actions.add_action(&save_as_action);

    // Cmd-, / Ctrl-, opens Preferences.
    let prefs_action = gio::SimpleAction::new("preferences", None);
    {
        let win = window.clone();
        let style = style.clone();
        let status_area = status_area.clone();
        prefs_action.connect_activate(move |_, _| {
            crate::prefs_ui::show_preferences(&win, &style, &status_area)
        });
    }
    actions.add_action(&prefs_action);

    // Cmd-R / Ctrl-R toggles recording (drives the header toggle so it stays in sync).
    let record_action = gio::SimpleAction::new("record", None);
    {
        let record_btn = record_btn.clone();
        record_action.connect_activate(move |_, _| record_btn.set_active(!record_btn.is_active()));
    }
    actions.add_action(&record_action);

    window.insert_action_group("win", Some(&actions));

    let menu = gio::Menu::new();
    menu.append(Some("Open…"), Some("win.open"));
    menu.append(Some("Save As…"), Some("win.save-as"));
    menu.append(Some("Export…"), Some("win.export"));
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("tl-menu-symbolic");
    menu_btn.set_tooltip_text(Some("Menu"));
    menu_btn.set_menu_model(Some(&menu));

    let firmware = crate::popovers::FirmwarePopover::new(&state);

    // Right-aligned cluster. pack_end stacks right-to-left, so pack in the
    // reverse of the desired left-to-right order:
    // connect · add · pause · record · log · health · settings · menu · update.
    header.pack_end(&firmware.button);
    header.pack_end(&menu_btn);
    header.pack_end(&settings_btn);
    header.pack_end(&health.button);
    header.pack_end(&log_btn);
    header.pack_end(&record_btn);
    header.pack_end(&pause_btn);
    header.pack_end(&add_graph_btn);
    header.pack_end(&device_popover.button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&root));
    window.set_content(Some(&toolbar_view));

    install_event_pump(
        EventCtx {
            state: state.clone(),
            sidebar: sidebar.clone(),
            canvases: canvases.clone(),
            panes_box: panes_box.clone(),
            device_popover: device_popover.clone(),
            log_view: log_view.clone(),
            status,
            log_label,
            health,
            firmware,
            scrubber,
            scrubber_suppress,
            controls: main_controls.clone(),
            record_btn: record_btn.clone(),
        },
        rx,
    );

    // Persist geometry on close, and offer to save an in-progress recording.
    {
        let paned = paned.clone();
        let state = state.clone();
        let saving_done = Rc::new(Cell::new(false));
        window.connect_close_request(move |win| {
            let mut prefs = crate::persistence::Preferences::load();
            prefs.window_width = win.width().max(200);
            prefs.window_height = win.height().max(150);
            prefs.sidebar_width = paned.position();
            prefs.window_seconds = state
                .data
                .borrow()
                .panes
                .first()
                .map(|p| p.view.window_seconds)
                .unwrap_or(10.0);
            prefs.save();

            // Re-entry after the save dialog handled the recording: just close.
            if saving_done.get() {
                return glib::Propagation::Proceed;
            }
            // Don't block headless auto-close (self-tests) with a dialog.
            if std::env::var_os("TWINLEAF_SELFTEST_QUIT").is_some() {
                return glib::Propagation::Proceed;
            }
            // If this session recorded a .tio (to the temp dir), prompt for where
            // to save it before closing.
            let recording = state
                .data
                .borrow()
                .recording_path
                .clone()
                .filter(|p| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false));
            let Some(temp_path) = recording else {
                return glib::Propagation::Proceed;
            };
            // Stop logging so the core flushes and closes the file before we copy.
            state.set_logging(false);

            let dialog = gtk::FileDialog::new();
            dialog.set_title("Save recording");
            dialog.set_initial_name(Some("recording.tio"));
            let win2 = win.clone();
            let state2 = state.clone();
            let saving_done2 = saving_done.clone();
            dialog.save(Some(win), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(dest) = file.path() {
                        if let Err(err) = std::fs::copy(&temp_path, &dest) {
                            eprintln!("[twinleaf] could not save recording: {err}");
                        }
                        state2.data.borrow_mut().recording_path = None;
                    }
                }
                // Saved or cancelled, allow the close now (on cancel the recording
                // remains in the temp dir).
                saving_done2.set(true);
                win2.close();
            });
            glib::Propagation::Stop
        });
    }

    state.refresh_devices();
    window.present();

    if std::env::var_os("TWINLEAF_SELFTEST_MULTIPANE").is_some() && state.add_pane().is_some() {
        append_pane(&state, &canvases, &panes_box, &main_controls);
        eprintln!("[selftest] added pane 1");
    }
    if let Ok(url) = std::env::var("TWINLEAF_SELFTEST_CONNECT") {
        eprintln!("[selftest] auto-connecting to {url}");
        state.connect(url, None);
    }
    if let Ok(path) = std::env::var("TWINLEAF_SELFTEST_OPEN") {
        eprintln!("[selftest] opening log {path}");
        state.open_log(path);
    }
    if std::env::var_os("TWINLEAF_SELFTEST_PREFS").is_some() {
        let win = window.clone();
        let style = style.clone();
        let status_area = status_area.clone();
        glib::timeout_add_seconds_local_once(2, move || {
            eprintln!("[selftest] opening preferences");
            crate::prefs_ui::show_preferences(&win, &style, &status_area);
        });
    }
    if let Ok(secs) = std::env::var("TWINLEAF_SELFTEST_QUIT") {
        if let Ok(n) = secs.parse::<u32>() {
            let window = window.clone();
            glib::timeout_add_seconds_local_once(n, move || window.close());
        }
    }
}

/// Widgets the event pump updates. Bundled to keep the pump signature sane.
struct EventCtx {
    state: Rc<AppState>,
    sidebar: Rc<Sidebar>,
    canvases: Canvases,
    panes_box: gtk::Box,
    device_popover: Rc<crate::popovers::DevicePopover>,
    log_view: gtk::TextView,
    status: gtk::Label,
    log_label: gtk::Label,
    health: Rc<crate::popovers::HealthIndicator>,
    firmware: Rc<crate::popovers::FirmwarePopover>,
    scrubber: gtk::Scale,
    scrubber_suppress: Rc<Cell<bool>>,
    controls: Rc<crate::controls::MainControls>,
    record_btn: gtk::ToggleButton,
}

fn install_event_pump(ctx: EventCtx, rx: async_channel::Receiver<CoreEvent>) {
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            handle_event(&ctx, event);
        }
    });
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1 << 10;
    const MB: u64 = 1 << 20;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn handle_event(ctx: &EventCtx, event: CoreEvent) {
    let EventCtx {
        state,
        sidebar,
        canvases,
        panes_box,
        device_popover,
        log_view,
        status,
        log_label,
        health,
        firmware,
        scrubber,
        scrubber_suppress,
        controls,
        record_btn,
    } = ctx;
    match event {
        CoreEvent::Status {
            state: phase,
            message,
        } => {
            status.set_text(&format!("{phase} — {message}"));
            // A fresh connection restarts the live plot clock (mirrors the mac's
            // resetLivePlotTiming), so the "+ N s" offset counts from this session.
            if phase == "connecting" {
                for canvas in canvases.borrow().iter() {
                    canvas.reset_origin();
                }
                controls.clear();
            }
            let connected = phase == "streaming";
            if connected {
                state.data.borrow_mut().connected = true;
                // Auto-start recording once per connection if preferred. The
                // "streaming" status re-fires when recording stops, so the
                // `auto_logged` guard keeps it from immediately restarting.
                if !state.data.borrow().auto_logged
                    && (crate::persistence::Preferences::load().start_logging_on_connect
                        || std::env::var_os("TWINLEAF_SELFTEST_AUTOLOG").is_some())
                {
                    state.data.borrow_mut().auto_logged = true;
                    if !record_btn.is_active() {
                        record_btn.set_active(true);
                    }
                }
            }
            // The connect button stays primary (green/accent) until streaming,
            // then reverts to a normal toolbar button.
            if connected {
                device_popover.button.remove_css_class("suggested-action");
            } else {
                device_popover.button.add_css_class("suggested-action");
            }
        }
        CoreEvent::Error(message) => status.set_text(&format!("Error: {message}")),
        CoreEvent::Debug(_) => {}
        CoreEvent::Plot(frame) => {
            if std::env::var_os("TWINLEAF_SELFTEST_CONNECT").is_some() {
                let points: usize = frame.series.iter().map(|s| s.points.len()).sum();
                eprintln!(
                    "[selftest] plot frame pane={} mode={:?} series={} points={}",
                    frame.pane_id,
                    frame.mode,
                    frame.series.len(),
                    points
                );
            }
            if !state.data.borrow().paused {
                // A pane can reappear (e.g. a channel checked after all graphs
                // were closed); make sure its canvas exists before drawing.
                if frame.pane_id >= canvases.borrow().len() {
                    sync_canvases(state, canvases, panes_box, controls);
                }
                if let Some(canvas) = canvases.borrow().get(frame.pane_id) {
                    canvas.set_frame(frame);
                }
            }
        }
        CoreEvent::Json(value) => {
            match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "deviceList" => {
                    let devices = model::device_list(&value);
                    device_popover.set_available(&devices);
                    state.data.borrow_mut().available = devices;
                }
                "metadata" => {
                    let devices = model::metadata_devices(&value);
                    let labels = model::column_labels(&devices);
                    for canvas in canvases.borrow().iter() {
                        canvas.set_labels(labels.clone());
                    }
                    sidebar.set_metadata(&devices);
                    // Self-test: promote the first capture and the first
                    // slider-suitable setting into the main-view controls tray, so
                    // the build/draw/trigger paths run headless.
                    if std::env::var_os("TWINLEAF_SELFTEST_CONTROLS").is_some() {
                        for d in &devices {
                            if let Some(r) =
                                d.rpcs.iter().find(|r| r.is_visible() && r.is_capture())
                            {
                                eprintln!("[selftest] add_capture {}", r.name);
                                controls.add_capture(
                                    r,
                                    &[(r.route.clone(), r.name.clone())],
                                    &r.name,
                                );
                            }
                            if let Some(r) = d.rpcs.iter().find(|r| {
                                r.is_visible()
                                    && r.is_numeric()
                                    && r.readable
                                    && r.writable
                                    && r.has_metadata()
                                    && !r.is_bool()
                            }) {
                                eprintln!("[selftest] add_slider {}", r.name);
                                controls.add_slider(
                                    r,
                                    &[(r.route.clone(), r.name.clone())],
                                    &r.name,
                                );
                            }
                        }
                        // Drive the capture auto-retrigger loop so it runs headless.
                        controls.selftest_enable_auto();
                    }
                    // Self-test: close every graph (exercises remove + rebuild and
                    // the zero-graphs state).
                    if std::env::var_os("TWINLEAF_SELFTEST_CLOSE_PANES").is_some() {
                        let n = state.data.borrow().panes.len();
                        eprintln!("[selftest] closing {n} graph(s)");
                        for _ in 0..n {
                            remove_one_pane(state, canvases, panes_box, controls, 0);
                        }
                        eprintln!("[selftest] graphs now {}", state.data.borrow().panes.len());
                    }
                    // Self-test: relocate the recording to a path after a few
                    // seconds (exercises Save As → move + continue).
                    if let Ok(dest) = std::env::var("TWINLEAF_SELFTEST_SAVEAS") {
                        let state = state.clone();
                        glib::timeout_add_seconds_local_once(3, move || {
                            eprintln!("[selftest] save_log_as {dest}");
                            state.save_log_as(std::path::PathBuf::from(dest));
                        });
                    }
                    // The core sends RPC descriptors without current values, so
                    // prime each readable RPC with a value read (mirrors the
                    // SwiftUI app's loadReadableRPCs); replies populate the rows.
                    let inspection = state.data.borrow().inspection;
                    if !inspection {
                        for rpc in model::readable_rpcs(&devices) {
                            state.call_rpc(rpc.route, rpc.name, None);
                        }
                    }
                    if std::env::var_os("TWINLEAF_SELFTEST_RPC").is_some() {
                        if let Some(r) = model::visible_rpcs(&devices)
                            .into_iter()
                            .find(|r| r.readable)
                        {
                            eprintln!("[selftest] reading RPC {}", r.name);
                            state.call_rpc(r.route, r.name, None);
                        }
                    }
                    // On a fresh connect or opened file, display a default channel:
                    // the first column of the lowest-id stream that has data.
                    let no_columns = state
                        .data
                        .borrow()
                        .panes
                        .iter()
                        .all(|p| p.columns.is_empty());
                    if no_columns {
                        let defaults = model::default_display_columns(
                            &devices,
                            crate::persistence::Preferences::load().skip_comm_hub_default_plot,
                        );
                        if !defaults.is_empty() {
                            state.toggle_columns(defaults, true);
                        }
                        // Self-test extras, driven off that default selection.
                        if std::env::var_os("TWINLEAF_SELFTEST_CONNECT").is_some() {
                            if std::env::var_os("TWINLEAF_SELFTEST_MULTIPANE").is_some() {
                                let second = devices
                                    .iter()
                                    .flat_map(|d| &d.streams)
                                    .flat_map(|s| &s.columns)
                                    .map(|c| c.key.clone())
                                    .nth(1);
                                if let Some(k1) = second {
                                    eprintln!("[selftest] assigning {k1:?} to pane 1");
                                    state.add_column_to_pane(1, k1);
                                }
                            }
                            if std::env::var_os("TWINLEAF_SELFTEST_FFT").is_some() {
                                eprintln!("[selftest] switching to FFT mode");
                                state.set_pane_mode(0, PlotMode::Fft);
                            }
                            if std::env::var_os("TWINLEAF_SELFTEST_LOG").is_some() {
                                eprintln!("[selftest] enabling logging");
                                state.set_logging(true);
                            }
                            if std::env::var_os("TWINLEAF_SELFTEST_UPGRADE").is_some() {
                                eprintln!("[selftest] checking firmware");
                                state.check_upgrade();
                            }
                        }
                    }
                    // Auto-check for a firmware update once per connection; the
                    // header arrow only appears if one is available.
                    if !state.data.borrow().upgrade_checked {
                        state.data.borrow_mut().upgrade_checked = true;
                        state.check_upgrade();
                    }
                    state.data.borrow_mut().devices = devices;
                }
                "streamValues" => {
                    let values = model::stream_values(&value);
                    sidebar.update_values(&values);
                    let mut data = state.data.borrow_mut();
                    for sv in &values {
                        data.column_values.insert(sv.key.clone(), sv.value);
                    }
                }
                "logProgress" => {
                    let bytes = value.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0);
                    let packets = value.get("packets").and_then(|p| p.as_u64()).unwrap_or(0);
                    if state.data.borrow().logging {
                        log_label.set_text(&format!("● {} · {packets} pkts", human_bytes(bytes)));
                    }
                    if std::env::var_os("TWINLEAF_SELFTEST_LOG").is_some() {
                        eprintln!("[selftest] logProgress: {bytes} bytes, {packets} packets");
                    }
                }
                "rpcResult" => {
                    let ok = value.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                    let route = value.get("route").and_then(|r| r.as_str()).unwrap_or("");
                    let name = value.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    if std::env::var_os("TWINLEAF_SELFTEST_RPC").is_some() {
                        eprintln!(
                            "[selftest] rpcResult ok={ok} {route}/{name} value={:?}",
                            value.get("value")
                        );
                    }
                    if ok {
                        if let Some(v) = value.get("value") {
                            if !route.is_empty() && !name.is_empty() {
                                sidebar.update_rpc_value(route, name, v);
                                controls.update_rpc_value(route, name, v);
                            }
                        }
                    } else if let Some(err) = value.get("error").and_then(|e| e.as_str()) {
                        status.set_text(&format!("RPC error: {err}"));
                    }
                }
                "rpcInvalidated" => {}
                "exportResult" => {
                    if value.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        let out = value
                            .get("outputPath")
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        status.set_text(&format!("Exported to {out}"));
                    } else {
                        let err = value
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("unknown error");
                        status.set_text(&format!("Export failed: {err}"));
                    }
                }
                "viewData" => {
                    if value.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
                            if let Some(display) = gdk::Display::default() {
                                display.clipboard().set_text(text);
                                status.set_text("Copied plotted data to clipboard");
                            }
                        }
                    }
                }
                "playback" => {
                    let start = value.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let end = value.get("end").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    let position = value
                        .get("position")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(start);
                    scrubber_suppress.set(true);
                    if end > start {
                        scrubber.set_range(start, end);
                        state.data.borrow_mut().playback_range = Some((start, end));
                    }
                    scrubber.set_value(position);
                    scrubber_suppress.set(false);
                    scrubber.set_visible(true);
                }
                "health" => {
                    health.set_health(&value);
                    if std::env::var_os("TWINLEAF_SELFTEST_CONNECT").is_some() {
                        if let Some(s) = value
                            .get("streams")
                            .and_then(|s| s.as_array())
                            .and_then(|a| a.first())
                        {
                            let rate = s.get("rateHz").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let jitter = s.get("jitterMs").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            eprintln!("[selftest] health {rate:.1} Hz, jitter {jitter:.2} ms");
                        }
                    }
                }
                "logMessage" => {
                    let msg = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    if !msg.is_empty() {
                        let route = value.get("route").and_then(|v| v.as_str()).unwrap_or("/");
                        let buffer = log_view.buffer();
                        let mut end = buffer.end_iter();
                        buffer.insert(&mut end, &format!("{route}  {msg}\n"));
                        // Cap the scrollback so long sessions don't grow forever.
                        let lines = buffer.line_count();
                        if lines > 2000 {
                            let mut start = buffer.start_iter();
                            if let Some(mut cut) = buffer.iter_at_line(lines - 2000) {
                                buffer.delete(&mut start, &mut cut);
                            }
                        }
                        let mut end = buffer.end_iter();
                        log_view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
                    }
                }
                "upgradeStatus" => firmware.set_status(&value),
                "upgradeProgress" => firmware.set_progress(&value),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clamp_to_range;

    #[test]
    fn pan_stays_within_the_logged_range() {
        // Window fits inside the log: a mid-range target is untouched.
        assert_eq!(clamp_to_range(50.0, 10.0, 0.0, 100.0), 50.0);
        // Can't pan the right edge past the end of the data.
        assert_eq!(clamp_to_range(150.0, 10.0, 0.0, 100.0), 100.0);
        // Can't pan the left edge before the start (floored at start + window).
        assert_eq!(clamp_to_range(2.0, 10.0, 0.0, 100.0), 10.0);
        // Log shorter than the window: pin to the end and show the whole thing.
        assert_eq!(clamp_to_range(5.0, 50.0, 0.0, 20.0), 20.0);
    }
}
