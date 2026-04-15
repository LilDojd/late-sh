use anyhow::Result;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::audio::AudioRuntime;
use crate::ssh::SshProcess;
use crate::ssh::pty::PtyResizeHandle;

#[derive(Debug)]
pub enum Outcome {
    CleanExit,
    SshBeforeHandshake(ExitStatus),
    SshAfterHandshake(ExitStatus),
    #[allow(dead_code)]
    AudioFailed(anyhow::Error),
    PairLoopFailed(anyhow::Error),
}

pub async fn run(
    audio: AudioRuntime,
    ssh: SshProcess,
    resize_task: JoinHandle<()>,
    pair_task: JoinHandle<Result<()>>,
    handshake_done: Arc<AtomicBool>,
) -> Result<Outcome> {
    let SshProcess {
        mut child,
        output_task,
        input_task,
        resize_handle,
        input_gate: _,
    } = ssh;

    let mut pair_task = pair_task;
    // After the select fires, we track whether output_task is already consumed
    // (awaited to completion) so teardown skips re-awaiting it.
    let mut output_task_opt = Some(output_task);
    let outcome = tokio::select! {
        biased;
        status = child.wait() => match status {
            Ok(status) => {
                // If the stdout-forwarding task has already finished, await it here
                // to determine whether it closed cleanly. This matches legacy
                // behavior at legacy.rs:563-567 where ssh exit 255 + clean stdout
                // close is treated as a clean session end.
                let mut output_task = output_task_opt.take().unwrap();
                let stdout_closed_cleanly = if output_task.is_finished() {
                    match (&mut output_task).await {
                        Ok(Ok(())) => Some(true),
                        _ => Some(false),
                    }
                } else {
                    // Output task still running — either the PTY is still open or
                    // the child just died and the forwarder is about to EOF. We do
                    // not want to block here, so put the handle back for teardown
                    // and report that stdout has not been confirmed as cleanly closed.
                    output_task_opt = Some(output_task);
                    Some(false)
                };
                classify_exit(status, handshake_done.load(Ordering::Relaxed), stdout_closed_cleanly)
            }
            Err(err) => return Err(err.into()),
        },
        result = poll_output_task(&mut output_task_opt) => {
            let outcome = handle_output_end(result, handshake_done.load(Ordering::Relaxed));
            output_task_opt = None; // consumed by the select
            outcome
        }
        result = &mut pair_task => handle_pair_end(result),
    };

    teardown(
        &audio,
        &mut child,
        &resize_handle,
        output_task_opt,
        input_task,
        resize_task,
        pair_task,
    )
    .await;
    Ok(outcome)
}

async fn poll_output_task(
    opt: &mut Option<JoinHandle<Result<()>>>,
) -> std::result::Result<Result<()>, tokio::task::JoinError> {
    if let Some(h) = opt {
        h.await
    } else {
        std::future::pending().await
    }
}

pub fn classify_exit(
    status: ExitStatus,
    handshake_done: bool,
    stdout_closed_cleanly: Option<bool>,
) -> Outcome {
    if !handshake_done {
        return Outcome::SshBeforeHandshake(status);
    }

    let stdout_closed = stdout_closed_cleanly.unwrap_or(false);
    if status.success() || (status.code() == Some(255) && stdout_closed) {
        Outcome::CleanExit
    } else {
        Outcome::SshAfterHandshake(status)
    }
}

fn handle_output_end(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    handshake_done: bool,
) -> Outcome {
    match result {
        Ok(Ok(())) if !handshake_done => Outcome::SshBeforeHandshake(fake_exit_status(0)),
        Ok(Ok(())) => Outcome::CleanExit,
        Ok(Err(err)) => Outcome::PairLoopFailed(err.context("ssh stdout forwarding failed")),
        Err(err) => Outcome::PairLoopFailed(anyhow::anyhow!("ssh stdout task join failed: {err}")),
    }
}

fn handle_pair_end(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Outcome {
    match result {
        Ok(Ok(())) => Outcome::CleanExit,
        Ok(Err(err)) => Outcome::PairLoopFailed(err),
        Err(err) => Outcome::PairLoopFailed(anyhow::anyhow!("pair task join failed: {err}")),
    }
}

fn fake_exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw((code & 0xff) << 8)
}

async fn teardown(
    audio: &AudioRuntime,
    child: &mut Child,
    _resize_handle: &PtyResizeHandle,
    output_task: Option<JoinHandle<Result<()>>>,
    input_task: JoinHandle<Result<()>>,
    resize_task: JoinHandle<()>,
    pair_task: JoinHandle<Result<()>>,
) {
    audio.shutdown();
    pair_task.abort();
    input_task.abort();
    resize_task.abort();

    if let Err(err) = child.start_kill() {
        tracing::debug!(error = ?err, "ssh already dead or un-killable");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;

    if let Some(output_task) = output_task {
        output_task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), output_task).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn status(code: i32) -> ExitStatus {
        ExitStatus::from_raw((code & 0xff) << 8)
    }

    #[test]
    fn exit_before_handshake_is_before_handshake() {
        let o = classify_exit(status(1), false, None);
        assert!(matches!(o, Outcome::SshBeforeHandshake(_)));
    }

    #[test]
    fn success_after_handshake_is_clean() {
        let o = classify_exit(status(0), true, None);
        assert!(matches!(o, Outcome::CleanExit));
    }

    #[test]
    fn code_255_after_handshake_with_clean_stdout_is_clean() {
        let o = classify_exit(status(255), true, Some(true));
        assert!(matches!(o, Outcome::CleanExit));
    }

    #[test]
    fn code_255_after_handshake_without_clean_stdout_is_after_handshake() {
        let o = classify_exit(status(255), true, Some(false));
        assert!(matches!(o, Outcome::SshAfterHandshake(_)));
    }

    #[test]
    fn nonzero_after_handshake_is_after_handshake() {
        let o = classify_exit(status(42), true, None);
        assert!(matches!(o, Outcome::SshAfterHandshake(_)));
    }
}
