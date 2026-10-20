//! Sample-rate conversion (M8, first stage): a synchronous FFT resampler
//! (rubato) running in 64-bit float, fed in fixed chunks. The pipeline only
//! plans it when the output cannot take the source rate and the config
//! allows it; the signal path then shows a `Lossy` `resample` stage.
//!
//! Streaming contract: [`Resampler::push`] returns whatever whole chunks
//! produced; [`Resampler::finish`] flushes the filter delay so the track
//! ends with the right number of frames, `round(input × ratio)`.

use crate::pipeline::PipelineError;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler as _};

/// Input frames per chunk; ~85 ms at 48 kHz, small enough for responsive seeks.
const CHUNK: usize = 4096;

pub struct Resampler {
    inner: Fft<f64>,
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    pending: Vec<f64>,
    frames_in: u64,
    frames_out: u64,
    /// Leading frames of filter delay still to drop.
    skip: usize,
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Resampler({} -> {} Hz, {} ch)",
            self.in_rate, self.out_rate, self.channels
        )
    }
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Result<Self, PipelineError> {
        let inner = Fft::<f64>::new(
            in_rate as usize,
            out_rate as usize,
            CHUNK,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| PipelineError::InvalidFormat(format!("resampler: {e}")))?;
        let skip = inner.output_delay();
        Ok(Self {
            inner,
            channels,
            in_rate,
            out_rate,
            pending: Vec::with_capacity(CHUNK * channels),
            frames_in: 0,
            frames_out: 0,
            skip,
        })
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Feed interleaved f64 frames; returns the interleaved output ready so far.
    pub fn push(&mut self, interleaved: &[f64]) -> Result<Vec<f64>, PipelineError> {
        self.frames_in += (interleaved.len() / self.channels) as u64;
        self.pending.extend_from_slice(interleaved);
        let mut out = Vec::new();
        let need = CHUNK * self.channels;
        while self.pending.len() >= need {
            let chunk: Vec<f64> = self.pending.drain(..need).collect();
            self.run_chunk(&chunk, &mut out)?;
        }
        Ok(out)
    }

    /// Flush: pad the partial chunk and the filter delay with silence, then
    /// trim so that the total output is `round(frames_in × ratio)`.
    pub fn finish(&mut self) -> Result<Vec<f64>, PipelineError> {
        let target = ((self.frames_in as f64) * self.out_rate as f64 / self.in_rate as f64).round() as u64;
        let mut out = Vec::new();
        let need = CHUNK * self.channels;
        // Enough zero chunks to push the delayed tail out.
        let delay_chunks = self.inner.output_delay().div_ceil(CHUNK) + 1;
        for _ in 0..=delay_chunks {
            if self.frames_out >= target {
                break;
            }
            self.pending.resize(need, 0.0);
            let chunk: Vec<f64> = self.pending.drain(..need).collect();
            self.run_chunk(&chunk, &mut out)?;
        }
        let excess = self.frames_out.saturating_sub(target) as usize * self.channels;
        if excess > 0 && excess <= out.len() {
            out.truncate(out.len() - excess);
            self.frames_out = target;
        }
        self.pending.clear();
        Ok(out)
    }

    fn run_chunk(&mut self, chunk: &[f64], out: &mut Vec<f64>) -> Result<(), PipelineError> {
        let input = InterleavedSlice::new(chunk, self.channels, CHUNK)
            .map_err(|e| PipelineError::InvalidFormat(format!("resampler input: {e}")))?;
        let produced = self
            .inner
            .process(&input, None)
            .map_err(|e| PipelineError::InvalidFormat(format!("resampler: {e}")))?;
        let mut data = produced.take_data();
        if self.skip > 0 {
            let frames = data.len() / self.channels;
            let drop = self.skip.min(frames);
            data.drain(..drop * self.channels);
            self.skip -= drop;
        }
        self.frames_out += (data.len() / self.channels) as u64;
        out.extend_from_slice(&data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, frames: usize, channels: usize) -> Vec<f64> {
        (0..frames)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() * 0.5;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    #[test]
    fn length_and_tone_survive_96k_to_48k() {
        let mut r = Resampler::new(96_000, 48_000, 2).unwrap();
        let frames = 96_000; // one second
        let input = sine(96_000, 1000.0, frames, 2);
        let mut out = Vec::new();
        for part in input.chunks(2 * 1000) {
            out.extend(r.push(part).unwrap());
        }
        out.extend(r.finish().unwrap());
        assert_eq!(out.len() / 2, 48_000, "one second in, one second out");
        // Energy of the middle half is that of a 0.5-amplitude sine (RMS 0.3536), within 1%.
        let mid: Vec<f64> = out[(out.len() / 4)..(3 * out.len() / 4)]
            .iter()
            .step_by(2)
            .copied()
            .collect();
        let rms = (mid.iter().map(|x| x * x).sum::<f64>() / mid.len() as f64).sqrt();
        assert!((rms - 0.3536).abs() < 0.0036, "rms {rms}");
        // 1 kHz at 48 kHz: zero crossings per second ≈ 2000.
        let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let expected = 2000.0 * mid.len() as f64 / 48_000.0;
        assert!(
            (crossings as f64 - expected).abs() <= 4.0,
            "{crossings} vs {expected}"
        );
    }

    #[test]
    fn upsampling_and_odd_ratios_keep_the_frame_count() {
        for (a, b, n) in [
            (44_100u32, 48_000u32, 44_100usize),
            (48_000, 44_100, 5000),
            (44_100, 96_000, 1234),
        ] {
            let mut r = Resampler::new(a, b, 1).unwrap();
            let input = sine(a, 440.0, n, 1);
            let mut out = r.push(&input).unwrap();
            out.extend(r.finish().unwrap());
            let want = (n as f64 * b as f64 / a as f64).round() as usize;
            assert_eq!(out.len(), want, "{a}->{b} with {n} frames");
        }
    }
}
