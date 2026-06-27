// SPDX-License-Identifier: Apache-2.0
//! Bridges the synchronous, threaded `twinleaf-core` runtime to the GTK main
//! loop. The core sends [`CoreEvent`]s on an `async_channel`; the UI drains the
//! receiver with `glib::spawn_future_local` (see `window::install_event_pump`),
//! so event handling runs on the main thread and can touch widgets directly.

use twinleaf_core::{CoreEvent, TwinleafCore};

/// Spawn the core runtime, returning the handle used to send commands and the
/// receiver the GTK main loop drains for events.
pub fn spawn_core() -> (TwinleafCore, async_channel::Receiver<CoreEvent>) {
    let (tx, rx) = async_channel::unbounded::<CoreEvent>();
    let core = TwinleafCore::spawn(tx);
    (core, rx)
}
