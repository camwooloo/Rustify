//! Spectrum analysis for the visualiser.
//!
//! The window has no access to the audio: playback happens in the daemon, and
//! by the time sound reaches the speakers it has left the process the
//! interface lives in. So the analysis happens here, at the same point the
//! equaliser sits — the one place every decoded sample passes through — and
//! the daemon sends bands to whoever is drawing them.
//!
//! Nothing is computed unless someone is watching. The audio thread checks a
//! single atomic per buffer and returns; a visualiser nobody has open costs
//! one comparison per few thousand samples.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// Samples per analysis window. At 44.1 kHz this is ~46 ms, which is long
/// enough to resolve bass and short enough that the bars still feel attached
/// to the music.
const FFT_SIZE: usize = 2048;

/// How many bars the interface receives. Spaced logarithmically, because
/// hearing is: an even split would spend half the bars above 10 kHz where
/// almost nothing happens.
pub const BANDS: usize = 48;

/// The range mapped across those bars.
const MIN_HZ: f32 = 40.0;
const MAX_HZ: f32 = 16_000.0;
const SAMPLE_RATE: f32 = 44_100.0;

/// Decibel window mapped onto 0..255.
const FLOOR_DB: f32 = -72.0;
const CEIL_DB: f32 = -12.0;

/// How quickly a bar rises and falls. Rising fast and falling slow is what
/// makes a meter look like music rather than noise.
const ATTACK: f32 = 0.55;
const DECAY: f32 = 0.12;

struct Analyser {
    /// Mono samples awaiting a window.
    pending: Vec<f32>,
    window: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    /// Smoothed level per band, 0..1.
    levels: Vec<f32>,
    /// Which band each FFT bin belongs to, precomputed.
    bins: Vec<usize>,
    fft: Arc<dyn rustfft::Fft<f32>>,
}

impl Analyser {
    fn new() -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);

        // Hann, to stop a window's edges ringing across the whole spectrum.
        let window = (0..FFT_SIZE)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();

        // Each usable bin lands in the band its frequency falls into.
        let bins = (0..FFT_SIZE / 2)
            .map(|bin| {
                let hz = bin as f32 * SAMPLE_RATE / FFT_SIZE as f32;
                if hz <= MIN_HZ {
                    return 0;
                }
                let position = (hz / MIN_HZ).ln() / (MAX_HZ / MIN_HZ).ln();
                ((position * BANDS as f32) as usize).min(BANDS - 1)
            })
            .collect();

        Self {
            pending: Vec::with_capacity(FFT_SIZE * 2),
            window,
            scratch: vec![Complex { re: 0.0, im: 0.0 }; FFT_SIZE],
            levels: vec![0.0; BANDS],
            bins,
            fft,
        }
    }

    /// Fold interleaved stereo down to mono and analyse whole windows.
    fn feed(&mut self, samples: &[f64]) {
        for pair in samples.chunks(2) {
            let mono = match pair {
                [l, r] => (*l as f32 + *r as f32) * 0.5,
                [only] => *only as f32,
                _ => continue,
            };
            self.pending.push(mono);
        }

        // Only ever analyse the most recent window: falling behind should
        // drop history rather than queue it, or the bars lag the sound.
        while self.pending.len() >= FFT_SIZE {
            let start = self.pending.len() - FFT_SIZE;
            for (i, sample) in self.pending[start..].iter().enumerate() {
                self.scratch[i] = Complex {
                    re: sample * self.window[i],
                    im: 0.0,
                };
            }
            self.pending.clear();
            self.transform();
        }
    }

    fn transform(&mut self) {
        self.fft.process(&mut self.scratch);

        let mut sums = [0.0f32; BANDS];
        let mut counts = [0u32; BANDS];

        for (bin, band) in self.bins.iter().enumerate() {
            sums[*band] += self.scratch[bin].norm();
            counts[*band] += 1;
        }

        for band in 0..BANDS {
            let mean = if counts[band] == 0 {
                0.0
            } else {
                sums[band] / counts[band] as f32
            };

            // Into decibels, then onto 0..1 across the window worth showing.
            let db = 20.0 * (mean.max(1e-9) / FFT_SIZE as f32).log10();
            let level = ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0);

            let previous = self.levels[band];
            let rate = if level > previous { ATTACK } else { DECAY };
            self.levels[band] = previous + (level - previous) * rate;
        }
    }

    fn bands(&self) -> Vec<u8> {
        self.levels
            .iter()
            .map(|level| (level * 255.0).clamp(0.0, 255.0) as u8)
            .collect()
    }
}

