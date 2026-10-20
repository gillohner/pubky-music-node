//! Parametric EQ (M8): RBJ cookbook biquads run per channel in 64-bit
//! float. Planned only when bands are configured, so the default path stays
//! bit-perfect; shown as an `Enhanced` `eq` stage with the bands in its note.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandKind {
    /// Bell: boost or cut around `freq_hz` with width `q`.
    Peaking,
    LowShelf,
    HighShelf,
    /// Second-order roll-offs; `gain_db` is ignored.
    LowPass,
    HighPass,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub kind: BandKind,
    pub freq_hz: f64,
    #[serde(default)]
    pub gain_db: f64,
    /// Quality factor; 0.7071 is the usual starting point.
    #[serde(default = "default_q")]
    pub q: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_q() -> f64 {
    std::f64::consts::FRAC_1_SQRT_2
}

fn yes() -> bool {
    true
}

impl EqBand {
    pub fn peaking(freq_hz: f64, gain_db: f64, q: f64) -> Self {
        Self {
            kind: BandKind::Peaking,
            freq_hz,
            gain_db,
            q,
            enabled: true,
        }
    }

    /// Reject what cannot be computed: non-positive or super-Nyquist
    /// frequencies, absurd Q, gains beyond ±24 dB.
    pub fn validate(&self, rate_hz: u32) -> Result<(), String> {
        if !(self.freq_hz > 0.0 && self.freq_hz < rate_hz as f64 / 2.0) {
            return Err(format!(
                "band frequency {} Hz is outside (0, {}) Hz",
                self.freq_hz,
                rate_hz / 2
            ));
        }
        if !(self.q > 0.05 && self.q < 50.0) {
            return Err(format!("band Q {} is outside (0.05, 50)", self.q));
        }
        if self.gain_db.is_nan() || self.gain_db.abs() > 24.0 {
            return Err(format!("band gain {} dB is outside ±24 dB", self.gain_db));
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        match self.kind {
            BandKind::Peaking => format!(
                "peak {:.0} Hz {:+.1} dB Q{:.2}",
                self.freq_hz, self.gain_db, self.q
            ),
            BandKind::LowShelf => format!("low shelf {:.0} Hz {:+.1} dB", self.freq_hz, self.gain_db),
            BandKind::HighShelf => format!("high shelf {:.0} Hz {:+.1} dB", self.freq_hz, self.gain_db),
            BandKind::LowPass => format!("low pass {:.0} Hz Q{:.2}", self.freq_hz, self.q),
            BandKind::HighPass => format!("high pass {:.0} Hz Q{:.2}", self.freq_hz, self.q),
        }
    }
}

/// Direct form I biquad coefficients, normalised by a0.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

fn coeffs(band: &EqBand, rate_hz: u32) -> Coeffs {
    let w0 = 2.0 * PI * band.freq_hz / rate_hz as f64;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * band.q);
    let a = 10f64.powf(band.gain_db / 40.0);
    let (b0, b1, b2, a0, a1, a2) = match band.kind {
        BandKind::Peaking => (
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ),
        BandKind::LowShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * cos + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - s),
                (a + 1.0) + (a - 1.0) * cos + s,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - s,
            )
        }
        BandKind::HighShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * cos + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - s),
                (a + 1.0) - (a - 1.0) * cos + s,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - s,
            )
        }
        BandKind::LowPass => (
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        BandKind::HighPass => (
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
    };
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct State {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

/// A cascade of biquads for `channels` interleaved channels.
#[derive(Debug, Clone)]
pub struct Equalizer {
    bands: Vec<(Coeffs, Vec<State>)>,
    channels: usize,
    description: Vec<String>,
}

impl Equalizer {
    pub fn new(bands: &[EqBand], rate_hz: u32, channels: usize) -> Result<Self, String> {
        let mut out = Vec::new();
        let mut description = Vec::new();
        for b in bands.iter().filter(|b| b.enabled) {
            b.validate(rate_hz)?;
            out.push((coeffs(b, rate_hz), vec![State::default(); channels]));
            description.push(b.describe());
        }
        Ok(Self {
            bands: out,
            channels,
            description,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.bands.is_empty()
    }

    pub fn describe(&self) -> String {
        self.description.join(", ")
    }

    /// Filter interleaved f64 samples in place.
    pub fn process(&mut self, samples: &mut [f64]) {
        for (c, state) in &mut self.bands {
            for (i, s) in samples.iter_mut().enumerate() {
                let st = &mut state[i % self.channels];
                let x = *s;
                let y = c.b0 * x + c.b1 * st.x1 + c.b2 * st.x2 - c.a1 * st.y1 - c.a2 * st.y2;
                st.x2 = st.x1;
                st.x1 = x;
                st.y2 = st.y1;
                st.y1 = y;
                *s = y;
            }
        }
    }

    pub fn reset(&mut self) {
        for (_, state) in &mut self.bands {
            for s in state.iter_mut() {
                *s = State::default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms_of_tone(eq: &mut Equalizer, rate: u32, hz: f64) -> f64 {
        let n = rate as usize; // one second
        let mut v: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * hz * i as f64 / rate as f64).sin() * 0.5)
            .collect();
        eq.process(&mut v);
        let tail = &v[n / 2..];
        (tail.iter().map(|x| x * x).sum::<f64>() / tail.len() as f64).sqrt()
    }

    #[test]
    fn peaking_band_boosts_its_centre_and_leaves_far_tones_alone() {
        let band = EqBand::peaking(1000.0, 6.0, 1.0);
        let mut eq = Equalizer::new(&[band], 48_000, 1).unwrap();
        let at = rms_of_tone(&mut eq, 48_000, 1000.0);
        eq.reset();
        let far = rms_of_tone(&mut eq, 48_000, 10_000.0);
        let flat = 0.5 / 2f64.sqrt();
        assert!((20.0 * (at / flat).log10() - 6.0).abs() < 0.2, "{at}");
        assert!((20.0 * (far / flat).log10()).abs() < 0.3, "{far}");
    }

    #[test]
    fn shelves_and_passes_behave() {
        let mut ls = Equalizer::new(
            &[EqBand {
                kind: BandKind::LowShelf,
                freq_hz: 200.0,
                gain_db: -6.0,
                q: std::f64::consts::FRAC_1_SQRT_2,
                enabled: true,
            }],
            48_000,
            2,
        )
        .unwrap();
        let low = rms_of_tone(&mut ls, 48_000, 40.0);
        ls.reset();
        let high = rms_of_tone(&mut ls, 48_000, 8000.0);
        assert!(low < high * 0.6, "low shelf cut: {low} vs {high}");
        let mut hp = Equalizer::new(
            &[EqBand {
                kind: BandKind::HighPass,
                freq_hz: 1000.0,
                gain_db: 0.0,
                q: std::f64::consts::FRAC_1_SQRT_2,
                enabled: true,
            }],
            48_000,
            1,
        )
        .unwrap();
        let sub = rms_of_tone(&mut hp, 48_000, 50.0);
        assert!(sub < 0.01, "high pass should remove 50 Hz: {sub}");
    }

    #[test]
    fn validation_and_disabled_bands() {
        assert!(EqBand::peaking(30_000.0, 0.0, 1.0).validate(48_000).is_err());
        assert!(EqBand::peaking(100.0, 30.0, 1.0).validate(48_000).is_err());
        assert!(EqBand::peaking(100.0, 3.0, 0.01).validate(48_000).is_err());
        let mut off = EqBand::peaking(100.0, 3.0, 1.0);
        off.enabled = false;
        let eq = Equalizer::new(&[off], 48_000, 2).unwrap();
        assert!(eq.is_empty());
        let json = serde_json::to_string(&EqBand::peaking(100.0, 3.0, 1.0)).unwrap();
        assert!(json.contains("\"kind\":\"peaking\""));
    }
}

/// Headphone crossfeed: a little of each channel, low-passed and slightly
/// delayed, bleeds into the other, the way loudspeakers reach both ears.
/// Stereo only; other channel counts pass through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CrossfeedConfig {
    /// Level of the bled signal relative to the direct one, in dB (−3 .. −12 typical).
    #[serde(default = "default_cf_level")]
    pub level_db: f64,
    /// Low-pass corner for the bled signal (the head shadows highs).
    #[serde(default = "default_cf_cutoff")]
    pub cutoff_hz: f64,
    /// Interaural delay in microseconds (about 300 µs for an average head).
    #[serde(default = "default_cf_delay")]
    pub delay_us: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_cf_level() -> f64 {
    -6.0
}
fn default_cf_cutoff() -> f64 {
    700.0
}
fn default_cf_delay() -> f64 {
    300.0
}

impl Default for CrossfeedConfig {
    fn default() -> Self {
        Self {
            level_db: default_cf_level(),
            cutoff_hz: default_cf_cutoff(),
            delay_us: default_cf_delay(),
            enabled: true,
        }
    }
}

impl CrossfeedConfig {
    pub fn validate(&self, rate_hz: u32) -> Result<(), String> {
        if !(-30.0..=0.0).contains(&self.level_db) {
            return Err(format!("crossfeed level {} dB is outside -30..0", self.level_db));
        }
        if !(self.cutoff_hz > 50.0 && self.cutoff_hz < rate_hz as f64 / 2.0) {
            return Err(format!("crossfeed cutoff {} Hz is out of range", self.cutoff_hz));
        }
        if !(0.0..=2000.0).contains(&self.delay_us) {
            return Err(format!("crossfeed delay {} µs is outside 0..2000", self.delay_us));
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        format!(
            "crossfeed {:+.1} dB, {:.0} Hz low-pass, {:.0} µs",
            self.level_db, self.cutoff_hz, self.delay_us
        )
    }
}

/// The running crossfeed filter for a stereo stream.
#[derive(Debug, Clone)]
pub struct Crossfeed {
    gain: f64,
    /// Direct-path scaling so the mono sum keeps its level.
    direct: f64,
    lp: [Equalizer; 2],
    delay: [std::collections::VecDeque<f64>; 2],
    description: String,
}

impl Crossfeed {
    pub fn new(cfg: &CrossfeedConfig, rate_hz: u32) -> Result<Self, String> {
        cfg.validate(rate_hz)?;
        let band = EqBand {
            kind: BandKind::LowPass,
            freq_hz: cfg.cutoff_hz,
            gain_db: 0.0,
            q: std::f64::consts::FRAC_1_SQRT_2,
            enabled: true,
        };
        let lp = [
            Equalizer::new(&[band], rate_hz, 1)?,
            Equalizer::new(&[band], rate_hz, 1)?,
        ];
        let delay_frames = (cfg.delay_us * rate_hz as f64 / 1_000_000.0).round() as usize;
        let gain = 10f64.powf(cfg.level_db / 20.0);
        let mk = || {
            let mut d = std::collections::VecDeque::with_capacity(delay_frames + 1);
            d.extend(std::iter::repeat_n(0.0, delay_frames));
            d
        };
        Ok(Self {
            gain,
            direct: 1.0 / (1.0 + gain),
            lp,
            delay: [mk(), mk()],
            description: cfg.describe(),
        })
    }

    pub fn describe(&self) -> &str {
        &self.description
    }

    /// In place on interleaved stereo f64.
    pub fn process(&mut self, samples: &mut [f64]) {
        let mut one = [0.0f64];
        for frame in samples.as_chunks_mut::<2>().0 {
            let (l, r) = (frame[0], frame[1]);
            // Bleed: the other channel, low-passed, delayed, attenuated.
            one[0] = r;
            self.lp[0].process(&mut one);
            self.delay[0].push_back(one[0]);
            let from_r = self.delay[0].pop_front().unwrap_or(0.0);
            one[0] = l;
            self.lp[1].process(&mut one);
            self.delay[1].push_back(one[0]);
            let from_l = self.delay[1].pop_front().unwrap_or(0.0);
            frame[0] = (l + self.gain * from_r) * self.direct;
            frame[1] = (r + self.gain * from_l) * self.direct;
        }
    }
}

#[cfg(test)]
mod crossfeed_tests {
    use super::*;

    #[test]
    fn hard_panned_signal_bleeds_quietly_into_the_other_side() {
        let mut cf = Crossfeed::new(&CrossfeedConfig::default(), 48_000).unwrap();
        // 200 Hz tone in the left channel only, one second.
        let mut v: Vec<f64> = (0..48_000)
            .flat_map(|i| {
                let x = (2.0 * PI * 200.0 * i as f64 / 48_000.0).sin() * 0.5;
                [x, 0.0]
            })
            .collect();
        cf.process(&mut v);
        let tail = &v[v.len() / 2..];
        let rms = |step: usize| {
            let s: Vec<f64> = tail.iter().skip(step).step_by(2).copied().collect();
            (s.iter().map(|x| x * x).sum::<f64>() / s.len() as f64).sqrt()
        };
        let (left, right) = (rms(0), rms(1));
        let ratio_db = 20.0 * (right / left).log10();
        // At 200 Hz (below the cutoff) the bleed is close to the configured -6 dB.
        assert!((ratio_db + 6.0).abs() < 1.0, "bleed {ratio_db} dB");
        // A centre signal keeps its level: equal channels in, equal and unscaled out.
        let mut c: Vec<f64> = (0..4800).flat_map(|_| [0.25, 0.25]).collect();
        let mut cf = Crossfeed::new(
            &CrossfeedConfig {
                delay_us: 0.0,
                ..Default::default()
            },
            48_000,
        )
        .unwrap();
        cf.process(&mut c);
        let last = c[c.len() - 2];
        assert!((last - 0.25).abs() < 0.01, "centre level {last}");
        assert!(CrossfeedConfig {
            level_db: 3.0,
            ..Default::default()
        }
        .validate(48_000)
        .is_err());
    }
}
