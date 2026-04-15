use crate::ssh::ChannelCmd;
use crate::ssh::banner::{BannerParser, Event};
use anyhow::Result;
use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// Output forwarding task. Owns the russh channel, services stdin/resize
/// commands from the mpsc, and relays channel data to stdout via the banner
/// parser. The remote exit status is delivered once via `exit_tx`; if the
/// channel closes without one, `None` is sent.
pub async fn forward_output(
    mut channel: Channel<Msg>,
    token_tx: oneshot::Sender<String>,
    mut cmd_rx: mpsc::Receiver<ChannelCmd>,
    exit_tx: oneshot::Sender<Option<u32>>,
) -> Result<()> {
    let mut parser = BannerParser::new();
    let mut stdout = tokio::io::stdout();
    let mut token_tx = Some(token_tx);
    let mut exit_tx = Some(exit_tx);

    loop {
        tokio::select! {
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
                                }
                                Event::Passthrough(bytes) => {
                                    stdout.write_all(&bytes).await?;
                                    stdout.flush().await?;
                                }
                            }
                        }
                    }
                    ChannelMsg::ExtendedData { ref data, .. } => {
                        let mut stderr = tokio::io::stderr();
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
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else { break; };
                match cmd {
                    ChannelCmd::Stdin(bytes) => {
                        if let Err(err) = channel.data(&bytes[..]).await {
                            tracing::debug!(error = ?err, "failed to forward stdin to ssh channel");
                            break;
                        }
                    }
                    ChannelCmd::Resize { cols, rows } => {
                        if let Err(err) = channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await
                        {
                            tracing::debug!(error = ?err, "failed to send window_change");
                        }
                    }
                    ChannelCmd::Eof => {
                        let _ = channel.eof().await;
                    }
                    ChannelCmd::Close => break,
                }
            }
        }
    }

    if let Some(tx) = exit_tx.take() {
        let _ = tx.send(None);
    }
    Ok(())
}

/// Flush any bytes the kernel has buffered on stdin. Called right before the
/// stdin forwarder starts so that keystrokes mashed during the pre-handshake
/// race do not leak into the remote TUI as spurious input.
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

/// Stdin forwarding task. Reads the local tty into 4KiB chunks and relays them
/// as `ChannelCmd::Stdin(..)` messages so the channel stays owned by the
/// output task.
pub async fn forward_stdin(cmd_tx: mpsc::Sender<ChannelCmd>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) => {
                let _ = cmd_tx.send(ChannelCmd::Eof).await;
                break;
            }
            Ok(n) => {
                if cmd_tx
                    .send(ChannelCmd::Stdin(buf[..n].to_vec()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}
