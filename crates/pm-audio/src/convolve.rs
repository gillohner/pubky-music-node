//! Convolution (room correction, M8): uniformly partitioned overlap-save
//! FFT convolution in 64-bit float, one impulse response per channel (a
//! mono IR applies to every channel). Latency is one partition
//! ([`PARTITION`] frames); [`Convolver::finish`] flushes the tail at the
//! end of a track so nothing of the reverb is cut.

use crate::pipeline::PipelineError;
use pm_model::Samples;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::path::Path;
use std::sync::Arc;

/// Input frames per FFT block; FFT size is twice this.
pub const PARTITION: usize = 4096;

/// An impulse response as loaded from a file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpulseResponse {
    pub rate_hz: u32,
    /// One vector per channel.
    pub channels: Vec<Vec<f64>>,
    pub name: String,
}

impl ImpulseResponse {
    /// Load a WAV (16/24/32-bit PCM) as an impulse response.
    pub fn from_wav(path: &Path) -> Result<Self, PipelineError> {
        let block = pm_zones::outputs::read_wav(path)
            .map_err(|e| PipelineError::InvalidFormat(format!("impulse response {}: {e}", path.display())))?;
        let ch = block.format.channels as usize;
        let scale = 1.0 / (1u64 << (block.format.bits as u32 - 1)) as f64;
        let interleaved: Vec<f64> = match &block.samples {
            Samples::PcmI32(v) => v.iter().map(|s| *s as f64 * scale).collect(),
            Samples::PcmF64(v) => v.clone(),
            Samples::Dsd(_) => {
                return Err(PipelineError::InvalidFormat(
                    "impulse response must be PCM".into(),
                ));
            }
        };
        let frames = interleaved.len() / ch;
        if frames == 0 {
            return Err(PipelineError::InvalidFormat("impulse response is empty".into()));
        }
        let channels = (0..ch)
            .map(|c| (0..frames).map(|f| interleaved[f * ch + c]).collect())
            .collect();
        Ok(Self {
            rate_hz: block.format.rate_hz,
            channels,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        })
    }

    pub fn taps(&self) -> usize {
        self.channels.first().map(Vec::len).unwrap_or(0)
    }
}

struct ChannelState {
    /// Frequency-domain IR partitions.
    partitions: Vec<Vec<Complex<f64>>>,
    /// Frequency-delay line of past input spectra, newest first.
    history: Vec<Vec<Complex<f64>>>,
    /// Last `PARTITION` input samples (overlap-save).
    overlap: Vec<f64>,
}

pub struct Convolver {
    fft: Arc<dyn RealToComplex<f64>>,
    ifft: Arc<dyn ComplexToReal<f64>>,
    states: Vec<ChannelState>,
    channels: usize,
    gain: f64,
    pending: Vec<f64>,
    frames_in: u64,
    frames_out: u64,
    taps: usize,
    description: String,
}

impl std::fmt::Debug for Convolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Convolver({})", self.description)
    }
}

impl Convolver {
    /// `ir` must be at the stream's rate; a mono IR is used for every channel.
    pub fn new(
        ir: &ImpulseResponse,
        rate_hz: u32,
        channels: usize,
        gain_db: f32,
    ) -> Result<Self, PipelineError> {
        if ir.rate_hz != rate_hz {
            return Err(PipelineError::InvalidFormat(format!(
                "impulse response is {} Hz, the stream {} Hz (resample the IR)",
                ir.rate_hz, rate_hz
            )));
        }
        if ir.channels.len() != 1 && ir.channels.len() != channels {
            return Err(PipelineError::InvalidFormat(format!(
                "impulse response has {} channels, the stream {channels}",
                ir.channels.len()
            )));
        }
        let n = 2 * PARTITION;
        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        let mut states = Vec::with_capacity(channels);
        for c in 0..channels {
            let taps = &ir.channels[if ir.channels.len() == 1 { 0 } else { c }];
            let mut partitions = Vec::new();
            for chunk in taps.chunks(PARTITION) {
                let mut time = vec![0.0; n];
                time[..chunk.len()].copy_from_slice(chunk);
                let mut spec = fft.make_output_vec();
                fft.process(&mut time, &mut spec)
                    .map_err(|e| PipelineError::InvalidFormat(format!("fft: {e}")))?;
                partitions.push(spec);
            }
            let k = partitions.len();
            states.push(ChannelState {
                partitions,
                history: vec![vec![Complex::new(0.0, 0.0); n / 2 + 1]; k],
                overlap: vec![0.0; PARTITION],
            });
        }
        let gain = 10f64.powf(gain_db as f64 / 20.0);
        Ok(Self {
            fft,
            ifft,
            states,
            channels,
            gain,
            pending: Vec::new(),
            frames_in: 0,
            frames_out: 0,
            taps: ir.taps(),
            description: format!(
                "{} ({} taps at {} Hz){}",
                ir.name,
                ir.taps(),
                ir.rate_hz,
                if gain_db != 0.0 {
                    format!(", {gain_db:+.1} dB")
                } else {
                    String::new()
                }
            ),
        })
    }

