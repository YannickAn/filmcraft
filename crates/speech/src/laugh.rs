//! Laughter in the pauses between words.
//!
//! Deleting a pause cuts what lies between two words: a cough, a breath, a click or room noise
//! there goes with it. A laugh is part of what was said and stays ([`laughs`]): the Text panel cuts
//! the silence on both sides of it (`filmcraft_edit::transcript`).
//!
//! A laugh differs from other sound in a pause by two things a cough or a breath lacks:
//!
//! - it is **voiced**: the vocal folds vibrate as on a vowel, so the waveform repeats with a pitch
//!   period of 1.25–14 ms (about 70–800 Hz, a laugh goes high; normalised autocorrelation of at
//!   least 0.5 over 30 ms);
//! - it comes in **pulses** ("ha-ha-ha"): level peaks at least 6 dB above the dips on both sides,
//!   100–400 ms apart.
//!
//! A run of sound in a gap between words (frames above the edge threshold of
//! [`crate::vad::edge_threshold`], dips of up to 150 ms bridged, peaking above the speech threshold)
//! is a laugh when two consecutive pulses in it are voiced and that close together. A cough is a
//! burst of noise (unvoiced; in a fit, the bursts are further apart), a breath is quiet noise, a
//! click is a single short burst. A single voiced "hm" or "ha" is one pulse and stays part of the
//! pause; the recogniser usually writes those as words anyway.

use filmcraft_project::Word;
use filmcraft_time::{Tick, TimeRange};

use crate::TICKS_PER_SAMPLE;
use crate::vad::{edge_threshold, frame_db, threshold};

/// 10 ms frames of 16 kHz audio.
const FRAME: usize = 160;
/// Gaps shorter than this (frames) hold no laugh to keep.
const MIN_GAP_FRAMES: usize = 20;
/// Dips between the pulses of one laugh may stay below the edge threshold this long (frames).
const BRIDGE_FRAMES: usize = 15;
/// A pulse rises and falls at least this much (dB) against the dips on both sides.
const PULSE_PROMINENCE_DB: f32 = 6.0;
/// Consecutive pulses of a laugh lie 100–400 ms apart (frames).
const MIN_PULSE_GAP: usize = 10;
const MAX_PULSE_GAP: usize = 40;
/// Normalised autocorrelation at the pitch period from which a pulse counts as voiced.
const VOICED: f32 = 0.5;
/// Pitch periods looked at: 800 Hz down to about 70 Hz (samples).
const MIN_LAG: usize = 20;
const MAX_LAG: usize = 228;
/// The stretch of waveform compared with itself one period later (30 ms).
const WINDOW: usize = 480;

/// The laughs in the gaps between `words` (and before the first and after the last word) of
/// `audio` (16 kHz mono), in media time, in order. See the module docs.
pub fn laughs(audio: &[f32], words: &[Word]) -> Vec<TimeRange> {
    let db = frame_db(audio);
    if db.is_empty() {
        return Vec::new();
    }
    let (th, edge) = (threshold(&db), edge_threshold(&db));
    let frame_ticks = TICKS_PER_SAMPLE * FRAME as i64;
    let n = db.len();
    let floor_frame = |t: Tick| usize::try_from(t.0.max(0) / frame_ticks).unwrap_or(usize::MAX).min(n);
    let ceil_frame = |t: Tick| usize::try_from(t.0.max(0).saturating_add(frame_ticks - 1) / frame_ticks).unwrap_or(usize::MAX).min(n);
    // the gaps as frame ranges: before the first word, between words, after the last
    let mut gaps = Vec::new();
    let mut from = 0usize;
    for w in words {
        gaps.push((from, floor_frame(w.start)));
        from = from.max(ceil_frame(w.end));
    }
    gaps.push((from, n));
    let tick = |f: usize| Tick(i64::try_from(f).unwrap_or(i64::MAX).saturating_mul(frame_ticks));
    let mut out = Vec::new();
    for (a, b) in gaps {
        if b < a.saturating_add(MIN_GAP_FRAMES) {
            continue;
        }
        for (s, e) in sound_runs(&db, a, b, edge) {
            let peak = db.get(s..e).into_iter().flatten().copied().fold(f32::NEG_INFINITY, f32::max);
            if peak >= th && is_laugh(audio, &db, s, e) {
                out.push(TimeRange::from_bounds(tick(s), tick(e)));
            }
        }
    }
    out
}

/// Runs of frames `a..b` at or above `edge`, joined across dips of up to [`BRIDGE_FRAMES`].
fn sound_runs(db: &[f32], a: usize, b: usize, edge: f32) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for f in a..=b {
        let loud = f < b && db.get(f).is_some_and(|v| *v >= edge);
        match (loud, start) {
            (true, None) => start = Some(f),
            (false, Some(s)) => {
                match runs.last_mut() {
                    Some(last) if s.saturating_sub(last.1) <= BRIDGE_FRAMES => last.1 = f,
                    _ => runs.push((s, f)),
                }
                start = None;
            }
            _ => {}
        }
    }
    runs
}

