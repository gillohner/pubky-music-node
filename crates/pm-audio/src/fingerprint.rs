//! Chromaprint fingerprints in-process (no `fpcalc`), in the form AcoustID's
//! lookup API takes: the compressed fingerprint, URL-safe base64 without
//! padding, and the file's duration in whole seconds.
//!
//! Like `fpcalc`, only the first [`MAX_SECONDS`] of audio are used; the
//! algorithm is Chromaprint's default (`TEST2`), mixed to mono 16-bit as the
//! reference implementation does after decoding.

use crate::decode::Decoder;
use crate::error::AudioError;
use base64::Engine;
use pm_model::Samples;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use std::path::Path;

/// Seconds of audio fingerprinted (fpcalc's default `-length 120`).
pub const MAX_SECONDS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// Compressed, URL-safe base64 without padding (AcoustID `fingerprint`).
    pub fingerprint: String,
    /// The whole file's duration in seconds (AcoustID `duration`).
    pub duration_secs: u64,
    /// Raw sub-fingerprints, for local comparisons.
    pub raw: Vec<u32>,
}

/// Fingerprint a PCM file. DSD files are refused (Chromaprint needs PCM;
/// their tags usually carry the IDs anyway).
pub fn fingerprint_file(path: impl AsRef<Path>) -> Result<Fingerprint, AudioError> {
    let mut dec = Decoder::open(path.as_ref())?;
    let fmt = dec.format();
    let duration_secs = dec.duration_ms().map(|ms| ms / 1000).unwrap_or(0);
    let channels = fmt.channels.max(1) as usize;
    let shift = (fmt.bits as i32 - 16).max(0) as u32;
    let up = (16 - fmt.bits as i32).max(0) as u32;
    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    printer
        .start(fmt.rate_hz, 1)
        .map_err(|e| AudioError::Unsupported(format!("fingerprint: {e:?}")))?;
    let limit_frames = MAX_SECONDS * fmt.rate_hz as u64;
    let mut frames_done: u64 = 0;
    let mut mono: Vec<i16> = Vec::new();
    while frames_done < limit_frames {
        let Some(block) = dec.next_block()? else { break };
        let Samples::PcmI32(v) = &block.samples else {
            return Err(AudioError::Unsupported("fingerprint: PCM only".into()));
        };
        mono.clear();
        for frame in v.chunks_exact(channels) {
            if frames_done >= limit_frames {
                break;
            }
            let sum: i64 = frame.iter().map(|&x| x as i64).sum();
            let avg = (sum / channels as i64) as i32;
            let s16 = if shift > 0 { avg >> shift } else { avg << up };
            mono.push(s16.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            frames_done += 1;
        }
        printer.consume(&mono);
    }
    printer.finish();
    let raw = printer.fingerprint().to_vec();
    if raw.is_empty() {
        return Err(AudioError::Unsupported("fingerprint: too short".into()));
    }
    let compressed = FingerprintCompressor::from(&config).compress(&raw);
    let fingerprint = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed);
    let duration_secs = if duration_secs == 0 {
        frames_done / fmt.rate_hz.max(1) as u64
    } else {
        duration_secs
    };
    Ok(Fingerprint {
        fingerprint,
        duration_secs,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(path: &std::path::Path, secs: u32, rate: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        // A few seconds of a chord that changes every half second, so the
        // chroma image is not flat.
        let notes = [261.6_f64, 329.6, 392.0, 523.3, 440.0, 349.2];
        for i in 0..(secs * rate) {
            let t = i as f64 / rate as f64;
            let n = notes[((t * 2.0) as usize) % notes.len()];
            let v = ((t * n * std::f64::consts::TAU).sin() * 0.4
                + (t * n * 1.5 * std::f64::consts::TAU).sin() * 0.2)
                * 32767.0;
            w.write_sample(v as i16).unwrap();
            w.write_sample(v as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn a_file_fingerprints_the_same_way_twice_and_differs_from_another() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        tone(&a, 12, 44_100);
        let f1 = fingerprint_file(&a).unwrap();
        let f2 = fingerprint_file(&a).unwrap();
        assert_eq!(f1, f2);
        assert_eq!(f1.duration_secs, 12);
        assert!(
            !f1.fingerprint.contains('=') && !f1.fingerprint.contains('+') && !f1.fingerprint.contains('/')
        );
        // Chromaprint's compressed form starts with the algorithm byte (1 = TEST2) as base64 "AQ".
        assert!(f1.fingerprint.starts_with("AQ"), "{}", &f1.fingerprint[..8]);
        // The same audio at another rate gives nearly the same fingerprint.
        let b = dir.path().join("b.wav");
        tone(&b, 12, 48_000);
        let g = fingerprint_file(&b).unwrap();
        let n = f1.raw.len().min(g.raw.len());
        let diff: u32 = f1.raw[..n]
            .iter()
            .zip(&g.raw[..n])
            .map(|(x, y)| (x ^ y).count_ones())
            .sum();
        assert!(
            (diff as f64) / (n as f64 * 32.0) < 0.2,
            "bit error rate too high: {diff}/{}",
            n * 32
        );
    }
}
