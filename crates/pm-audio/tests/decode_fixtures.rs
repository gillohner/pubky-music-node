//! Decoder tests against generated WAVs and the CC0 FLAC fixtures.

use pm_audio::{probe_file, Decoder};
use pm_model::{AudioFormat, Samples};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn write_wav(path: &Path, bits: u16, rate: u32, channels: u16, frames: usize) -> Vec<i32> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: bits,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let fs = 1i64 << (bits - 1);
    let mut v = Vec::new();
    for i in 0..frames * channels as usize {
        let s = match i {
            0 => fs - 1,
            1 => -fs,
            2 => 1,
            3 => -1,
            _ => ((i as i64 * 104_729) % (2 * fs)) - fs,
        } as i32;
        v.push(s);
        if bits == 16 {
            w.write_sample(s as i16).unwrap();
        } else {
            w.write_sample(s).unwrap();
        }
    }
    w.finalize().unwrap();
    v
}

#[test]
fn wav_decodes_sample_exact() {
    for (bits, rate, ch) in [
        (16u16, 44_100u32, 2u16),
        (24, 96_000, 2),
        (32, 192_000, 2),
        (24, 48_000, 1),
        (16, 44_100, 6),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        let expect = write_wav(&p, bits, rate, ch, 5000);
        let mut d = Decoder::open(&p).unwrap();
        assert_eq!(d.container(), "wav");
        assert_eq!(d.format(), AudioFormat::pcm(rate, bits as u8, ch as u8));
        assert_eq!(d.duration_ms(), Some(5000 * 1000 / rate as u64));
        let all = d.decode_all().unwrap();
        assert_eq!(all.format, AudioFormat::pcm(rate, bits as u8, ch as u8));
        assert_eq!(
            all.samples,
            Samples::PcmI32(expect),
            "bits={bits} rate={rate} ch={ch}"
        );
        let info = probe_file(&p).unwrap();
        assert_eq!(info.container, "wav");
        assert_eq!(info.format, d.format());
    }
}

#[test]
fn wav_seek_is_sample_accurate() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.wav");
    let rate = 48_000;
    let expect = write_wav(&p, 24, rate, 2, 48_000);
    let mut d = Decoder::open(&p).unwrap();
    d.seek_ms(250).unwrap();
    let rest = d.decode_all().unwrap();
    let skip_frames = rate as usize / 4;
    assert_eq!(rest.samples, Samples::PcmI32(expect[skip_frames * 2..].to_vec()));
    // Seek back to the start after reaching the end.
    d.seek_ms(0).unwrap();
    let all = d.decode_all().unwrap();
    assert_eq!(all.samples, Samples::PcmI32(expect));
}

fn decode_flac_verified(name: &str) -> (AudioFormat, Vec<i32>, Option<u64>) {
    let mut d = Decoder::open(fixture(name)).unwrap();
    assert_eq!(d.container(), "flac");
    let fmt = d.format();
    let all = d.decode_all().unwrap();
    assert_eq!(d.finalize(), Some(true), "{name}: STREAMINFO MD5 must verify");
    let Samples::PcmI32(v) = all.samples else { panic!() };
    (fmt, v, d.duration_ms())
}

#[test]
fn flac_16_bit_decodes_bit_exact() {
    let (fmt, v, dur) = decode_flac_verified("61_predictor_overflow_check_16-bit.flac");
    assert_eq!(fmt.bits, 16);
    assert!(
        v.iter().all(|s| (-32_768..=32_767).contains(s)),
        "right-justified 16-bit range"
    );
    assert!(
        v.iter().any(|s| *s == 32_767 || *s == -32_768),
        "predictor overflow file hits full scale"
    );
    let frames = v.len() / fmt.channels as usize;
    assert_eq!(dur, Some(frames as u64 * 1000 / fmt.rate_hz as u64));
}

#[test]
fn flac_24_bit_decodes_bit_exact() {
    let (fmt, v, _) = decode_flac_verified("63_predictor_overflow_check_24-bit.flac");
    assert_eq!(fmt.bits, 24);
    assert!(
        v.iter().all(|s| (-8_388_608..=8_388_607).contains(s)),
        "right-justified 24-bit range"
    );
    assert!(
        v.iter().any(|s| *s == 8_388_607 || *s == -8_388_608),
        "predictor overflow file hits full scale"
    );
    // Low byte is populated: the samples are not 16-bit values padded to 24.
    assert!(v.iter().any(|s| s & 0xff != 0));
}

#[test]
fn flac_mono_and_wasted_bits() {
    let (fmt, v, _) = decode_flac_verified("60_mono_audio.flac");
    assert_eq!(fmt.channels, 1);
    assert!(!v.is_empty());
    let (fmt, v, _) = decode_flac_verified("14_wasted_bits.flac");
    assert_eq!(fmt.channels, 2);
    assert!(!v.is_empty());
    let info = probe_file(fixture("14_wasted_bits.flac")).unwrap();
    assert_eq!(info.container, "flac");
    assert_eq!(info.format, fmt);
    assert!(info.duration_ms.unwrap() > 0);
}

#[test]
fn flac_seek_then_md5_still_matches_full_decode() {
    // Seeking changes what we read, so compare against a full decode instead of the MD5.
    let (fmt, full, _) = decode_flac_verified("63_predictor_overflow_check_24-bit.flac");
    let mut d = Decoder::open(fixture("63_predictor_overflow_check_24-bit.flac")).unwrap();
    d.seek_ms(100).unwrap();
    let rest = d.decode_all().unwrap();
    let skip = (fmt.rate_hz as usize / 10) * fmt.channels as usize;
    assert_eq!(rest.samples, Samples::PcmI32(full[skip..].to_vec()));
}

#[test]
fn unsupported_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.flac");
    std::fs::write(&p, b"not audio at all").unwrap();
    assert!(Decoder::open(&p).is_err());
    assert!(Decoder::open(dir.path().join("missing.flac")).is_err());
}
