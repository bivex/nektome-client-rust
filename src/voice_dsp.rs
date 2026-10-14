//! Pure DSP helpers for the RVC voice changer.
//!
//! Everything here is independent of ONNX Runtime so it can be unit-tested
//! without model files:
//! - model-rate detection and conversion to the 48 kHz engine rate;
//! - SOLA (Synchronized Overlap-Add) stitching of independently synthesized chunks;
//! - the playout stage that adapts converted 48 kHz audio to the microphone
//!   device rate, with prebuffering and dry/wet mixing.

#![allow(clippy::all, clippy::pedantic)]

use crate::audio::LinearResampler;
use std::collections::VecDeque;

/// Rate of the audio produced by the voice-changer worker.
pub const ENGINE_RATE: u32 = 48_000;

/// Hop duration of the voice-changer worker (ms).
pub const HOP_MS: usize = 200;
/// Prebuffering cushion before converted audio is played (ms).
pub const PREBUFFER_MS: usize = 400;
/// Hard latency cap for the playout queue (ms).
pub const MAX_BUFFER_MS: usize = 1000;
/// Peak scale of converted audio (~ -1.1 dBFS).
pub const OUTPUT_SCALE: f32 = 28_835.0;

/// Derives the synthesizer output rate from its output length.
///
/// RVC synthesizers emit one hop per 10 ms feature frame, so
/// `rate = (out_len / frames) * 100` (32k -> 320, 40k -> 400, 48k -> 480).
/// Returns `None` when the length is not a whole number of hops or the implied
/// rate is implausible.
#[must_use]
pub fn model_rate_from_output(out_len: usize, frames: usize) -> Option<u32> {
    if frames == 0 || out_len == 0 || out_len % frames != 0 {
        return None;
    }
    let rate = u32::try_from(out_len / frames).ok()?.checked_mul(100)?;
    (16_000..=96_000).contains(&rate).then_some(rate)
}

/// Linearly resamples `input` to exactly `out_len` samples (edge-clamped).
#[must_use]
pub fn resample_to_len(input: &[f32], out_len: usize) -> Vec<f32> {
    if input.is_empty() || out_len == 0 {
        return Vec::new();
    }
    if input.len() == out_len {
        return input.to_vec();
    }
    let ratio = input.len() as f64 / out_len as f64;
    let last = input.len() - 1;
    (0..out_len)
        .map(|i| {
            let src = i as f64 * ratio;
            let i0 = (src.floor() as usize).min(last);
            let i1 = (i0 + 1).min(last);
            let frac = (src - i0 as f64) as f32;
            input[i0] + frac * (input[i1] - input[i0])
        })
        .collect()
}

/// Converts synthesizer output at `model_rate` to [`ENGINE_RATE`].
#[must_use]
pub fn to_engine_rate(raw: &[f32], model_rate: u32) -> Vec<f32> {
    if model_rate == ENGINE_RATE || model_rate == 0 {
        return raw.to_vec();
    }
    let out_len = (raw.len() as u64 * u64::from(ENGINE_RATE) + u64::from(model_rate) / 2)
        / u64::from(model_rate);
    resample_to_len(raw, out_len as usize)
}

/// Synchronized Overlap-Add stitcher for chunked, independently synthesized audio.
///
/// Every chunk handed to [`Sola::push`] must hold at least
/// [`Sola::required_len`] samples: `block + overlap + search`. The stitcher
/// searches the first `search + 1` start offsets for the one whose waveform best
/// matches the previous chunk's saved tail (normalized cross-correlation),
/// crossfades over `overlap` samples there, and emits exactly `block` samples.
pub struct Sola {
    block: usize,
    overlap: usize,
    search: usize,
    tail: Vec<f32>,
}

impl Sola {
    #[must_use]
    pub fn new(block: usize, overlap: usize, search: usize) -> Self {
        assert!(block >= overlap && overlap > 0, "invalid SOLA geometry");
        Self {
            block,
            overlap,
            search,
            tail: Vec::new(),
        }
    }

    #[must_use]
    pub const fn required_len(&self) -> usize {
        self.block + self.overlap + self.search
    }

    pub fn reset(&mut self) {
        self.tail.clear();
    }

