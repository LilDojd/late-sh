// Task 1 of the russh migration replaces the subprocess SSH client with
// `russh`. The subprocess-era helpers in `banner`, `io`, and `pty` are kept
// intact so the crate compiles, but `spawn` below no longer drives them.
// Task 2 rewrites or deletes them outright; silence dead-code warnings in
// the meantime rather than gutting each module piecemeal.
#![allow(dead_code)]

pub mod banner;
pub mod io;
pub mod pty;

use crate::cli::{AddressFamily, Config};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::process::Child;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use pty::PtyResizeHandle;

pub const CLI_MODE_ENV: &str = "LATE_CLI_MODE";

pub struct SshProcess {
    pub child: Child,
    pub output_task: JoinHandle<Result<()>>,
    pub input_task: JoinHandle<Result<()>>,
    pub resize_handle: PtyResizeHandle,
    pub input_gate: Arc<AtomicBool>,
}

pub async fn spawn(
    _cfg: &Config,
    _identity: &Path,
    _token_tx: oneshot::Sender<String>,
) -> Result<SshProcess> {
    anyhow::bail!("ssh subsystem being migrated to russh; rebuild after Task 2")
}

pub fn address_family_flags(af: AddressFamily) -> &'static [&'static str] {
    match af {
        AddressFamily::V4 => &["-4"],
        AddressFamily::V6 => &["-6"],
        AddressFamily::Auto => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_family_flags_auto_is_empty() {
        assert!(address_family_flags(AddressFamily::Auto).is_empty());
    }

    #[test]
    fn address_family_flags_v4_passes_minus_4() {
        assert_eq!(address_family_flags(AddressFamily::V4), &["-4"]);
    }

    #[test]
    fn address_family_flags_v6_passes_minus_6() {
        assert_eq!(address_family_flags(AddressFamily::V6), &["-6"]);
    }
}
