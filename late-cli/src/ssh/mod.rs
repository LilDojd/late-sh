pub mod banner;
pub mod io;
pub mod pty;

use crate::cli::{AddressFamily, Config};
use anyhow::{Context, Result};
use nix::libc;
use nix::pty::openpty;
use nix::unistd::setsid;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use pty::{
    PtyResizeHandle, TIOCSCTTY_IOCTL_REQUEST, nix_to_io_error, pty_winsize,
    terminal_size_or_default,
};

pub const CLI_MODE_ENV: &str = "LATE_CLI_MODE";

pub struct SshProcess {
    pub child: Child,
    pub output_task: JoinHandle<Result<()>>,
    pub input_task: JoinHandle<Result<()>>,
    pub resize_handle: PtyResizeHandle,
    pub input_gate: Arc<AtomicBool>,
}

pub async fn spawn(
    cfg: &Config,
    identity: &Path,
    token_tx: oneshot::Sender<String>,
) -> Result<SshProcess> {
    let (cols, rows) = terminal_size_or_default();
    let winsize = pty_winsize(cols, rows);
    let pty_pair = openpty(Some(&winsize), None).context("failed to allocate local ssh pty")?;
    let master = Arc::new(fs::File::from(pty_pair.master));
    let slave = fs::File::from(pty_pair.slave);
    let slave_fd = slave.as_raw_fd();

    let (program, extra_args) = cfg
        .ssh_bin
        .split_first()
        .context("ssh client command is empty")?;
    let mut cmd = Command::new(program);
    cmd.env(CLI_MODE_ENV, "1")
        .args(extra_args)
        .args(address_family_flags(cfg.address_family))
        .arg("-i")
        .arg(identity)
        .arg("-tt")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(format!("SendEnv={CLI_MODE_ENV}"))
        .arg(&cfg.ssh_target)
        .stdin(Stdio::from(
            slave.try_clone().context("clone ssh pty slave for stdin")?,
        ))
        .stdout(Stdio::from(
            slave
                .try_clone()
                .context("clone ssh pty slave for stdout")?,
        ))
        .stderr(Stdio::from(
            slave
                .try_clone()
                .context("clone ssh pty slave for stderr")?,
        ))
        .kill_on_drop(true);

    unsafe {
        cmd.pre_exec(move || {
            setsid().map_err(nix_to_io_error)?;
            if libc::ioctl(slave_fd, TIOCSCTTY_IOCTL_REQUEST, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("failed to start ssh session")?;
    drop(slave);

    let output_pty = master
        .try_clone()
        .context("clone ssh pty master for output")?;
    let input_pty = master
        .try_clone()
        .context("clone ssh pty master for input")?;
    let input_gate = Arc::new(AtomicBool::new(false));
    let input_gate_for_task = Arc::clone(&input_gate);

    let output_task = tokio::task::spawn_blocking(move || io::forward_output(output_pty, token_tx));
    let input_task =
        tokio::task::spawn_blocking(move || io::forward_stdin(input_pty, input_gate_for_task));

    Ok(SshProcess {
        child,
        output_task,
        input_task,
        resize_handle: PtyResizeHandle { master },
        input_gate,
    })
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
