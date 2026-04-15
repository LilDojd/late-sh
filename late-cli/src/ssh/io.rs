use super::banner::{BannerParser, Event};
use anyhow::Result;
use nix::libc;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

pub fn forward_output(mut pty: fs::File, token_tx: oneshot::Sender<String>) -> Result<()> {
    let mut parser = BannerParser::new();
    let mut buf = [0u8; 4096];
    let mut out = std::io::stdout();
    let mut token_tx = Some(token_tx);

    loop {
        let n = match pty.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        if n == 0 {
            break;
        }

        for event in parser.feed(&buf[..n]) {
            match event {
                Event::Token(token) => {
                    if let Some(tx) = token_tx.take() {
                        let _ = tx.send(token);
                        tracing::debug!("captured cli session token banner");
                    }
                }
                Event::Passthrough(bytes) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
            }
        }
    }

    Ok(())
}

pub fn forward_stdin(mut pty: fs::File, input_gate: Arc<AtomicBool>) -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        if n == 0 {
            break;
        }
        if !input_gate.load(Ordering::Relaxed) {
            continue;
        }
        pty.write_all(&buf[..n])?;
    }
    Ok(())
}

pub fn flush_stdin_input_queue() {
    if !std::io::stdin().is_terminal() {
        return;
    }

    let rc = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    if rc == -1 {
        tracing::debug!(
            error = ?io::Error::last_os_error(),
            "failed to flush pending stdin before enabling ssh input"
        );
    }
}
