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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

use crate::audio::AudioRuntime;
use crate::pair::PairShared;
use crate::ssh::io::flush_stdin_input_queue;
use crate::ssh::pty::forward_resize_events;
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

    let identity_path = identity::ensure()?;
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
    let ssh_process = ssh::spawn(&config, &identity_path, token_tx).await?;
    let resize_task = tokio::spawn(forward_resize_events(ssh_process.resize_handle.clone()));

    // Wait for token OR an early SSH exit, whichever happens first.
    let token = tokio::select! {
        token = tokio::time::timeout(Duration::from_secs(10), token_rx) => {
            token
                .context(
                    "timed out waiting for SSH session token (is the server reachable? \
                     try: ssh late.sh, or rerun with -4)"
                )?
                .context("ssh session token channel closed")?
        }
    };

    flush_stdin_input_queue();
    ssh_process.input_gate.store(true, Ordering::Relaxed);
    let handshake_done = Arc::new(AtomicBool::new(true));
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

    let outcome =
        supervisor::run(audio, ssh_process, resize_task, pair_task, handshake_done).await?;

    Ok(report(outcome))
}

fn report(outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::CleanExit => ExitCode::from(0),
        Outcome::SshBeforeHandshake(status) => {
            eprintln!(
                "error: ssh exited (status {status}) before the session started. Try: late -4"
            );
            ExitCode::from(2)
        }
        Outcome::SshAfterHandshake(status) => {
            eprintln!("error: ssh session ended with status {status}");
            ExitCode::from(1)
        }
        Outcome::PairLoopFailed(err) => {
            eprintln!("error: visualizer pairing failed: {err:#}");
            ExitCode::from(4)
        }
    }
}