    /// Stitches one synthesized chunk; returns `block` samples, or `None` if the
    /// chunk is too short.
    pub fn push(&mut self, out: &[f32]) -> Option<Vec<f32>> {
        if out.len() < self.required_len() {
            return None;
        }
        let mut emitted = Vec::with_capacity(self.block);
        let offset = if self.tail.len() == self.overlap {
            let offset = best_offset(&self.tail, out, self.search);
            for k in 0..self.overlap {
                let t = (k as f32 + 0.5) / self.overlap as f32;
                let w = 0.5 * (1.0 - (std::f32::consts::PI * t).cos());
                emitted.push((1.0 - w) * self.tail[k] + w * out[offset + k]);
            }
            emitted.extend_from_slice(&out[offset + self.overlap..offset + self.block]);
            offset
        } else {
            // First chunk after startup/reset: nothing to align against.
            emitted.extend_from_slice(&out[..self.block]);
            0
        };
        self.tail.clear();
        self.tail
            .extend_from_slice(&out[offset + self.block..offset + self.block + self.overlap]);
        Some(emitted)
    }
}

/// Start offset in `0..=search` maximizing `<tail, out[off..off+n]> / ||out[off..off+n]||`.
/// Ties (e.g. silence) resolve to the smallest offset.
fn best_offset(tail: &[f32], out: &[f32], search: usize) -> usize {
    let n = tail.len();
    let mut best = 0;
    let mut best_score = f32::NEG_INFINITY;
    for off in 0..=search {
        let window = &out[off..off + n];
        let mut corr = 0.0f32;
        let mut energy = 0.0f32;
        for (a, b) in tail.iter().zip(window) {
            corr += a * b;
            energy += b * b;
        }
        let score = corr / (energy.sqrt() + 1e-6);
        if score > best_score {
            best_score = score;
            best = off;
        }
    }
    best
}

/// Soft-knee saturation above 0.85, then scaling to i16 range.
#[must_use]
pub fn soft_clip_to_i16(x: f32) -> i16 {
    let ax = x.abs();
    let soft = if ax <= 0.85 {
        x
    } else {
        x.signum() * (0.85 + 0.15 * ((ax - 0.85) / 0.15).tanh())
    };
    (soft * OUTPUT_SCALE).round().clamp(-32768.0, 32767.0) as i16
}

/// Playout stage: queues converted 48 kHz audio at the *microphone device rate*
/// and mixes it into the live callback buffer.
pub struct Playout {
    resampler: LinearResampler,
    scratch_48k: Vec<i16>,
    scratch_dev: Vec<i16>,
    ready: VecDeque<i16>,
    dry_history: VecDeque<i16>,
    device_rate: u32,
    prebuffering: bool,
}

impl Default for Playout {
    fn default() -> Self {
        Self::new()
    }
}

impl Playout {
    #[must_use]
    pub fn new() -> Self {
        Self {
            resampler: LinearResampler::new(),
            scratch_48k: Vec::with_capacity(9600),
            scratch_dev: Vec::with_capacity(9600),
            ready: VecDeque::with_capacity(48_000),
            dry_history: VecDeque::with_capacity(48_000),
            device_rate: 0,
            prebuffering: true,
        }
    }

    pub fn reset(&mut self) {
        self.resampler.reset();
        self.scratch_48k.clear();
        self.scratch_dev.clear();
        self.ready.clear();
        self.dry_history.clear();
        self.prebuffering = true;
    }

