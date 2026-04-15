use anyhow::Result;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::audio::AudioRuntime;
use crate::ssh::pty::PtyResizeHandle;
use crate::ssh::SshProcess;

#[derive(Debug)]
pub enum Outcome {
    CleanExit,
    SshBeforeHandshake(ExitStatus),
    SshAfterHandshake(ExitStatus),
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
        mut output_task,
        input_task,
        resize_handle,
        input_gate: _,
    } = ssh;

    let mut pair_task = pair_task;
    let outcome = tokio::select! {
        biased;
        status = child.wait() => match status {
            Ok(status) => classify_exit(status, handshake_done.load(Ordering::Relaxed), None),
            Err(err) => return Err(err.into()),
        },
        result = &mut output_task => {
            handle_output_end(result, handshake_done.load(Ordering::Relaxed))
        }
        result = &mut pair_task => handle_pair_end(result),
    };

    teardown(&audio, &mut child, &resize_handle, output_task, input_task, resize_task, pair_task).await;
    Ok(outcome)
}

pub fn classify_exit(status: ExitStatus, handshake_done: bool, stdout_closed_cleanly: Option<bool>) -> Outcome {
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

fn handle_pair_end(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Outcome {
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
    output_task: JoinHandle<Result<()>>,
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

    output_task.abort();
    let _ = output_task.await;
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