/// Handle shared between the audio thread and whoever is reading bands.
#[derive(Clone)]
pub struct Spectrum {
    watching: Arc<AtomicBool>,
    analyser: Arc<Mutex<Analyser>>,
}

impl Spectrum {
    pub fn new() -> Self {
        Self {
            watching: Arc::new(AtomicBool::new(false)),
            analyser: Arc::new(Mutex::new(Analyser::new())),
        }
    }

    /// Turn analysis on or off. Off is the default and costs nothing.
    pub fn watch(&self, on: bool) {
        self.watching.store(on, Ordering::Relaxed);
        if !on {
            if let Ok(mut analyser) = self.analyser.lock() {
                analyser.pending.clear();
                analyser.levels.iter_mut().for_each(|l| *l = 0.0);
            }
        }
    }

    pub fn watching(&self) -> bool {
        self.watching.load(Ordering::Relaxed)
    }

    /// Called from the audio thread for every buffer on its way out.
    pub fn feed(&self, samples: &[f64]) {
        if !self.watching() {
            return;
        }
        // `try_lock`: the reader holds this for microseconds, and skipping a
        // window is better than making the audio thread wait for anything.
        if let Ok(mut analyser) = self.analyser.try_lock() {
            analyser.feed(samples);
        }
    }

    /// The current bars, 0..255 each.
    pub fn bands(&self) -> Vec<u8> {
        match self.analyser.lock() {
            Ok(analyser) => analyser.bands(),
            Err(poisoned) => poisoned.into_inner().bands(),
        }
    }
}

impl Default for Spectrum {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `seconds` of a sine at `hz`, interleaved stereo.
    fn tone(hz: f32, seconds: f32) -> Vec<f64> {
        let frames = (SAMPLE_RATE * seconds) as usize;
        let mut out = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let t = n as f32 / SAMPLE_RATE;
            let value = (0.5 * (2.0 * std::f32::consts::PI * hz * t).sin()) as f64;
            out.push(value);
            out.push(value);
        }
        out
    }

    /// Which band a frequency should land in, by the same mapping.
    fn band_of(hz: f32) -> usize {
        let position = (hz / MIN_HZ).ln() / (MAX_HZ / MIN_HZ).ln();
        ((position * BANDS as f32) as usize).min(BANDS - 1)
    }

    #[test]
    fn nothing_is_analysed_while_nobody_is_watching() {
        let spectrum = Spectrum::new();
        spectrum.feed(&tone(1000.0, 0.2));
        assert!(spectrum.bands().iter().all(|b| *b == 0));
    }

    #[test]
    fn a_tone_lights_its_own_band() {
        let spectrum = Spectrum::new();
        spectrum.watch(true);

        // Several windows, so the smoothing has time to rise.
        for _ in 0..12 {
            spectrum.feed(&tone(1000.0, 0.05));
        }

        let bands = spectrum.bands();
        let expected = band_of(1000.0);
        let loudest = bands
            .iter()
            .enumerate()
            .max_by_key(|(_, level)| **level)
            .map(|(i, _)| i)
            .unwrap();

        assert!(
            loudest.abs_diff(expected) <= 1,
            "1 kHz should light band {expected}, lit {loudest}: {bands:?}"
        );
        assert!(bands[expected] > 100, "and it should be loud: {bands:?}");
    }

    #[test]
    fn bass_and_treble_land_at_opposite_ends() {
        assert!(band_of(60.0) < 6);
        assert!(band_of(12_000.0) > BANDS - 8);
    }

    #[test]
    fn silence_falls_back_to_nothing() {
        let spectrum = Spectrum::new();
        spectrum.watch(true);
        for _ in 0..12 {
            spectrum.feed(&tone(1000.0, 0.05));
        }
        assert!(spectrum.bands().iter().any(|b| *b > 50));

        // Enough silent windows for the decay to run down.
        let quiet = vec![0.0f64; 4096];
        for _ in 0..80 {
            spectrum.feed(&quiet);
        }
        assert!(
            spectrum.bands().iter().all(|b| *b < 30),
            "bars should fall when the music stops: {:?}",
            spectrum.bands()
        );
    }

    #[test]
    fn switching_off_clears_what_was_showing() {
        let spectrum = Spectrum::new();
        spectrum.watch(true);
        for _ in 0..12 {
            spectrum.feed(&tone(440.0, 0.05));
        }
        spectrum.watch(false);
        assert!(spectrum.bands().iter().all(|b| *b == 0));
    }
}