/// Whether frames `s..e` hold two consecutive voiced pulses 100–400 ms apart.
fn is_laugh(audio: &[f32], db: &[f32], s: usize, e: usize) -> bool {
    let pulses = pulses(db, s, e);
    let voiced: Vec<bool> = pulses.iter().map(|&f| voiced_near(audio, f)).collect();
    pulses.windows(2).zip(voiced.windows(2)).any(|(p, v)| {
        let gap = p.get(1).copied().unwrap_or(0).saturating_sub(p.first().copied().unwrap_or(0));
        v.iter().all(|x| *x) && (MIN_PULSE_GAP..=MAX_PULSE_GAP).contains(&gap)
    })
}

/// The frames of the level peaks in `s..e` that stand at least [`PULSE_PROMINENCE_DB`] above the
/// dips on both sides (the level smoothed over 30 ms; the run's ends count as dips).
fn pulses(db: &[f32], s: usize, e: usize) -> Vec<usize> {
    let level = |f: usize| {
        let lo = f.saturating_sub(1).max(s);
        let hi = f.saturating_add(2).min(e);
        let v = db.get(lo..hi).unwrap_or_default();
        if v.is_empty() { f32::NEG_INFINITY } else { v.iter().sum::<f32>() / v.len() as f32 }
    };
    let mut out = Vec::new();
    let (mut max, mut max_at) = (f32::NEG_INFINITY, s);
    let mut min = f32::INFINITY;
    let mut rising = true;
    // one frame past the run: below everything, so the last peak is closed
    for f in s..=e {
        let v = if f < e { level(f) } else { f32::NEG_INFINITY };
        if v > max {
            (max, max_at) = (v, f);
        }
        if v < min {
            min = v;
        }
        if rising {
            if v < max - PULSE_PROMINENCE_DB && max - PULSE_PROMINENCE_DB >= min.min(v) {
                out.push(max_at);
                (min, rising) = (v, false);
            }
        } else if v > min + PULSE_PROMINENCE_DB {
            (max, max_at, rising) = (v, f, true);
        }
    }
    out
}

/// Whether the waveform around frame `f` (or the frames next to it) is voiced.
fn voiced_near(audio: &[f32], f: usize) -> bool {
    [f.saturating_sub(1), f, f.saturating_add(1)].iter().any(|&g| periodicity(audio, g.saturating_mul(FRAME).saturating_add(FRAME / 2)) >= VOICED)
}

