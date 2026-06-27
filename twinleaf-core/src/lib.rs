// SPDX-License-Identifier: Apache-2.0
//! UI-agnostic runtime for the Twinleaf app.
//!
//! This crate is a port of the SwiftUI app's `tio-bridge` Rust core with the
//! C-FFI boundary removed. It owns all device communication (via the
//! `twinleaf` crate), FPCS decimation, Welch FFT, `.tio` packet logging,
//! CSV/HDF5 export, playback/inspection, firmware updates, and stream-health
//! diagnostics. The UI drives it by sending [`CoreCommand`]s and consuming
//! [`CoreEvent`]s on an `async_channel` receiver.
//!
//! ```no_run
//! use twinleaf_core::{CoreCommand, CoreEvent, TwinleafCore};
//! let (tx, rx) = async_channel::unbounded::<CoreEvent>();
//! let core = TwinleafCore::spawn(tx);
//! core.send(CoreCommand::ListDevices { include_all: Some(false) });
//! while let Ok(event) = rx.recv_blocking() {
//!     println!("{event:?}");
//! }
//! ```

mod runtime;

pub use runtime::{
    run_runtime, ColumnKeyDto, CoreCommand, CoreEvent, DecimationMethod, DetrendMethod, EventSink,
    ExportFormat, PlotFrame, PlotMode, PlotPaneConfig, PlotSeriesData, Point, ViewConfig,
};

use crossbeam::channel::{self, Sender};
use std::thread::{self, JoinHandle};

/// Handle to the running core.
///
/// [`spawn`](Self::spawn) starts the `twinleaf-runtime` supervisor thread,
/// which owns the streaming-session and playback threads. Commands are sent
/// over an internal channel; events arrive on the `async_channel` receiver the
/// caller passed to `spawn`. Dropping the handle asks the runtime to shut down
/// and joins its thread.
pub struct TwinleafCore {
    command_tx: Sender<CoreCommand>,
    thread: Option<JoinHandle<()>>,
}

impl TwinleafCore {
    /// Spawn the runtime. `event_tx` is the sending half of an `async_channel`
    /// whose receiver the UI awaits on the main loop.
    pub fn spawn(event_tx: async_channel::Sender<CoreEvent>) -> Self {
        let (command_tx, command_rx) = channel::unbounded::<CoreCommand>();
        let sink = EventSink::new(event_tx);
        let thread = thread::Builder::new()
            .name("twinleaf-runtime".into())
            .spawn(move || run_runtime(command_rx, sink))
            .ok();
        Self { command_tx, thread }
    }

    /// Send a command to the runtime. No-op once the runtime has stopped.
    pub fn send(&self, command: CoreCommand) {
        let _ = self.command_tx.send(command);
    }
}

impl Drop for TwinleafCore {
    fn drop(&mut self) {
        let _ = self.command_tx.send(CoreCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
