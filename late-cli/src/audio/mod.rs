pub mod analyzer;
pub mod decoder;
pub mod output;

pub use analyzer::VizSample;
pub use decoder::AudioSpec;

use anyhow::{Context, Result};
use cpal::traits::StreamTrait;
use std::collections::VecDeque;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::broadcast;
use url::Url;

use analyzer as analyzer_mod;
use decoder::{trim_stream_suffix, StreamDecoder, StreamingLinearResampler};
use output::{build_output_stream, output_sample_rate_for, BuiltOutputStream};

pub struct AudioRuntime {
    _stream: cpal::Stream,
    pub analyzer_tx: broadcast::Sender<VizSample>,
    pub played_samples: Arc<AtomicU64>,
    pub sample_rate: u32,
    pub stop: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
    pub volume_percent: Arc<AtomicU8>,
}

impl AudioRuntime {
    pub async fn start(audio_base_url: &Url) -> Result<Self> {
        let base_url_string = audio_base_url.as_str().to_string();
        let probe_url = base_url_string.clone();

        let source_spec = tokio::task::spawn_blocking(move || probe_stream_spec(&probe_url))
            .await
            .context("audio stream probe task failed")??;

        let _ = output_sample_rate_for(source_spec)?; // smoke check; real rate read from built stream
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(4096)));
        let played_ring = Arc::new(Mutex::new(VecDeque::with_capacity(4096)));
        let played_samples = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let volume_percent = Arc::new(AtomicU8::new(30));
        let (analyzer_tx, _) = broadcast::channel(32);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let BuiltOutputStream { stream, sample_rate } = build_output_stream(
            source_spec,
            Arc::clone(&queue),
            Arc::clone(&played_ring),
            Arc::clone(&played_samples),
            Arc::clone(&muted),
            Arc::clone(&volume_percent),
        )?;

        spawn_decoder_thread(
            base_url_string,
            Arc::clone(&queue),
            source_spec,
            sample_rate,
            Arc::clone(&stop),
            ready_tx,
        );

        analyzer_mod::spawn(
            Arc::clone(&played_ring),
            analyzer_tx.clone(),
            sample_rate,
            Arc::clone(&stop),
        );

        ready_rx
            .recv()
            .context("failed to receive decoder startup status")??;
        stream.play().context("failed to start audio output stream")?;

        Ok(Self {
            _stream: stream,
            analyzer_tx,
            played_samples,
            sample_rate,
            stop,
            muted,
            volume_percent,
        })
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn probe_stream_spec(audio_base_url: &str) -> Result<AudioSpec> {
    let decoder = StreamDecoder::new_http(&trim_stream_suffix(audio_base_url))
        .context("failed to create audio decoder for stream probe")?;
    Ok(decoder.spec())
}

fn spawn_decoder_thread(
    audio_base_url: String,
    queue: output::PlaybackQueue,
    source_spec: AudioSpec,
    output_sample_rate: u32,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<()>>,
) {
    thread::Builder::new()
        .name("late-decoder".into())
        .spawn(move || decoder_loop(audio_base_url, queue, source_spec, output_sample_rate, stop, ready_tx))
        .expect("spawn late-decoder thread");
}

fn decoder_loop(
    audio_base_url: String,
    queue: output::PlaybackQueue,
    source_spec: AudioSpec,
    output_sample_rate: u32,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<()>>,
) {
    let mut decoder_opt =
        match StreamDecoder::new_http(&trim_stream_suffix(&audio_base_url)) {
            Ok(decoder) => {
                let _ = ready_tx.send(Ok(()));
                Some(decoder)
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err.context("failed to create audio decoder")));
                return;
            }
        };

    let max_buffer_samples = output_sample_rate as usize * source_spec.channels * 2;
    let mut chunk = Vec::with_capacity(1024 * source_spec.channels);
    let mut resampler = StreamingLinearResampler::new(
        source_spec.channels,
        source_spec.sample_rate,
        output_sample_rate,
    );
    let mut retries = 0;
    const MAX_RETRIES: usize = 10;

    while !stop.load(Ordering::Relaxed) {
        chunk.clear();

        if let Some(decoder) = &mut decoder_opt {
            for _ in 0..(1024 * source_spec.channels) {
                match decoder.next() {
                    Some(sample) => chunk.push(sample),
                    None => {
                        decoder_opt = None;
                        break;
                    }
                }
            }
        }

        if chunk.is_empty() {
            if decoder_opt.is_none() {
                retries += 1;
                if retries > MAX_RETRIES {
                    tracing::error!(
                        "audio stream failed {} times consecutively; giving up",
                        MAX_RETRIES
                    );
                    break;
                }
                tracing::warn!(attempt = retries, "audio stream ended or errored, reconnecting in 2s...");
                thread::sleep(Duration::from_secs(2));

                match StreamDecoder::new_http(&trim_stream_suffix(&audio_base_url)) {
                    Ok(new_decoder) => {
                        tracing::info!("audio stream reconnected");
                        decoder_opt = Some(new_decoder);
                        retries = 0;
                    }
                    Err(err) => {
                        tracing::error!(error = ?err, "failed to reconnect audio stream");
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
            continue;
        }

        let chunk = resampler.process(&chunk);
        if chunk.is_empty() {
            continue;
        }

        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            let mut queue_guard = queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue_guard.len() + chunk.len() <= max_buffer_samples {
                queue_guard.extend(chunk.iter().copied());
                break;
            }
            drop(queue_guard);
            thread::sleep(Duration::from_millis(5));
        }
    }
}

pub fn audio_startup_hint() -> String {
    if is_wsl() {
        if missing_wsl_audio_env() {
            return "WSL was detected, but no Linux audio bridge appears configured.\n\
                    Checked env: DISPLAY, WAYLAND_DISPLAY, PULSE_SERVER.\n\
                    To enable audio:\n\
                    - On WSLg, update WSL/Windows and verify audio works in another Linux app\n\
                    - Otherwise run a PulseAudio server on Windows and set PULSE_SERVER\n\
                    - Then rerun `late`"
                .to_string();
        }

        return "WSL was detected and audio startup still failed.\n\
                Verify audio works in another Linux app first, then rerun `late`.\n\
                If you use a Windows PulseAudio server, confirm `PULSE_SERVER` points to it."
            .to_string();
    }

    "Check that this machine has a usable default audio output device and that \
     another app can play sound, then rerun `late`."
        .to_string()
}

pub fn is_wsl() -> bool {
    env::var_os("WSL_DISTRO_NAME").is_some() || env::var_os("WSL_INTEROP").is_some()
}

pub fn missing_wsl_audio_env() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY", "PULSE_SERVER"]
        .into_iter()
        .all(env_var_missing_or_blank)
}

pub fn env_var_missing_or_blank(key: &str) -> bool {
    env::var(key).map_or(true, |value| value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_missing_or_blank_treats_missing_and_blank_as_missing() {
        let key = "LATE_TEST_AUDIO_HINT_ENV";

        unsafe { env::remove_var(key) };
        assert!(env_var_missing_or_blank(key));

        unsafe { env::set_var(key, "   ") };
        assert!(env_var_missing_or_blank(key));

        unsafe { env::set_var(key, "set") };
        assert!(!env_var_missing_or_blank(key));

        unsafe { env::remove_var(key) };
    }
}
