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
        mut io_task,
        mut exit_rx,
    } = ssh_session;

    let mut pair_task = pair_task;

    let outcome = tokio::select! {
        biased;
        exit = &mut exit_rx => {
            let status = exit.unwrap_or(None);
            classify_exit(status)
        }
        join = &mut io_task => {
            match join {
                Ok(Ok(())) => Outcome::CleanExit,
                Ok(Err(err)) => Outcome::PairLoopFailed(err.context("ssh io loop failed")),
                Err(err) => Outcome::PairLoopFailed(anyhow::anyhow!("ssh io task join failed: {err}")),
            }
        }
        result = &mut pair_task => handle_pair_end(result),
    };

    teardown(&audio, &handle, io_task, pair_task).await;
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
    io_task: JoinHandle<Result<()>>,
    pair_task: JoinHandle<Result<()>>,
) {
    audio.shutdown();
    pair_task.abort();

    // Drop the underlying transport so the io task's channel.wait() returns
    // None and the loop exits naturally.
    ssh::disconnect(handle).await;

    // Abort the io task before awaiting so that dropping the JoinHandle on
    // timeout doesn't just detach it — we actually want it cancelled.
    io_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(2), io_task).await;
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