    pub fn describe(&self) -> &str {
        &self.description
    }

    /// Feed interleaved frames; returns convolved frames (delayed by one partition).
    pub fn push(&mut self, interleaved: &[f64]) -> Result<Vec<f64>, PipelineError> {
        self.frames_in += (interleaved.len() / self.channels) as u64;
        self.pending.extend_from_slice(interleaved);
        let need = PARTITION * self.channels;
        let mut out = Vec::new();
        while self.pending.len() >= need {
            let chunk: Vec<f64> = self.pending.drain(..need).collect();
            self.run_block(&chunk, &mut out)?;
        }
        Ok(out)
    }

    /// Flush the partition latency and the IR tail; total output becomes
    /// `frames_in + taps - 1` frames.
    pub fn finish(&mut self) -> Result<Vec<f64>, PipelineError> {
        let target = self.frames_in + self.taps as u64 - 1;
        let need = PARTITION * self.channels;
        let mut out = Vec::new();
        while self.frames_out < target {
            self.pending.resize(need, 0.0);
            let chunk: Vec<f64> = self.pending.drain(..need).collect();
            self.run_block(&chunk, &mut out)?;
        }
        let excess = self.frames_out.saturating_sub(target) as usize * self.channels;
        if excess > 0 && excess <= out.len() {
            out.truncate(out.len() - excess);
            self.frames_out = target;
        }
        Ok(out)
    }

