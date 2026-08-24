//! A five-band equaliser, applied between the decoder and the audio device.
//!
//! Spotify's desktop client has no equaliser at all, so this is one of the
//! few places Rustify can simply be better rather than equal. The bands match
//! the ones Spotify uses on mobile — 60 Hz, 230 Hz, 910 Hz, 3.6 kHz, 14 kHz —
//! so a setting someone already knows carries over.
//!
//! Each band is a peaking biquad from the RBJ cookbook, run per channel over
//! the interleaved samples librespot hands to the sink. At flat gain the
//! filters are bypassed entirely rather than run with unity coefficients: the
//! common case should cost nothing.

use std::sync::{Arc, RwLock};

/// Centre frequencies, in hertz.
pub const BANDS: [f32; 5] = [60.0, 230.0, 910.0, 3600.0, 14000.0];

/// How far a band may be pushed, in decibels, either way.
pub const MAX_GAIN_DB: f32 = 12.0;

/// Everything Spotify decodes arrives at this rate.
const SAMPLE_RATE: f32 = 44_100.0;

/// Bandwidth of each peaking filter. A little under one octave, which keeps
/// neighbouring bands from stacking into a much larger boost than either
/// slider claims to apply.
const Q: f32 = 1.1;

/// One biquad section: coefficients plus the two samples of history it needs.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Peaking EQ, from the RBJ audio cookbook.
    fn peaking(freq: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * Q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            ..Default::default()
        }
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;

        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Forget history, so a change of coefficients cannot ring.
    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// The gains, shared between whoever sets them and the audio thread.
#[derive(Debug, Clone, Default)]
pub struct EqSettings {
    pub enabled: bool,
    pub gains: Vec<f32>,
    /// Bumped on every change so the filter knows to rebuild its coefficients
    /// without comparing float arrays on the audio thread.
    pub revision: u64,
}

impl EqSettings {
    fn is_flat(&self) -> bool {
        !self.enabled || self.gains.iter().all(|g| g.abs() < 0.05)
    }
}

/// Handle held by the daemon, read by the sink.
pub type SharedEq = Arc<RwLock<EqSettings>>;

pub fn shared() -> SharedEq {
    Arc::new(RwLock::new(EqSettings {
        enabled: false,
        gains: vec![0.0; BANDS.len()],
        revision: 0,
    }))
}

/// Replace the gains, clamped to what the sliders can express.
pub fn set(shared: &SharedEq, enabled: bool, gains: &[f32]) {
    let mut settings = match shared.write() {
        Ok(settings) => settings,
        // A poisoned lock means a panic on the audio thread; the sound
        // carrying on unequalised beats taking playback down with it.
        Err(poisoned) => poisoned.into_inner(),
    };

    settings.enabled = enabled;
    settings.gains = BANDS
        .iter()
        .enumerate()
        .map(|(i, _)| {
            gains
                .get(i)
                .copied()
                .unwrap_or(0.0)
                .clamp(-MAX_GAIN_DB, MAX_GAIN_DB)
        })
        .collect();
    settings.revision = settings.revision.wrapping_add(1);
}

/// The filter itself: one chain of biquads per channel.
pub struct Equaliser {
    channels: Vec<Vec<Biquad>>,
    revision: u64,
    bypass: bool,
    shared: SharedEq,
}

impl Equaliser {
    pub fn new(shared: SharedEq, channels: usize) -> Self {
        Self {
            channels: vec![vec![Biquad::default(); BANDS.len()]; channels.max(1)],
            revision: u64::MAX,
            bypass: true,
            shared,
        }
    }

    /// Pick up any change to the gains. Cheap enough to call per packet: it
    /// reads a lock and compares one integer.
    fn sync(&mut self) {
        let settings = match self.shared.read() {
            Ok(settings) => settings.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };

        if settings.revision == self.revision {
            return;
        }
        self.revision = settings.revision;
        self.bypass = settings.is_flat();

        for chain in &mut self.channels {
            for (band, filter) in chain.iter_mut().enumerate() {
                *filter = Biquad::peaking(
                    BANDS[band],
                    settings.gains.get(band).copied().unwrap_or(0.0),
                );
                filter.reset();
            }
        }
    }