    /// Queued samples at the device rate.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.ready.len()
    }

    /// Appends a converted chunk (48 kHz, nominal range -1..1).
    pub fn push_converted(&mut self, chunk: &[f32], device_rate: u32) {
        if device_rate == 0 {
            return;
        }
        if device_rate != self.device_rate {
            // Device rate changed (or first use): queued audio has the wrong rate.
            self.reset();
            self.device_rate = device_rate;
        }
        self.scratch_48k.clear();
        self.scratch_48k
            .extend(chunk.iter().map(|&s| soft_clip_to_i16(s)));
        self.resampler.resample_into(
            &self.scratch_48k,
            ENGINE_RATE,
            device_rate,
            &mut self.scratch_dev,
        );
        self.ready.extend(self.scratch_dev.iter().copied());
    }

    /// Replaces/mixes the microphone buffer (`device_rate` Hz) with queued
    /// converted audio. `wet = 1.0` fully replaces the live voice.
    ///
    /// When `0.0 < wet < 1.0`, dry audio is delayed through `dry_history` to
    /// match the pipeline latency (~600 ms) so dry and wet voices stay
    /// synchronized without stadium echo.
    pub fn mix_into(&mut self, pcm: &mut [i16], device_rate: u32, wet: f32) {
        let wet = wet.clamp(0.0, 1.0);
        if wet < 0.001 {
            self.dry_history.clear();
            return;
        }

        let prebuffer = device_rate as usize * PREBUFFER_MS / 1000;
        let max_buffer = device_rate as usize * MAX_BUFFER_MS / 1000;

        // Fast path for 100% wet: zero dry-voice leak and no delay-line overhead.
        if wet >= 0.999 {
            self.dry_history.clear();
            if self.prebuffering {
                if self.ready.len() >= prebuffer {
                    self.prebuffering = false;
                } else {
                    scale(pcm, 0.0);
                    return;
                }
            }
            if self.ready.len() > max_buffer {
                let excess = self.ready.len() - max_buffer;
                self.ready.drain(..excess);
            }
            let n = pcm.len().min(self.ready.len());
            for sample in &mut pcm[..n] {
                *sample = self.ready.pop_front().unwrap_or(0);
            }
            scale(&mut pcm[n..], 0.0);
            if self.ready.is_empty() {
                self.prebuffering = true;
            }
            return;
        }

        // Blended mix (0 < wet < 1): delay dry audio to match wet pipeline latency.
        let delay_samples = device_rate as usize * (HOP_MS + PREBUFFER_MS) / 1000;
        self.dry_history.extend(pcm.iter().copied());

        let dry_gain = 1.0 - wet;
        if self.prebuffering {
            if self.ready.len() >= prebuffer && self.dry_history.len() >= delay_samples {
                self.prebuffering = false;
            } else {
                scale(pcm, 0.0);
                return;
            }
        }
        if self.ready.len() > max_buffer {
            let excess = self.ready.len() - max_buffer;
            self.ready.drain(..excess);
        }
        while self.dry_history.len() > max_buffer + delay_samples {
            self.dry_history.pop_front();
        }

        let n = pcm.len().min(self.ready.len());
        for sample in &mut pcm[..n] {
            let conv = f32::from(self.ready.pop_front().unwrap_or(0));
            let dry = f32::from(self.dry_history.pop_front().unwrap_or(0));
            *sample = (dry_gain * dry + wet * conv).clamp(-32768.0, 32767.0) as i16;
        }
        scale(&mut pcm[n..], dry_gain);
        if self.ready.is_empty() {
            self.prebuffering = true;
        }
    }
}

fn scale(pcm: &mut [i16], gain: f32) {
    for s in pcm {
        *s = (f32::from(*s) * gain).clamp(-32768.0, 32767.0) as i16;
    }
}

#[cfg(test)]
#[allow(clippy::all, clippy::pedantic)]
mod tests {
    use super::*;

    const BLOCK: usize = 9_600;
    const OVERLAP: usize = 960;
    const SEARCH: usize = 480;

    fn sine(freq: f32, phase: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0 + phase).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn model_rate_is_derived_from_output_length() {
        assert_eq!(model_rate_from_output(7_040, 22), Some(32_000));
        assert_eq!(model_rate_from_output(8_800, 22), Some(40_000));
        assert_eq!(model_rate_from_output(10_560, 22), Some(48_000));
        assert_eq!(model_rate_from_output(8_801, 22), None);
        assert_eq!(model_rate_from_output(0, 22), None);
        assert_eq!(model_rate_from_output(100, 0), None);
    }

    #[test]
    fn engine_rate_conversion_has_exact_length() {
        for rate in [32_000usize, 40_000, 48_000] {
            let raw = vec![0.1f32; 23 * rate / 100];
            assert_eq!(to_engine_rate(&raw, rate as u32).len(), 23 * 480);
        }
    }

    #[test]
    fn sola_emits_exactly_one_block_and_rejects_short_chunks() {
        let mut sola = Sola::new(BLOCK, OVERLAP, SEARCH);
        assert_eq!(sola.required_len(), BLOCK + OVERLAP + SEARCH);
        assert!(sola.push(&vec![0.0; sola.required_len() - 1]).is_none());
        let chunk = sine(200.0, 0.0, sola.required_len());
        for _ in 0..3 {
            assert_eq!(sola.push(&chunk).unwrap().len(), BLOCK);
        }
    }

