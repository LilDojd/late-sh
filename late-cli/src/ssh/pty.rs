use anyhow::{Context, Result};
use nix::libc;
use nix::pty::Winsize;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub const TIOCSCTTY_IOCTL_REQUEST: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
pub const TIOCSCTTY_IOCTL_REQUEST: libc::c_ulong = libc::TIOCSCTTY;

#[derive(Clone)]
pub struct PtyResizeHandle {
    pub master: Arc<fs::File>,
}

impl PtyResizeHandle {
    pub fn resize_to_current(&self) -> Result<()> {
        let (cols, rows) = terminal_size_or_default();
        resize_pty(&self.master, cols, rows)
    }
}

pub fn terminal_size_or_default() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

pub fn pty_winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

pub fn nix_to_io_error(err: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

pub fn resize_pty(master: &fs::File, cols: u16, rows: u16) -> Result<()> {
    let winsize = pty_winsize(cols, rows);
    let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
    if rc == -1 {
        return Err(io::Error::last_os_error()).context("failed to resize local ssh pty");
    }
    tracing::debug!(cols, rows, "resized local ssh pty");
    Ok(())
}

pub async fn forward_resize_events(handle: PtyResizeHandle) {
    let Ok(mut sigwinch) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
    else {
        return;
    };

    while sigwinch.recv().await.is_some() {
        if let Err(err) = handle.resize_to_current() {
            tracing::debug!(error = ?err, "failed to forward local terminal resize");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_winsize_maps_rows_and_cols() {
        let winsize = pty_winsize(120, 40);
        assert_eq!(winsize.ws_col, 120);
        assert_eq!(winsize.ws_row, 40);
    }

    #[test]
    fn terminal_size_default_fallback_is_sane() {
        let (cols, rows) = terminal_size_or_default();
        assert!(cols > 0);
        assert!(rows > 0);
    }
}
