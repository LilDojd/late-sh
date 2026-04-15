use crate::ssh::banner::{BannerParser, Event};
use anyhow::Result;
use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;

/// Owns the russh channel and multiplexes.
// TODO: refactor later
pub async fn run_io_loop(
    mut channel: Channel<Msg>,
    token_tx: oneshot::Sender<String>,
    exit_tx: oneshot::Sender<Option<u32>>,
) -> Result<()> {
    let mut parser = BannerParser::new();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut buf = vec![0u8; 4096];
    let mut token_tx = Some(token_tx);
    let mut exit_tx = Some(exit_tx);
    let mut handshake_done = false;
    let mut stdin_closed = false;

    let mut sigwinch = match signal(SignalKind::window_change()) {
        Ok(s) => Some(s),
        Err(err) => {
            tracing::debug!(error = ?err, "SIGWINCH handler unavailable");
            None
        }
    };

    loop {
        tokio::select! {
            r = stdin.read(&mut buf), if handshake_done && !stdin_closed => {
                match r {
                    Ok(0) => {
                        stdin_closed = true;
                        if let Err(err) = channel.eof().await {
                            tracing::debug!(error = ?err, "channel.eof() failed");
                        }
                    }
                    Ok(n) => {
                        if let Err(err) = channel.data(&buf[..n]).await {
                            return Err(err.into());
                        }
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            maybe_msg = channel.wait() => {
                let Some(msg) = maybe_msg else { break; };
                match msg {
                    ChannelMsg::Data { ref data } => {
                        for event in parser.feed(data.as_ref()) {
                            match event {
                                Event::Token(token) => {
                                    if let Some(tx) = token_tx.take() {
                                        let _ = tx.send(token);
                                        tracing::debug!("captured cli session token banner");
                                    }
                                    // Flush any pre-handshake keystrokes the kernel
                                    // buffered in the tty input queue before the
                                    // stdin forwarder starts reading.
                                    flush_stdin_input_queue();
                                    handshake_done = true;
                                }
                                Event::Passthrough(bytes) => {
                                    stdout.write_all(&bytes).await?;
                                    stdout.flush().await?;
                                }
                            }
                        }
                    }
                    ChannelMsg::ExtendedData { ref data, .. } => {
                        stderr.write_all(data.as_ref()).await?;
                        stderr.flush().await?;
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        if let Some(tx) = exit_tx.take() {
                            let _ = tx.send(Some(exit_status));
                        }
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
            Some(()) = async { sigwinch.as_mut()?.recv().await }, if sigwinch.is_some() => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                if let Err(err) = channel
                    .window_change(cols as u32, rows as u32, 0, 0)
                    .await
                {
                    tracing::debug!(error = ?err, cols, rows, "window_change failed");
                }
            }
        }
    }

    if let Some(tx) = exit_tx.take() {
        let _ = tx.send(None);
    }
    Ok(())
}

pub fn flush_stdin_input_queue() {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return;
    }
    // SAFETY: tcflush is safe to call on a file descriptor we own (STDIN).
    let rc = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    if rc == -1 {
        tracing::debug!(
            error = ?std::io::Error::last_os_error(),
            "failed to flush pending stdin before enabling ssh input"
        );
    }
}
