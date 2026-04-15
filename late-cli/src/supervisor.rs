use anyhow::Result;
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::audio::AudioRuntime;
use crate::ssh::{self, SshSession};

#[derive(Debug)]
pub enum Outcome {
    CleanExit,
    SshBeforeHandshake(Option<u32>),
    SshAfterHandshake(Option<u32>),
    PairLoopFailed(anyhow::Error),
}

pub async fn run(
    audio: AudioRuntime,
    ssh_session: SshSession,
    pair_task: JoinHandle<Result<()>>,
) -> Result<Outcome> {
    let SshSession {
        handle,
        mut output_task,
        resize_task,
        stdin_task,
        mut exit_rx,
        cmd_tx,
    } = ssh_session;

    let mut pair_task = pair_task;

    let outcome = tokio::select! {
        biased;
        exit = &mut exit_rx => {
            let status = exit.unwrap_or(None);
            classify_exit(status)
        }
        join = &mut output_task => {
            match join {
                Ok(Ok(())) => Outcome::CleanExit,
                Ok(Err(err)) => Outcome::PairLoopFailed(err.context("ssh stdout forwarding failed")),
                Err(err) => Outcome::PairLoopFailed(anyhow::anyhow!("ssh stdout task join failed: {err}")),
            }
        }
        result = &mut pair_task => handle_pair_end(result),
    };

    teardown(
        &audio,
        &handle,
        &cmd_tx,
        output_task,
        stdin_task,
        resize_task,
        pair_task,
    )
    .await;
    Ok(outcome)
}

pub fn classify_exit(status: Option<u32>) -> Outcome {
    match status {
        Some(0) => Outcome::CleanExit,
        // russh closes the channel without an ExitStatus when the remote
        // disconnects cleanly — treat that as a clean exit.
        None => Outcome::CleanExit,
        Some(_) => Outcome::SshAfterHandshake(status),
    }
}

fn handle_pair_end(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Outcome {
    match result {
        Ok(Ok(())) => Outcome::CleanExit,
        Ok(Err(err)) => Outcome::PairLoopFailed(err),
        Err(err) => Outcome::PairLoopFailed(anyhow::anyhow!("pair task join failed: {err}")),
    }
}

async fn teardown(
    audio: &AudioRuntime,
    handle: &russh::client::Handle<ssh::Client>,
    cmd_tx: &tokio::sync::mpsc::Sender<ssh::ChannelCmd>,
    output_task: JoinHandle<Result<()>>,
    stdin_task: Option<JoinHandle<Result<()>>>,
    resize_task: JoinHandle<()>,
    pair_task: JoinHandle<Result<()>>,
) {
    audio.shutdown();
    pair_task.abort();
    if let Some(stdin_task) = stdin_task {
        stdin_task.abort();
    }
    resize_task.abort();

    // Politely tell the output task to close the channel; then ensure the ssh
    // handle drops the underlying transport.
    let _ = cmd_tx.send(ssh::ChannelCmd::Close).await;
    ssh::disconnect(handle).await;

    // Abort the output task before awaiting so that dropping the JoinHandle on
    // timeout doesn't just detach it — we actually want it cancelled.
    output_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(2), output_task).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_after_handshake_is_clean() {
        let o = classify_exit(Some(0));
        assert!(matches!(o, Outcome::CleanExit));
    }

    #[test]
    fn nonzero_is_after_handshake() {
        let o = classify_exit(Some(42));
        assert!(matches!(o, Outcome::SshAfterHandshake(Some(42))));
    }

    #[test]
    fn none_status_is_clean() {
        let o = classify_exit(None);
        assert!(matches!(o, Outcome::CleanExit));
    }
}
