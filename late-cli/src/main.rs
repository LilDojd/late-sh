mod audio;
mod cli;
mod identity;
mod logging;
mod pair;
mod ssh;
mod supervisor;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::audio::AudioRuntime;
use crate::pair::PairShared;
use crate::ssh::io::flush_stdin_input_queue;
use crate::supervisor::Outcome;

struct RawModeGuard(bool);

impl RawModeGuard {
    fn enable_if_tty() -> Self {
        if !std::io::stdin().is_terminal() {
            return Self(false);
        }
        match enable_raw_mode() {
            Ok(()) => Self(true),
            Err(err) => {
                eprintln!("warning: failed to enable raw mode: {err}");
                Self(false)
            }
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = disable_raw_mode();
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let args = cli::Args::parse();
    let (config, verbosity) = args.resolve()?;
    logging::init(&verbosity)?;
    tracing::debug!(?config, "resolved cli config");

    let _raw_mode = RawModeGuard::enable_if_tty();

    tracing::info!("starting audio runtime");
    let audio = AudioRuntime::start(&config.audio_base_url)
        .await
        .map_err(|err| {
            let hint = audio::audio_startup_hint();
            anyhow::anyhow!("failed to start local audio: {err:#}\n\n{hint}")
        })?;
    tracing::info!(sample_rate = audio.sample_rate, "audio runtime ready");

    tracing::info!("starting ssh session");
    let (token_tx, token_rx) = oneshot::channel();
    let mut ssh_session = ssh::connect(&config, token_tx).await?;

    // Wait for the token OR an early exit from the output task / exit channel.
    let token = {
        let output_task = &mut ssh_session.output_task;
        let exit_rx = &mut ssh_session.exit_rx;
        tokio::select! {
            biased;
            exit = exit_rx => {
                let status = exit.ok().flatten();
                return Ok(report(Outcome::SshBeforeHandshake(status)));
            }
            result = output_task => {
                match result {
                    Ok(Ok(())) => {
                        return Ok(report(Outcome::SshBeforeHandshake(None)));
                    }
                    Ok(Err(err)) => return Err(err.context("ssh stdout forwarding failed before handshake")),
                    Err(err) => return Err(anyhow::anyhow!("ssh stdout task join failed before handshake: {err}")),
                }
            }
            token = tokio::time::timeout(Duration::from_secs(10), token_rx) => {
                token
                    .context("timed out waiting for SSH session token before handshake. Try: late -4")?
                    .context("ssh session token channel closed")?
            }
        }
    };

    // Pre-handshake keystrokes were not being forwarded (stdin task wasn't
    // running), but the kernel has been buffering them in the tty input queue
    // the whole time. Flush that queue before starting the forwarder so that
    // impatient keystrokes during the handshake race don't leak into the
    // remote TUI as spurious input.
    flush_stdin_input_queue();
    ssh_session.spawn_stdin();
    tracing::info!("received session token and starting websocket pairing");

    let shared = PairShared {
        played_samples: Arc::clone(&audio.played_samples),
        sample_rate: audio.sample_rate,
        muted: Arc::clone(&audio.muted),
        volume_percent: Arc::clone(&audio.volume_percent),
    };
    let frames = audio.analyzer_tx.subscribe();
    let api_base_url = config.api_base_url.clone();
    let pair_task = tokio::spawn(pair::run_loop(api_base_url, token, frames, shared));

    let outcome = supervisor::run(audio, ssh_session, pair_task).await?;

    Ok(report(outcome))
}

fn report(outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::CleanExit => ExitCode::from(0),
        Outcome::SshBeforeHandshake(status) => {
            eprintln!(
                "error: ssh exited (status {}) before the session started. Try: late -4",
                format_status(status)
            );
            ExitCode::from(2)
        }
        Outcome::SshAfterHandshake(status) => {
            eprintln!(
                "error: ssh session ended with status {}",
                format_status(status)
            );
            ExitCode::from(1)
        }
        Outcome::PairLoopFailed(err) => {
            eprintln!("error: visualizer pairing failed: {err:#}");
            ExitCode::from(4)
        }
    }
}

fn format_status(status: Option<u32>) -> String {
    match status {
        Some(code) => code.to_string(),
        None => "unknown".to_string(),
    }
}
