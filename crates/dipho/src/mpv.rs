//! mpv JSON IPC client. dipho never renders video itself: mpv runs as an
//! external slave player started with `--input-ipc-server=<socket>`, and
//! this module drives it (loadfile, seek, ab-loop, frame-step) over the
//! socket. Stub — milestone: audition.

#![allow(dead_code)] // scaffold stub — remove once wired into the TUI (milestone: audition)

use std::path::{Path, PathBuf};

/// Client for a slave mpv process over its JSON IPC socket.
pub struct MpvClient {
    socket_path: PathBuf,
}

impl MpvClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}