    /// Filter one interleaved buffer in place.
    pub fn process(&mut self, samples: &mut [f64]) {
        self.sync();
        if self.bypass {
            return;
        }

        let channels = self.channels.len();
        for (i, sample) in samples.iter_mut().enumerate() {
            let chain = &mut self.channels[i % channels];
            let mut value = *sample as f32;
            for filter in chain.iter_mut() {
                value = filter.run(value);
            }
            // Boosting can push past full scale, and what leaves here goes
            // straight to the device: clipping is audible, wrapping is worse.
            *sample = value.clamp(-1.0, 1.0) as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq_with(gains: &[f32]) -> Equaliser {
        let shared = shared();
        set(&shared, true, gains);
        Equaliser::new(shared, 2)
    }

    /// A sine at `freq`, and the peak level of it after filtering.
    fn peak_after(eq: &mut Equaliser, freq: f32) -> f64 {
        let mut samples: Vec<f64> = (0..8820)
            .map(|n| {
                let t = n as f32 / 2.0 / SAMPLE_RATE;
                (0.4 * (2.0 * std::f32::consts::PI * freq * t).sin()) as f64
            })
            .collect();

        eq.process(&mut samples);
        // The first samples are the filter settling; measure after that.
        samples[2000..].iter().fold(0.0f64, |a, s| a.max(s.abs()))
    }

    #[test]
    fn flat_gains_leave_the_audio_alone() {
        let mut eq = eq_with(&[0.0; 5]);
        let original: Vec<f64> = (0..64).map(|n| (n as f64 / 64.0) - 0.5).collect();
        let mut samples = original.clone();
        eq.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn a_disabled_equaliser_is_a_bypass() {
        let shared = shared();
        set(&shared, false, &[12.0; 5]);
        let mut eq = Equaliser::new(shared, 2);
        let mut samples = vec![0.25f64; 32];
        eq.process(&mut samples);
        assert!(samples.iter().all(|s| (*s - 0.25).abs() < f64::EPSILON));
    }

    #[test]
    fn boosting_a_band_raises_that_band() {
        let quiet = peak_after(&mut eq_with(&[0.0; 5]), 910.0);
        let loud = peak_after(&mut eq_with(&[0.0, 0.0, 10.0, 0.0, 0.0]), 910.0);
        assert!(
            loud > quiet * 1.5,
            "910 Hz should be much louder with +10 dB: {quiet} -> {loud}"
        );
    }

    #[test]
    fn cutting_a_band_lowers_only_that_band() {
        let mut cut = eq_with(&[0.0, 0.0, -12.0, 0.0, 0.0]);
        let mid = peak_after(&mut cut, 910.0);
        let mut same = eq_with(&[0.0, 0.0, -12.0, 0.0, 0.0]);
        let untouched = peak_after(&mut same, 60.0);

        assert!(mid < 0.25, "the cut band should drop: {mid}");
        assert!(untouched > 0.3, "a band left alone should not: {untouched}");
    }

    #[test]
    fn gains_are_clamped_to_the_range_the_sliders_offer() {
        let shared = shared();
        set(&shared, true, &[99.0, -99.0, 0.0, 0.0, 0.0]);
        let settings = shared.read().unwrap();
        assert_eq!(settings.gains[0], MAX_GAIN_DB);
        assert_eq!(settings.gains[1], -MAX_GAIN_DB);
    }

    #[test]
    fn a_short_buffer_of_missing_gains_still_fills_every_band() {
        let shared = shared();
        set(&shared, true, &[3.0]);
        let settings = shared.read().unwrap();
        assert_eq!(settings.gains.len(), BANDS.len());
        assert_eq!(settings.gains[4], 0.0);
    }
}
