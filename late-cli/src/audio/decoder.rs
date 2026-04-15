use anyhow::{Context, Result};
use std::io::{Cursor, Read};
use symphonia::core::{
    audio::{AudioBufferRef, SampleBuffer},
    codecs::{Decoder, DecoderOptions},
    formats::{FormatOptions, FormatReader},
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::Hint,
};
use symphonia::default::{get_codecs, get_probe};

#[derive(Debug, Clone, Copy)]
pub struct AudioSpec {
    pub sample_rate: u32,
    pub channels: usize,
}

pub struct StreamDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_buf: Vec<f32>,
    sample_pos: usize,
    spec: AudioSpec,
}

struct PrefixThenRead<R> {
    prefix: Cursor<Vec<u8>>,
    inner: R,
}

impl<R> PrefixThenRead<R> {
    fn new(prefix: Vec<u8>, inner: R) -> Self {
        Self {
            prefix: Cursor::new(prefix),
            inner,
        }
    }
}

impl<R: Read> Read for PrefixThenRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.prefix.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

impl StreamDecoder {
    pub fn new_http(url: &str) -> Result<Self> {
        let stream_url = format!("{url}/stream");
        let mut resp = reqwest::blocking::get(&stream_url)
            .context("http get")?
            .error_for_status()
            .with_context(|| format!("stream request failed for {stream_url}"))?;
        let prefix = read_until_mp3_sync(&mut resp)
            .with_context(|| format!("failed to align MP3 stream from {stream_url}"))?;
        let source = ReadOnlySource::new(PrefixThenRead::new(prefix, resp));

        let mss = MediaSourceStream::new(Box::new(source), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("mp3");

        let probed = get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;

        let format = probed.format;
        let (track_id, spec, decoder) = {
            let track = format.default_track().context("no default track")?;
            let sample_rate = track.codec_params.sample_rate.context("no sample rate")?;
            let channels = track
                .codec_params
                .channels
                .context("no channel layout")?
                .count();
            let decoder = get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
            (
                track.id,
                AudioSpec { sample_rate, channels },
                decoder,
            )
        };

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_buf: Vec::new(),
            sample_pos: 0,
            spec,
        })
    }

    pub fn spec(&self) -> AudioSpec {
        self.spec
    }

    fn refill(&mut self) -> Result<bool> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(symphonia::core::errors::Error::IoError(_)) => return Ok(false),
                Err(err) => return Err(err.into()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = self.decoder.decode(&packet)?;
            self.sample_buf.clear();
            self.sample_pos = 0;
            push_interleaved_samples(&mut self.sample_buf, decoded)?;
            return Ok(true);
        }
    }
}

impl Iterator for StreamDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.sample_pos >= self.sample_buf.len() {
            match self.refill() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(err) => {
                    tracing::warn!(error = ?err, "decoder refill error, treating as eof");
                    return None;
                }
            }
        }

        let sample = self.sample_buf.get(self.sample_pos).copied();
        self.sample_pos += 1;
        sample
    }
}

pub fn read_until_mp3_sync<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    const MAX_SCAN_BYTES: usize = 64 * 1024;
    const CHUNK_SIZE: usize = 4096;

    let mut buf = Vec::with_capacity(CHUNK_SIZE * 2);
    let mut chunk = [0u8; CHUNK_SIZE];

    while buf.len() < MAX_SCAN_BYTES {
        let read = reader
            .read(&mut chunk)
            .context("failed to read from audio stream")?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);

        if let Some(offset) = find_mp3_sync_offset(&buf) {
            return Ok(buf.split_off(offset));
        }
    }

    anyhow::bail!("could not find MP3 frame sync in first {} bytes", buf.len())
}

pub fn find_mp3_sync_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(b"ID3") {
        return Some(0);
    }

    for i in 0..=bytes.len().saturating_sub(3) {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        let b2 = bytes[i + 2];

        if b0 != 0xFF || (b1 & 0xE0) != 0xE0 {
            continue;
        }

        let version = (b1 >> 3) & 0x03;
        let layer = (b1 >> 1) & 0x03;
        let bitrate_idx = (b2 >> 4) & 0x0F;
        let sample_rate_idx = (b2 >> 2) & 0x03;

        if version == 0x01 || layer == 0x00 {
            continue;
        }
        if bitrate_idx == 0x00 || bitrate_idx == 0x0F {
            continue;
        }
        if sample_rate_idx == 0x03 {
            continue;
        }

        return Some(i);
    }

    None
}

