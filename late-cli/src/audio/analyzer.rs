use super::output::PlayedRing;
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct AnalyzerConfig {
    pub fft_size: usize,
    pub band_count: usize,
    pub gain: f32,
    pub target_hz: u64,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            fft_size: 1024,
            band_count: 8,
            gain: 3.0,
            target_hz: 15,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VizSample {
    pub bands: [f32; 8],
    pub rms: f32,
}

pub fn spawn(
    played_ring: PlayedRing,
    analyzer_tx: broadcast::Sender<VizSample>,
    sample_rate: u32,
    stop: Arc<AtomicBool>,
) {
    thread::Builder::new()
        .name("late-analyzer".into())
        .spawn(move || run(played_ring, analyzer_tx, sample_rate, stop))
        .expect("spawn late-analyzer thread");
}

fn run(
    played_ring: PlayedRing,
    analyzer_tx: broadcast::Sender<VizSample>,
    sample_rate: u32,
    stop: Arc<AtomicBool>,
) {
    let cfg = AnalyzerConfig::default();
    let bands = log_bands(sample_rate as f32, cfg.fft_size, cfg.band_count);
    let fft = FftPlanner::new().plan_fft_forward(cfg.fft_size);
    let mut scratch = vec![Complex::new(0.0, 0.0); cfg.fft_size];
    let tick = Duration::from_millis(1000 / cfg.target_hz.max(1));

    while !stop.load(Ordering::Relaxed) {
        let frame = {
            let played_ring = played_ring.lock().unwrap_or_else(|e| e.into_inner());
            if played_ring.len() < cfg.fft_size {
                None
            } else {
                let start = played_ring.len() - cfg.fft_size;
                let samples: Vec<f32> = played_ring.iter().skip(start).copied().collect();
                let (mut bands_out, mut rms) = analyze_frame(&samples, &*fft, &mut scratch, &bands);
                normalize_bands(&mut bands_out, &mut rms, cfg.gain);
                Some(VizSample {
                    bands: bands_out,
                    rms,
                })
            }
        };

        if let Some(frame) = frame {
            let _ = analyzer_tx.send(frame);
        }

        thread::sleep(tick);
    }
}

pub fn log_bands(sample_rate: f32, n_fft: usize, band_count: usize) -> Vec<(usize, usize)> {
    let nyquist = sample_rate / 2.0;
    let min_hz: f32 = 60.0;
    let max_hz = nyquist.min(12000.0);
    let log_min = min_hz.ln();
    let log_max = max_hz.ln();

    (0..band_count)
        .map(|i| {
            let t0 = i as f32 / band_count as f32;
            let t1 = (i + 1) as f32 / band_count as f32;
            let f0 = (log_min + (log_max - log_min) * t0).exp();
            let f1 = (log_min + (log_max - log_min) * t1).exp();
            let b0 = ((f0 / nyquist) * (n_fft as f32 / 2.0)).floor().max(1.0) as usize;
            let b1 = ((f1 / nyquist) * (n_fft as f32 / 2.0))
                .ceil()
                .max(b0 as f32 + 1.0) as usize;
            (b0, b1)
        })
        .collect()
}

pub fn analyze_frame(
    samples: &[f32],
    fft: &dyn Fft<f32>,
    scratch: &mut [Complex<f32>],
    bands: &[(usize, usize)],
) -> ([f32; 8], f32) {
    let n = samples.len();
    for (i, s) in samples.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n as f32 - 1.0)).cos();
        scratch[i] = Complex::new(s * w, 0.0);
    }

    fft.process(scratch);

    let mut mags = vec![0.0f32; n / 2];
    for (i, c) in scratch.iter().take(n / 2).enumerate() {
        mags[i] = (c.re * c.re + c.im * c.im).sqrt();
    }

    let mut out = [0.0f32; 8];
    for (bi, (b0, b1)) in bands.iter().enumerate() {
        let start = (*b0).min(mags.len());
        let end = (*b1).min(mags.len());
        let mut sum = 0.0;
        if end > start {
            for m in &mags[start..end] {
                sum += *m;
            }
            out[bi] = sum / ((end - start) as f32);
        }
    }

    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
    (out, rms)
}

pub fn soft_compress(x: f32) -> f32 {
    let k = 2.0;
    (k * x) / (1.0 + k * x)
}

pub fn normalize_bands(bands: &mut [f32], rms: &mut f32, gain: f32) {
    for b in bands.iter_mut() {
        *b = soft_compress(*b * gain).clamp(0.0, 1.0);
    }
    *rms = soft_compress(*rms * gain).clamp(0.0, 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_bands_produce_band_count_entries() {
        let b = log_bands(48_000.0, 1024, 8);
        assert_eq!(b.len(), 8);
        for (lo, hi) in b {
            assert!(lo < hi);
        }
    }

    #[test]
    fn normalize_bands_clamps_output_to_zero_one() {
        let mut bands = [10.0; 8];
        let mut rms = 10.0;
        normalize_bands(&mut bands, &mut rms, 5.0);
        assert!(bands.iter().all(|b| (0.0..=1.0).contains(b)));
        assert!((0.0..=1.0).contains(&rms));
    }
}
