use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::audio::VizSample;

#[derive(Debug, Clone)]
pub struct PairShared {
    pub played_samples: Arc<AtomicU64>,
    pub sample_rate: u32,
    pub muted: Arc<AtomicBool>,
    pub volume_percent: Arc<AtomicU8>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum PairControlMessage {
    ToggleMute,
    VolumeUp,
    VolumeDown,
}

pub async fn run_loop(
    api_base_url: Url,
    token: String,
    mut frames: broadcast::Receiver<VizSample>,
    shared: PairShared,
) -> Result<()> {
    let mut retries = 0usize;
    const MAX_RETRIES: usize = 10;

    loop {
        match run_once(&api_base_url, &token, &mut frames, &shared).await {
            Ok(()) => {
                retries = 0;
                tracing::info!("visualizer websocket closed cleanly; reconnecting in 2s...");
            }
            Err(err) => {
                retries += 1;
                if retries > MAX_RETRIES {
                    tracing::error!(error = ?err, "visualizer websocket failed {MAX_RETRIES} times; giving up");
                    return Err(err);
                }
                tracing::error!(error = ?err, attempt = retries, "visualizer websocket failed; reconnecting in 2s...");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_once(
    api_base_url: &Url,
    token: &str,
    frames: &mut broadcast::Receiver<VizSample>,
    shared: &PairShared,
) -> Result<()> {
    let ws_url = pair_ws_url(api_base_url, token)?;
    tracing::debug!(%ws_url, "connecting pair websocket");
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(ws_url.as_str()))
        .await
        .with_context(|| format!("timed out connecting to pair websocket at {ws_url}"))?
        .with_context(|| format!("failed to connect to pair websocket at {ws_url}"))?;
    tracing::info!("pair websocket established");

    let mut heartbeat = interval(Duration::from_secs(1));
    send_client_state(&mut ws, &shared.muted, &shared.volume_percent).await?;

    loop {
        tokio::select! {
            recv = frames.recv() => {
                let frame = match recv {
                    Ok(frame) => frame,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let position_ms = playback_position_ms(&shared.played_samples, shared.sample_rate);
                let payload = json!({
                    "event": "viz",
                    "position_ms": position_ms,
                    "bands": frame.bands,
                    "rms": frame.rms,
                });
                ws.send(Message::Text(payload.to_string().into())).await?;
            }
            _ = heartbeat.tick() => {
                let payload = json!({
                    "event": "heartbeat",
                    "position_ms": playback_position_ms(&shared.played_samples, shared.sample_rate),
                });
                ws.send(Message::Text(payload.to_string().into())).await?;
            }
            maybe_msg = ws.next() => {
                let Some(msg) = maybe_msg else { break; };
                match msg? {
                    Message::Text(text) => {
                        if apply_pair_control(&text, &shared.muted, &shared.volume_percent)? {
                            send_client_state(&mut ws, &shared.muted, &shared.volume_percent).await?;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

async fn send_client_state(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    muted: &AtomicBool,
    volume_percent: &AtomicU8,
) -> Result<()> {
    let payload = json!({
        "event": "client_state",
        "client_kind": "cli",
        "muted": muted.load(Ordering::Relaxed),
        "volume_percent": volume_percent.load(Ordering::Relaxed),
    });
    ws.send(Message::Text(payload.to_string().into())).await?;
    Ok(())
}

pub fn apply_pair_control(text: &str, muted: &AtomicBool, volume_percent: &AtomicU8) -> Result<bool> {
    match serde_json::from_str::<PairControlMessage>(text)? {
        PairControlMessage::ToggleMute => {
            let now_muted = muted.fetch_xor(true, Ordering::Relaxed) ^ true;
            tracing::info!(muted = now_muted, "applied paired mute toggle");
            Ok(true)
        }
        PairControlMessage::VolumeUp => {
            let new_volume = bump_volume(volume_percent, 5);
            tracing::info!(volume_percent = new_volume, "applied paired volume up");
            Ok(true)
        }
        PairControlMessage::VolumeDown => {
            let new_volume = bump_volume(volume_percent, -5);
            tracing::info!(volume_percent = new_volume, "applied paired volume down");
            Ok(true)
        }
    }
}

pub fn bump_volume(volume_percent: &AtomicU8, delta: i16) -> u8 {
    let current = volume_percent.load(Ordering::Relaxed) as i16;
    let next = (current + delta).clamp(0, 100) as u8;
    volume_percent.store(next, Ordering::Relaxed);
    next
}

pub fn playback_position_ms(played_samples: &AtomicU64, sample_rate: u32) -> u64 {
    played_samples.load(Ordering::Relaxed) * 1000 / sample_rate as u64
}

pub fn pair_ws_url(api_base_url: &Url, token: &str) -> Result<Url> {
    let mut url = api_base_url.clone();
    match url.scheme() {
        "https" => url.set_scheme("wss").map_err(|_| anyhow::anyhow!("failed to set wss scheme"))?,
        "http" => url.set_scheme("ws").map_err(|_| anyhow::anyhow!("failed to set ws scheme"))?,
        "ws" | "wss" => {}
        other => anyhow::bail!("api base url has unsupported scheme '{other}'"),
    }
    url.set_path("/api/ws/pair");
    url.query_pairs_mut().clear().append_pair("token", token);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_ws_url_rewrites_https_to_wss() {
        let url = pair_ws_url(&Url::parse("https://api.late.sh").unwrap(), "abc").unwrap();
        assert_eq!(url.as_str(), "wss://api.late.sh/api/ws/pair?token=abc");
    }

    #[test]
    fn pair_ws_url_rewrites_http_to_ws() {
        let url = pair_ws_url(&Url::parse("http://localhost:4000").unwrap(), "abc").unwrap();
        assert_eq!(url.as_str(), "ws://localhost:4000/api/ws/pair?token=abc");
    }

    #[test]
    fn pair_ws_url_preserves_ws_scheme() {
        let url = pair_ws_url(&Url::parse("ws://localhost").unwrap(), "t").unwrap();
        assert_eq!(url.as_str(), "ws://localhost/api/ws/pair?token=t");
    }

    #[test]
    fn pair_ws_url_rejects_unknown_scheme() {
        let bad = Url::parse("ftp://example.com").unwrap();
        assert!(pair_ws_url(&bad, "x").is_err());
    }

    #[test]
    fn apply_pair_control_toggles_muted_state() {
        let muted = AtomicBool::new(false);
        let volume = AtomicU8::new(100);
        apply_pair_control(r#"{"event":"toggle_mute"}"#, &muted, &volume).unwrap();
        assert!(muted.load(Ordering::Relaxed));
        apply_pair_control(r#"{"event":"toggle_mute"}"#, &muted, &volume).unwrap();
        assert!(!muted.load(Ordering::Relaxed));
    }

    #[test]
    fn apply_pair_control_adjusts_volume() {
        let muted = AtomicBool::new(false);
        let volume = AtomicU8::new(50);
        apply_pair_control(r#"{"event":"volume_up"}"#, &muted, &volume).unwrap();
        assert_eq!(volume.load(Ordering::Relaxed), 55);
        apply_pair_control(r#"{"event":"volume_down"}"#, &muted, &volume).unwrap();
        assert_eq!(volume.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn bump_volume_clamps_to_range() {
        let v = AtomicU8::new(98);
        assert_eq!(bump_volume(&v, 5), 100);
        assert_eq!(bump_volume(&v, -150), 0);
    }

    #[test]
    fn playback_position_ms_uses_sample_rate() {
        let played = AtomicU64::new(48_000);
        assert_eq!(playback_position_ms(&played, 48_000), 1_000);
    }
}
