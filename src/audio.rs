//! Audio plumbing shared by host and viewer.
//!
//! Internally everything is mono f32 at 48 kHz. Capture streams downmix and resample into that;
//! playback streams resample out of it and copy to every output channel. A small jitter buffer
//! between the network and playback smooths out uneven packet arrival.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const RATE: u32 = 48_000;
/// Playback waits until this much audio is buffered (60 ms) before starting.
const TARGET_FILL: usize = RATE as usize * 60 / 1000;
/// If the buffer grows past this (200 ms), it's trimmed back to the target to keep latency low.
const MAX_FILL: usize = RATE as usize * 200 / 1000;

/// Converts a mono stream from one sample rate to another by linear interpolation.
/// Fine for voice; keeps state between calls so chunk boundaries don't click.
pub struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, last: 0.0 }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        // `pos` is measured from `last` (index -1) into `input`.
        let len = input.len() as f64;
        while self.pos < len {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 1.0 { self.last } else { input[i as usize - 1] };
            let b = input[i as usize];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= len;
        if let Some(&last) = input.last() {
            self.last = last;
        }
    }
}

/// A thread-safe queue of mono samples between the network and an audio device.
#[derive(Clone, Default)]
pub struct Buffer {
    inner: Arc<Mutex<BufferState>>,
    muted: Arc<AtomicBool>,
    /// The loudest recent sample, stored as f32 bits, for level meters and "speaking" rings.
    peak: Arc<AtomicU32>,
}

#[derive(Default)]
struct BufferState {
    samples: VecDeque<f32>,
    playing: bool,
}

impl Buffer {
    /// Adds incoming audio, trimming the oldest samples if the buffer has grown too long.
    pub fn push(&self, samples: &[f32]) {
        if self.muted.load(Ordering::Relaxed) {
            return;
        }
        let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        let current = f32::from_bits(self.peak.load(Ordering::Relaxed));
        self.peak.store(current.max(peak).to_bits(), Ordering::Relaxed);
        let mut state = self.inner.lock().unwrap();
        state.samples.extend(samples);
        if state.samples.len() > MAX_FILL {
            let excess = state.samples.len() - TARGET_FILL;
            state.samples.drain(..excess);
        }
    }

    /// Takes everything currently queued (used for sending captured audio).
    pub fn take_all(&self) -> Vec<f32> {
        self.inner.lock().unwrap().samples.drain(..).collect()
    }

    /// Fills `out` for playback. Plays silence until the target fill is reached, and again after
    /// running dry, so a late packet causes one short gap instead of constant crackling.
    pub fn pull(&self, out: &mut [f32]) {
        let mut state = self.inner.lock().unwrap();
        if !state.playing && state.samples.len() >= TARGET_FILL {
            state.playing = true;
        }
        for slot in out.iter_mut() {
            *slot = if state.playing { state.samples.pop_front().unwrap_or(0.0) } else { 0.0 };
        }
        if state.samples.is_empty() {
            state.playing = false;
        }
    }

    /// While muted, incoming audio is dropped (used for the mute and deafen buttons).
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.clear();
            self.peak.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
    }

    /// The loudest sample since the last call (0.0–1.0).
    pub fn take_level(&self) -> f32 {
        f32::from_bits(self.peak.swap(0.0f32.to_bits(), Ordering::Relaxed))
    }

    pub fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.samples.clear();
        state.playing = false;
    }
}

pub fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown device".into())
}

/// Finds an output device whose name contains `needle` (case-insensitive).
pub fn find_output(needle: &str) -> Option<Device> {
    let needle = needle.to_lowercase();
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|d| device_name(d).to_lowercase().contains(&needle))
}

