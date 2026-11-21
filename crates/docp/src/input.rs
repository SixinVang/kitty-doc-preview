//! Single tty reader for the interactive views.
//!
//! Terminal capability probes must not race a second reader: the reply to
//! `CSI 16t` is a terminal input sequence and would otherwise be consumed by
//! the key-event reader. This thread owns all reads and serves cell-size
//! probes on request, which lets a view re-sync after a pane resize.

use crate::caps;
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::event::{self, Event};
use std::time::Duration;

pub struct Input {
    pub events: Receiver<Event>,
    probes: Sender<()>,
    cells: Receiver<Option<(u16, u16)>>,
}

impl Input {
    pub fn spawn() -> std::io::Result<Input> {
        let (events_tx, events) = unbounded();
        let (probes, probe_rx) = unbounded::<()>();
        let (cells_tx, cells) = unbounded();
        std::thread::Builder::new()
            .name("input".into())
            .spawn(move || {
                loop {
                    if probe_rx.try_recv().is_ok() {
                        if cells_tx.send(caps::probe_cell(200)).is_err() {
                            return;
                        }
                        continue;
                    }
                    // Poll instead of blocking in read() so a probe request is
                    // serviced even when the user is not typing.
                    match event::poll(Duration::from_millis(25)) {
                        Ok(true) => match event::read() {
                            Ok(ev) => {
                                if events_tx.send(ev).is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        },
                        Ok(false) => {}
                        Err(_) => return,
                    }
                }
            })?;
        Ok(Input {
            events,
            probes,
            cells,
        })
    }

    /// Ask the reader thread to measure the cell. Blocks up to `timeout`.
    pub fn cell(&self, timeout: Duration) -> Option<(u16, u16)> {
        let _ = self.probes.send(());
        self.cells.recv_timeout(timeout).ok().flatten()
    }
}