    /// Chunks whose phase is inverted relative to the previous tail (what a
    /// per-chunk NSF phase reset can produce) cancel under a fixed crossfade but
    /// are re-aligned by SOLA.
    #[test]
    fn sola_avoids_phase_cancellation() {
        // 200 Hz: 40 whole cycles per block, so phase offset pi == antiphase.
        let len = BLOCK + OVERLAP + SEARCH;
        let first = sine(200.0, 0.0, len);
        let second = sine(200.0, std::f32::consts::PI, len);

        let run = |search: usize| {
            let mut s = Sola::new(BLOCK, OVERLAP, search);
            s.push(&first).unwrap();
            let out = s.push(&second).unwrap();
            rms(&out[..OVERLAP])
        };
        let naive = run(0);
        let aligned = run(SEARCH);
        let ideal = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            naive < 0.55 * 1.0,
            "fixed crossfade should dip, got {naive}"
        );
        assert!(
            aligned > 0.97 * ideal,
            "SOLA crossfade should keep level, got {aligned}"
        );
    }

    #[test]
    fn sola_tie_on_silence_uses_offset_zero() {
        let tail = vec![0.0f32; OVERLAP];
        let out = vec![0.0f32; BLOCK + OVERLAP + SEARCH];
        assert_eq!(best_offset(&tail, &out, SEARCH), 0);
    }

    #[test]
    fn playout_resamples_to_device_rate() {
        for (rate, per_chunk) in [(16_000u32, 3_200usize), (44_100, 8_820), (48_000, 9_600)] {
            let mut p = Playout::new();
            let chunk = vec![0.5f32; 9_600];
            for _ in 0..3 {
                p.push_converted(&chunk, rate);
            }
            let got = p.queued() as i64;
            assert!(
                (got - 3 * per_chunk as i64).abs() <= 3,
                "rate {rate}: queued {got}"
            );
        }
    }

    #[test]
    fn playout_prebuffers_without_leaking_dry_voice() {
        let mut p = Playout::new();
        p.push_converted(&vec![0.5f32; 9_600], 48_000); // 200 ms < 400 ms cushion
        let mut pcm = vec![1_000i16; 480];
        p.mix_into(&mut pcm, 48_000, 1.0);
        assert!(pcm.iter().all(|&s| s == 0));
        assert_eq!(p.queued(), 9_600);
    }

    #[test]
    fn playout_mixes_at_device_rate_after_prebuffer() {
        let mut p = Playout::new();
        for _ in 0..3 {
            p.push_converted(&vec![0.5f32; 9_600], 16_000);
        }
        let queued = p.queued();
        let mut pcm = vec![0i16; 160]; // 10 ms at 16 kHz
        p.mix_into(&mut pcm, 16_000, 1.0);
        assert_eq!(
            queued - p.queued(),
            160,
            "one device-rate callback worth consumed"
        );
        let expected = soft_clip_to_i16(0.5);
        assert!(
            pcm.iter()
                .all(|&s| (i32::from(s) - i32::from(expected)).abs() <= 1)
        );
    }

    #[test]
    fn playout_resets_when_device_rate_changes() {
        let mut p = Playout::new();
        p.push_converted(&vec![0.5f32; 9_600], 48_000);
        p.push_converted(&vec![0.5f32; 9_600], 16_000);
        assert!((p.queued() as i64 - 3_200).abs() <= 2);
    }

    #[test]
    fn playout_delays_dry_audio_when_blended() {
        let mut p = Playout::new();
        let rate = 16_000u32;
        // 4 chunks of 200ms = 800ms of converted audio (enough for 400ms prebuffer)
        for _ in 0..4 {
            p.push_converted(&vec![0.0f32; 9_600], rate);
        }
        // Feed 600ms of dry audio (HOP_MS 200 + PREBUFFER_MS 400 = 9600 samples at 16kHz)
        // in chunks of 160 samples (10ms)
        for i in 0..60 {
            let mut pcm = vec![1000i16; 160];
            p.mix_into(&mut pcm, rate, 0.5);
            // During prebuffering, output is silenced
            if i < 59 {
                assert!(pcm.iter().all(|&s| s == 0));
            }
        }
        // Now ready: next callback gets the delayed dry audio (which was 1000 * 0.5 = 500)
        let mut pcm = vec![2000i16; 160];
        p.mix_into(&mut pcm, rate, 0.5);
        // Wet is 0.0, dry was 1000, so mixture is 0.5 * 1000 + 0.5 * 0 = 500
        assert_eq!(pcm[0], 500);
    }
}
