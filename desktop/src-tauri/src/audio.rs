//! Microphone capture -> 16 kHz mono 16-bit little-endian PCM, the format the API expects.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use tokio::sync::{mpsc, oneshot};

pub const TARGET_RATE: u32 = 16_000;

/// Downmixed mono f32 samples in, 16 kHz i16 LE bytes out.
/// Each output sample is the mean of the input samples it covers (a box filter), which also
/// removes most content above 8 kHz before decimating.
/// shortcut: box filter, not a windowed-sinc resampler; fine for speech recognition, upgrade if
/// recognition quality on 44.1 kHz devices turns out worse than on 48 kHz ones.
pub struct Resampler {
    step: f64, // input samples per output sample
    pos: f64,
    sum: f32,
    n: u32,
    last: f32,
}

impl Resampler {
    pub fn new(input_rate: u32) -> Self {
        Resampler {
            step: input_rate as f64 / TARGET_RATE as f64,
            pos: 0.0,
            sum: 0.0,
            n: 0,
            last: 0.0,
        }
    }

    /// `frames` is interleaved audio with `channels` channels.
    pub fn process(&mut self, frames: &[f32], channels: usize, out: &mut Vec<u8>) {
        for frame in frames.chunks_exact(channels.max(1)) {
            self.sum += frame.iter().sum::<f32>() / frame.len() as f32;
            self.n += 1;
            self.pos += 1.0;
            while self.pos >= self.step {
                if self.n > 0 {
                    self.last = self.sum / self.n as f32;
                    self.sum = 0.0;
                    self.n = 0;
                }
                let s = (self.last.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                out.extend_from_slice(&s.to_le_bytes());
                self.pos -= self.step;
            }
        }
    }
}

/// Records on a dedicated thread (a cpal Stream is not Send on every platform) until `stop`
/// fires, then drops the stream, which closes `tx` and so tells the session the audio ended.
/// Resolves once the microphone is open, or with the reason it could not be opened.
pub async fn record(
    tx: mpsc::UnboundedSender<Vec<u8>>,
    stop: std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    let (ready_tx, ready_rx) = oneshot::channel();
    std::thread::spawn(move || match open_stream(tx) {
        Ok(stream) => {
            let _ = ready_tx.send(Ok(()));
            let _ = stop.recv(); // Err too: the session was dropped
            drop(stream);
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
        }
    });
    ready_rx
        .await
        .unwrap_or_else(|_| Err("audio thread exited".into()))
}

fn open_stream(tx: mpsc::UnboundedSender<Vec<u8>>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("No microphone found")?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("Microphone config: {e}"))?;
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, tx),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, tx),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, tx),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, tx),
        other => return Err(format!("Unsupported microphone sample format {other:?}")),
    }?;
    stream
        .play()
        .map_err(|e| format!("Microphone start: {e}"))?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    tx: mpsc::UnboundedSender<Vec<u8>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels() as usize;
    let mut resampler = Resampler::new(config.sample_rate());
    let mut floats = Vec::new();
    device
        .build_input_stream(
            config.config(),
            move |data: &[T], _| {
                floats.clear();
                floats.extend(data.iter().map(|&s| s.to_sample::<f32>()));
                let mut out = Vec::with_capacity(data.len() / channels * 2);
                resampler.process(&floats, channels, &mut out);
                if !out.is_empty() {
                    let _ = tx.send(out); // receiver gone = session over; nothing to do
                }
            },
            |e| log::warn!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| format!("Microphone open: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect()
    }

    #[test]
    fn rates_and_channels() {
        let mut out = Vec::new();
        Resampler::new(48_000).process(&vec![0.5; 48_000 * 2], 2, &mut out); // 1 s stereo
        let s = samples(&out);
        assert_eq!(s.len(), 16_000);
        assert!(s.iter().all(|&x| (x - 16_383).abs() <= 1), "DC level kept");

        let mut out = Vec::new();
        Resampler::new(16_000).process(&[0.25, -0.25, 1.5], 1, &mut out); // passthrough + clamp
        assert_eq!(samples(&out), [8191, -8191, i16::MAX]);

        let mut out = Vec::new();
        Resampler::new(44_100).process(&vec![0.0; 44_100], 1, &mut out);
        assert!((samples(&out).len() as i32 - 16_000).abs() <= 1);

        let mut out = Vec::new();
        Resampler::new(8_000).process(&vec![0.1; 8_000], 1, &mut out); // upsample
        assert_eq!(samples(&out).len(), 16_000);
    }

    #[test]
    fn box_filter_removes_alternating_signal() {
        // 24 kHz tone sampled at 48 kHz: above the 8 kHz output Nyquist, must not alias through.
        let tone: Vec<f32> = (0..4800)
            .map(|i| if i % 2 == 0 { 0.8 } else { -0.8 })
            .collect();
        let mut out = Vec::new();
        Resampler::new(48_000).process(&tone, 1, &mut out);
        assert!(
            samples(&out).iter().all(|&x| x.abs() < 10_000),
            "attenuated"
        );
    }
}