    fn run_block(&mut self, chunk: &[f64], out: &mut Vec<f64>) -> Result<(), PipelineError> {
        let n = 2 * PARTITION;
        let ch = self.channels;
        let mut block_out = vec![0.0f64; PARTITION * ch];
        let mut time = vec![0.0f64; n];
        let mut spec = self.fft.make_output_vec();
        let mut acc = vec![Complex::new(0.0, 0.0); n / 2 + 1];
        for (c, st) in self.states.iter_mut().enumerate() {
            // Overlap-save input: previous partition then the new one.
            time[..PARTITION].copy_from_slice(&st.overlap);
            for (i, t) in time[PARTITION..].iter_mut().enumerate() {
                *t = chunk[i * ch + c];
            }
            st.overlap.copy_from_slice(&time[PARTITION..]);
            self.fft
                .process(&mut time, &mut spec)
                .map_err(|e| PipelineError::InvalidFormat(format!("fft: {e}")))?;
            st.history.rotate_right(1);
            st.history[0].copy_from_slice(&spec);
            for a in acc.iter_mut() {
                *a = Complex::new(0.0, 0.0);
            }
            for (h, p) in st.history.iter().zip(st.partitions.iter()) {
                for ((a, x), y) in acc.iter_mut().zip(h.iter()).zip(p.iter()) {
                    *a += x * y;
                }
            }
            let mut inv = self.ifft.make_output_vec();
            let mut acc_copy = acc.clone();
            self.ifft
                .process(&mut acc_copy, &mut inv)
                .map_err(|e| PipelineError::InvalidFormat(format!("ifft: {e}")))?;
            let norm = self.gain / n as f64;
            for (i, v) in inv[PARTITION..].iter().enumerate() {
                block_out[i * ch + c] = v * norm;
            }
        }
        self.frames_out += PARTITION as u64;
        out.extend_from_slice(&block_out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(taps: Vec<f64>) -> ImpulseResponse {
        ImpulseResponse {
            rate_hz: 48_000,
            channels: vec![taps],
            name: "test".into(),
        }
    }

    fn run(conv: &mut Convolver, input: &[f64]) -> Vec<f64> {
        let mut out = Vec::new();
        for part in input.chunks(1000) {
            out.extend(conv.push(part).unwrap());
        }
        out.extend(conv.finish().unwrap());
        out
    }

    #[test]
    fn delta_is_identity_and_length_adds_taps() {
        let mut conv = Convolver::new(&ir(vec![1.0]), 48_000, 2, 0.0).unwrap();
        let input: Vec<f64> = (0..20_000)
            .map(|i| ((i * 7919) % 1000) as f64 / 1000.0 - 0.5)
            .collect();
        let out = run(&mut conv, &input);
        assert_eq!(out.len(), input.len());
        for (a, b) in out.iter().zip(&input) {
            assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        }
        // Two-tap average, stereo input, 3-tap IR: length grows by taps-1 frames.
        let mut conv = Convolver::new(&ir(vec![0.5, 0.25, 0.25]), 48_000, 2, 0.0).unwrap();
        let input = vec![1.0, -1.0, 0.0, 0.0, 0.0, 0.0]; // 3 frames
        let out = run(&mut conv, &input);
        assert_eq!(out.len() / 2, 3 + 2);
        let left: Vec<f64> = out.iter().step_by(2).copied().collect();
        let right: Vec<f64> = out.iter().skip(1).step_by(2).copied().collect();
        for (v, w) in left.iter().zip([0.5, 0.25, 0.25, 0.0, 0.0]) {
            assert!((v - w).abs() < 1e-9, "{left:?}");
        }
        for (v, w) in right.iter().zip([-0.5, -0.25, -0.25, 0.0, 0.0]) {
            assert!((v - w).abs() < 1e-9, "{right:?}");
        }
    }

    #[test]
    fn long_ir_spans_partitions_and_gain_applies() {
        // IR longer than one partition: a delta at tap 5000 delays by 5000 frames.
        let mut taps = vec![0.0; 5001];
        taps[5000] = 1.0;
        let mut conv = Convolver::new(&ir(taps), 48_000, 1, -6.0206).unwrap();
        let mut input = vec![0.0; 12_000];
        input[10] = 1.0;
        let out = run(&mut conv, &input);
        assert_eq!(out.len(), 12_000 + 5000);
        let (idx, peak) =
            out.iter().enumerate().fold(
                (0, 0.0f64),
                |m, (i, v)| if v.abs() > m.1 { (i, v.abs()) } else { m },
            );
        assert_eq!(idx, 5010);
        assert!((peak - 0.5).abs() < 1e-6, "{peak}");
    }

    #[test]
    fn mismatches_are_refused_and_wavs_load() {
        let mut bad = ir(vec![1.0]);
        bad.rate_hz = 44_100;
        assert!(Convolver::new(&bad, 48_000, 2, 0.0).is_err());
        let three = ImpulseResponse {
            rate_hz: 48_000,
            channels: vec![vec![1.0], vec![1.0], vec![1.0]],
            name: "3ch".into(),
        };
        assert!(Convolver::new(&three, 48_000, 2, 0.0).is_err());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ir.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        w.write_sample(1 << 22).unwrap(); // 0.5 full scale
        w.write_sample(0).unwrap();
        w.finalize().unwrap();
        let ir = ImpulseResponse::from_wav(&p).unwrap();
        assert_eq!(ir.taps(), 2);
        assert!((ir.channels[0][0] - 0.5).abs() < 1e-9);
        assert!(ImpulseResponse::from_wav(&dir.path().join("missing.wav")).is_err());
    }
}