pub fn push_interleaved_samples(out: &mut Vec<f32>, decoded: AudioBufferRef<'_>) -> Result<()> {
    let spec = *decoded.spec();
    let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
    buf.copy_interleaved_ref(decoded);
    out.extend_from_slice(buf.samples());
    Ok(())
}

pub fn trim_stream_suffix(audio_base_url: &str) -> String {
    audio_base_url
        .trim_end_matches('/')
        .trim_end_matches("/stream")
        .to_string()
}

pub struct StreamingLinearResampler {
    channels: usize,
    source_rate: u32,
    target_rate: u32,
    position: f64,
    previous_frame: Option<Vec<f32>>,
}

impl StreamingLinearResampler {
    pub fn new(channels: usize, source_rate: u32, target_rate: u32) -> Self {
        Self {
            channels,
            source_rate,
            target_rate,
            position: 0.0,
            previous_frame: None,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.channels == 0 || input.is_empty() || !input.len().is_multiple_of(self.channels) {
            return Vec::new();
        }

        if self.source_rate == self.target_rate {
            self.previous_frame = Some(input[input.len() - self.channels..input.len()].to_vec());
            return input.to_vec();
        }

        let input_frames = input.len() / self.channels;
        let combined_frames = input_frames + usize::from(self.previous_frame.is_some());
        if combined_frames < 2 {
            self.previous_frame = Some(input.to_vec());
            return Vec::new();
        }

        let step = self.source_rate as f64 / self.target_rate as f64;
        let available_intervals = (combined_frames - 1) as f64;
        let mut output = Vec::new();

        while self.position < available_intervals {
            let left_idx = self.position.floor() as usize;
            let right_idx = left_idx + 1;
            let frac = (self.position - left_idx as f64) as f32;
            for channel in 0..self.channels {
                let left = self.frame_sample(input, left_idx, channel);
                let right = self.frame_sample(input, right_idx, channel);
                output.push(left + (right - left) * frac);
            }
            self.position += step;
        }

        self.position -= available_intervals;
        self.previous_frame = Some(input[input.len() - self.channels..input.len()].to_vec());
        output
    }

    fn frame_sample(&self, input: &[f32], frame_idx: usize, channel: usize) -> f32 {
        if let Some(prev) = &self.previous_frame {
            if frame_idx == 0 {
                return prev[channel];
            }
            return input[(frame_idx - 1) * self.channels + channel];
        }
        input[frame_idx * self.channels + channel]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_mp3_sync_offset_finds_frame_after_garbage() {
        let mut bytes = vec![0x12, 0x34, 0x56, 0x78];
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64, 0x00, 0x00]);
        assert_eq!(find_mp3_sync_offset(&bytes), Some(4));
    }

    #[test]
    fn find_mp3_sync_offset_accepts_id3_header() {
        assert_eq!(find_mp3_sync_offset(b"ID3\x04\x00\x00"), Some(0));
    }

    #[test]
    fn find_mp3_sync_offset_checks_last_possible_offset() {
        let bytes = [0x00, 0xFF, 0xFB, 0x90];
        assert_eq!(find_mp3_sync_offset(&bytes), Some(1));
    }

    #[test]
    fn trim_stream_suffix_normalizes_base_url() {
        assert_eq!(trim_stream_suffix("http://audio.late.sh/stream"), "http://audio.late.sh");
        assert_eq!(trim_stream_suffix("http://audio.late.sh/"), "http://audio.late.sh");
    }

    #[test]
    fn resampler_passthrough_preserves_native_rate_frames() {
        let mut r = StreamingLinearResampler::new(2, 44_100, 44_100);
        let input = vec![0.1, -0.1, 0.25, -0.25];
        assert_eq!(r.process(&input), input);
    }

    #[test]
    fn resampler_outputs_audio_when_upsampling() {
        let mut r = StreamingLinearResampler::new(1, 44_100, 48_000);
        let input = vec![0.0, 1.0, 0.0, -1.0];
        let output = r.process(&input);
        assert!(output.len() >= input.len());
        assert!(output.iter().all(|s| (-1.0..=1.0).contains(s)));
    }
}
