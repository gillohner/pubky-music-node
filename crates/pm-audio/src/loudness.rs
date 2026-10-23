//! Measured loudness (EBU R128) for files whose tags carry no ReplayGain
//! (compare-music-assistant #7). The node measures them in the background while a zone has
//! loudness on, and the ReplayGain stage uses the result in place of the missing tag; with
//! loudness off nothing changes, so bit-perfect stays the default.
//!
//! The gain follows ReplayGain 2.0: the target is −18 LUFS, and a gain that would push the
//! file's sample peak past full scale is held back to keep it at 0 dBFS.

use crate::{AudioError, Decoder};
use ebur128::{EbuR128, Mode};
use pm_model::{SampleKind, Samples};
use std::path::Path;

/// ReplayGain 2.0's reference loudness.
pub const TARGET_LUFS: f64 = -18.0;

/// One file's measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// Integrated loudness, LUFS.
    pub lufs: f64,
    /// Sample peak, linear (1.0 = full scale).
    pub peak: f64,
}

fn err(path: &Path, why: impl std::fmt::Display) -> AudioError {
    AudioError::Unsupported(format!("{}: {why}", path.display()))
}

/// Decode `path` and measure it. Fails on DSD, on silence, and on what can't be decoded.
pub fn measure(path: &Path) -> Result<Measured, AudioError> {
    let mut dec = Decoder::open(path)?;
    let fmt = dec.format();
    if fmt.kind != SampleKind::Pcm {
        return Err(err(path, "not PCM"));
    }
    let channels = u32::from(fmt.channels.max(1));
    let mut m = EbuR128::new(channels, fmt.rate_hz, Mode::I | Mode::SAMPLE_PEAK).map_err(|e| err(path, e))?;
    let mut buf: Vec<f64> = Vec::new();
    while let Some(b) = dec.next_block()? {
        match &b.samples {
            Samples::PcmI32(v) => {
                let scale = 1.0 / (1u64 << (b.format.bits.clamp(1, 32) - 1)) as f64;
                buf.clear();
                buf.extend(v.iter().map(|s| f64::from(*s) * scale));
                m.add_frames_f64(&buf).map_err(|e| err(path, e))?;
            }
            Samples::PcmF64(v) => m.add_frames_f64(v).map_err(|e| err(path, e))?,
            Samples::Dsd(_) => return Err(err(path, "not PCM")),
        }
    }
    let lufs = m.loudness_global().map_err(|e| err(path, e))?;
    if !lufs.is_finite() {
        return Err(err(path, "silent"));
    }
    let mut peak = 0f64;
    for c in 0..channels {
        peak = peak.max(m.sample_peak(c).map_err(|e| err(path, e))?);
    }
    Ok(Measured { lufs, peak })
}

/// The ReplayGain-style gain for a measurement, in dB: to −18 LUFS, at most what keeps the
/// sample peak at full scale, within −24…+12 dB.
pub fn gain_db(lufs: f64, peak: f64) -> f32 {
    let mut g = TARGET_LUFS - lufs;
    if peak > 0.0 {
        g = g.min(-20.0 * peak.log10());
    }
    g.clamp(-24.0, 12.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 kHz stereo sine at `amp` (full scale = 1.0), 16-bit, 3 s.
    fn sine(path: &Path, amp: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..48_000 * 3 {
            let s =
                (amp * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin() * 32767.0) as i16;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn measures_a_sine_and_turns_it_into_a_gain() {
        let d = tempfile::tempdir().unwrap();
        // A stereo 1 kHz sine at -20 dBFS peak reads about -20 LUFS (EBU Tech 3341 at -23)
        let p = d.path().join("quiet.wav");
        sine(&p, 0.1);
        let m = measure(&p).unwrap();
        assert!((m.lufs - -20.0).abs() < 0.5, "{m:?}");
        assert!((m.peak - 0.1).abs() < 0.01, "{m:?}");
        // +2 dB to -18 LUFS; the peak (-20 dBFS) leaves room
        assert!((gain_db(m.lufs, m.peak) - 2.0).abs() < 0.5);
        // A loud one comes down; a quiet one with a hot peak is held at full scale
        assert!((gain_db(-8.0, 1.0) - -10.0).abs() < 1e-6);
        assert!((gain_db(-30.0, 0.5) - 6.0206).abs() < 1e-3);
        assert_eq!(gain_db(-60.0, 0.0), 12.0);
    }

    #[test]
    fn silence_is_not_measured() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("silent.wav");
        sine(&p, 0.0);
        assert!(measure(&p).is_err());
    }
}