/// Records from `device` into `sink` as mono 48 kHz. Passing an output device records what it's
/// playing (Windows loopback capture).
pub fn start_capture(device: &Device, sink: Buffer, loopback: bool) -> Result<Stream, String> {
    let supported = if loopback { device.default_output_config() } else { device.default_input_config() }
        .map_err(|e| format!("couldn't read the device's audio format: {e}"))?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate, RATE);
    let mut mono = Vec::new();
    let mut resampled = Vec::new();
    let mut handle = move |frames: &mut dyn Iterator<Item = f32>| {
        mono.clear();
        let mut acc = 0.0;
        for (i, s) in frames.enumerate() {
            acc += s;
            if i % channels == channels - 1 {
                mono.push(acc / channels as f32);
                acc = 0.0;
            }
        }
        resampled.clear();
        resampler.process(&mono, &mut resampled);
        sink.push(&resampled);
    };
    // Windows reports a harmless glitch whenever capture starts or has nothing to record, so skip
    // that one and only say anything else once.
    let mut reported = false;
    let err = move |e: cpal::Error| {
        let message = e.to_string();
        if !reported && !message.contains("underrun or overrun") {
            reported = true;
            eprintln!("audio capture problem: {message}");
        }
    };
    let stream = match format {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |data, _| handle(&mut data.iter().copied()),
            err,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |data, _| handle(&mut data.iter().map(|&s| s as f32 / i16::MAX as f32)),
            err,
            None,
        ),
        SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
            config,
            move |data, _| handle(&mut data.iter().map(|&s| s as f32 / i32::MAX as f32)),
            err,
            None,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| format!("couldn't open {}: {e}", device_name(device)))?;
    stream.play().map_err(|e| format!("couldn't start {}: {e}", device_name(device)))?;
    Ok(stream)
}

/// Plays `source` (mono 48 kHz) on `device`, copying it to every channel.
pub fn start_playback(device: &Device, source: Buffer) -> Result<Stream, String> {
    let supported = device
        .default_output_config()
        .map_err(|e| format!("couldn't read the device's audio format: {e}"))?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = config.channels.max(1) as usize;
    // Resample by pulling at 48 kHz and converting to the device rate.
    let mut resampler = Resampler::new(RATE, config.sample_rate);
    let mut pending: VecDeque<f32> = VecDeque::new();
    let mut chunk = vec![0.0f32; 480];
    let mut converted = Vec::new();
    let mut fill = move |frames: usize| -> Vec<f32> {
        while pending.len() < frames {
            source.pull(&mut chunk);
            converted.clear();
            resampler.process(&chunk, &mut converted);
            pending.extend(converted.iter());
        }
        pending.drain(..frames).collect()
    };
    let err = |e| eprintln!("audio playback error: {e}");
    let stream = match format {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |data, _| {
                let mono = fill(data.len() / channels);
                for (frame, s) in data.chunks_mut(channels).zip(mono) {
                    frame.fill(s);
                }
            },
            err,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |data, _| {
                let mono = fill(data.len() / channels);
                for (frame, s) in data.chunks_mut(channels).zip(mono) {
                    frame.fill((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                }
            },
            err,
            None,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| format!("couldn't open {}: {e}", device_name(device)))?;
    stream.play().map_err(|e| format!("couldn't start {}: {e}", device_name(device)))?;
    Ok(stream)
}

pub fn default_output() -> Option<Device> {
    cpal::default_host().default_output_device()
}

pub fn default_input() -> Option<Device> {
    cpal::default_host().default_input_device()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_length_ratio() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&[0.5; 441], &mut out);
        }
        // 44 100 input samples should become about 48 000 output samples.
        assert!((out.len() as i64 - 48_000).abs() <= 2, "got {}", out.len());
        assert!(out[100..].iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn resampler_passthrough_at_same_rate() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn buffer_waits_then_plays_then_trims() {
        let buffer = Buffer::default();
        let mut out = vec![1.0; 10];
        buffer.push(&[0.5; 100]);
        buffer.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "should wait for the target fill");
        buffer.push(&vec![0.5; TARGET_FILL]);
        buffer.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.5));
        buffer.push(&vec![0.5; MAX_FILL * 2]);
        assert!(buffer.take_all().len() <= MAX_FILL);
    }

    #[test]
    fn muting_drops_audio_and_levels_reset() {
        let buffer = Buffer::default();
        buffer.push(&[0.25, -0.75]);
        assert_eq!(buffer.take_level(), 0.75);
        assert_eq!(buffer.take_level(), 0.0);
        buffer.set_muted(true);
        buffer.push(&[0.5; 10]);
        assert!(buffer.take_all().is_empty());
        assert_eq!(buffer.take_level(), 0.0);
        buffer.set_muted(false);
        buffer.push(&[0.5; 10]);
        assert_eq!(buffer.take_all().len(), 10);
    }
}