/// The best normalised autocorrelation of the 30 ms of `audio` around sample `centre` with itself
/// one pitch period (1.25–14 ms) later; 0 where too little audio is left.
fn periodicity(audio: &[f32], centre: usize) -> f32 {
    let start = centre.saturating_sub(WINDOW / 2);
    let Some(x) = audio.get(start..start.saturating_add(WINDOW + MAX_LAG)) else { return 0.0 };
    let mean = x.iter().sum::<f32>() / x.len().max(1) as f32;
    let x: Vec<f32> = x.iter().map(|v| v - mean).collect();
    let Some(head) = x.get(..WINDOW) else { return 0.0 };
    let e0: f32 = head.iter().map(|v| v * v).sum();
    let mut best = 0.0f32;
    for lag in MIN_LAG..=MAX_LAG {
        let Some(later) = x.get(lag..lag + WINDOW) else { break };
        let (mut num, mut e1) = (0.0f32, 0.0f32);
        for (a, b) in head.iter().zip(later) {
            num += a * b;
            e1 += b * b;
        }
        let den = (e0 * e1).sqrt();
        if den > 1e-12 {
            best = best.max(num / den);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seconds_tick;

    const SR: usize = 16_000;

    fn at(s: f64) -> usize {
        (s * SR as f64) as usize
    }

    /// Room noise: pseudo-random, about −65 dBFS.
    fn room(len: usize) -> Vec<f32> {
        let mut x: u32 = 12_345;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002
            })
            .collect()
    }

    /// White noise of amplitude `amp` (another seed).
    fn noise(len: usize, amp: f32, seed: u32) -> Vec<f32> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(22_695_477).wrapping_add(1);
                ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 2.0 * amp
            })
            .collect()
    }

    /// A voice at `f0` Hz (ten harmonics), amplitude `amp`, under the envelope `env(0..1)`.
    fn voice(len: usize, f0: f32, amp: f32, env: impl Fn(f32) -> f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / SR as f32;
                let v: f32 = (1..=10).map(|k| (2.0 * std::f32::consts::PI * f0 * k as f32 * t).sin() / k as f32).sum();
                amp * 0.6 * v * env(i as f32 / len as f32)
            })
            .collect()
    }

    fn hann(x: f32) -> f32 {
        (std::f32::consts::PI * x).sin().powi(2)
    }

    fn add(audio: &mut [f32], from: usize, part: &[f32]) {
        for (a, p) in audio.iter_mut().skip(from).zip(part) {
            *a += p;
        }
    }

    /// "Ha-ha-ha-ha": four voiced pulses 200 ms apart with breathy "h" noise between them.
    fn laugh(audio: &mut [f32], from: usize) -> usize {
        for k in 0..4 {
            let p = from + k * at(0.2);
            add(audio, p, &noise(at(0.04), 0.01, 7 + k as u32));
            add(audio, p + at(0.03), &voice(at(0.12), 240.0, 0.25, hann));
        }
        from + 3 * at(0.2) + at(0.15)
    }

    /// A cough: a loud burst of noise, dying away over 200 ms.
    fn cough(audio: &mut [f32], from: usize) {
        let n = at(0.2);
        let burst: Vec<f32> = noise(n, 0.5, 99).iter().enumerate().map(|(i, v)| v * (-(i as f32) / (0.04 * SR as f32)).exp()).collect();
        add(audio, from, &burst);
    }

    /// Words as voiced speech (amplitude 0.3) with their transcript.
    fn speak(audio: &mut [f32], words: &[(f64, f64)]) -> Vec<Word> {
        words
            .iter()
            .map(|&(a, b)| {
                add(audio, at(a), &voice(at(b - a), 140.0, 0.3, |x| (x * 20.0).min(1.0).min((1.0 - x) * 20.0)));
                Word::new("w", seconds_tick(a), seconds_tick(b))
            })
            .collect()
    }

    fn near(r: &TimeRange, a: f64, b: f64) -> bool {
        (r.start.seconds() - a).abs() < 0.05 && (r.end().seconds() - b).abs() < 0.05
    }

    #[test]
    fn a_laugh_between_words_is_found_and_a_cough_a_breath_and_a_click_are_not() {
        let mut audio = room(at(7.0));
        let words = speak(&mut audio, &[(0.3, 1.0), (2.6, 3.2), (5.6, 6.4)]);
        // between the first two words: a laugh 1.4–2.15
        let end = laugh(&mut audio, at(1.4));
        assert_eq!(end, at(2.15));
        // between the next two: a cough at 3.5, a breath 4.1–4.6, a click at 5.0
        cough(&mut audio, at(3.5));
        add(&mut audio, at(4.1), &noise(at(0.5), 0.01, 5));
        add(&mut audio, at(5.0), &[0.6, -0.5, 0.4, -0.3, 0.2]);
        let found = laughs(&audio, &words);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(near(&found[0], 1.4, 2.15), "{found:?}");
        // the laugh is four voiced pulses
        let db = frame_db(&audio);
        let p = pulses(&db, 140, 215);
        assert_eq!(p.len(), 4, "{p:?}");
        assert!(p.iter().all(|&f| voiced_near(&audio, f)));
    }

    #[test]
    fn a_single_hm_and_a_coughing_fit_are_no_laugh() {
        let mut audio = room(at(5.0));
        let words = speak(&mut audio, &[(0.2, 0.8), (4.2, 4.8)]);
        // a voiced "hm" of 300 ms: one pulse
        add(&mut audio, at(1.2), &voice(at(0.3), 120.0, 0.2, hann));
        // three coughs 250 ms apart: pulses at a laugh's pace, but noise
        for k in 0..3 {
            cough(&mut audio, at(2.2) + k * at(0.25));
        }
        assert_eq!(laughs(&audio, &words), Vec::<TimeRange>::new());
        // the fit has a laugh's pace: only the missing voice tells it apart
        let db = frame_db(&audio);
        let runs = sound_runs(&db, 85, 415, edge_threshold(&db));
        let fit = runs.iter().find(|r| r.0 >= 215).copied().unwrap();
        let p = pulses(&db, fit.0, fit.1);
        assert_eq!(p.len(), 3, "{p:?}");
        assert!(p.iter().all(|&f| !voiced_near(&audio, f)));
        let hm = runs.iter().find(|r| r.0 < 215).copied().unwrap();
        assert_eq!(pulses(&db, hm.0, hm.1).len(), 1);
        assert!(voiced_near(&audio, pulses(&db, hm.0, hm.1)[0]));
    }

    #[test]
    fn a_laugh_before_the_first_word_counts_and_no_input_is_safe() {
        let mut audio = room(at(3.0));
        laugh(&mut audio, at(0.3));
        let words = speak(&mut audio, &[(1.5, 2.5)]);
        let found = laughs(&audio, &words);
        assert_eq!(found.len(), 1, "{found:?}");
        // hostile input: nothing, words past the end, unsorted and overlapping words
        assert!(laughs(&[], &words).is_empty());
        let odd = vec![
            Word::new("x", seconds_tick(9.0), seconds_tick(1.0)),
            Word::new("y", Tick(-5), Tick(i64::MAX)),
            Word::new("z", seconds_tick(0.5), seconds_tick(0.6)),
        ];
        let _ = laughs(&audio, &odd);
        assert_eq!(periodicity(&[0.0; 10], 5), 0.0);
        assert!(pulses(&[], 0, 0).is_empty());
    }
}
