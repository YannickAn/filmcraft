//! Energy-based tightening of word bounds.
//!
//! Attention alignment places every word boundary on the next word's start, so silence between
//! words is absorbed into the word before it (and leading silence into the first word). Pause
//! detection and text-based editing need the silences, so each word's start and end are moved
//! inward past 10 ms frames whose level is below a threshold set between the clip's noise floor
//! (10th percentile of the levels of frames above −100 dBFS) and its speech level (95th percentile): 30 % of the way up,
//! and at least 6 dB above the floor. A word keeps at least 60 ms.
//!
//! A click in the pause (a mouse, a key, a tap on the desk) is louder than the threshold too, but
//! short: sound shorter than 40 ms more than 150 ms away from longer sound is no speech
//! ([`speech_frames`]), so it no longer holds the word's edge out in the pause. A final consonant
//! released after its closure (the t of "ist") is closer to its vowel than that and stays.
//!
//! A soft consonant at a word's edge (the s of "so", the f of "für", an h, a fading end) is well
//! below the threshold, which is set for vowels: on its own the threshold moved the word's start
//! past it, and deleting the pause before the word cut its start off. So speech grows outward from
//! the frames above the threshold into adjacent frames above a lower edge threshold
//! ([`edge_threshold`]: 12 % of the way from the floor to the speech level, at least 4.5 dB above
//! the floor), by at most 200 ms on each side ([`grow_speech`]).

use filmcraft_project::Word;

use crate::{SAMPLE_RATE, TICKS_PER_SAMPLE};

const FRAME: usize = (SAMPLE_RATE / 100) as usize;
const MIN_WORD_FRAMES: usize = 6;
/// Sound at least this long (10 ms frames) is speech...
const MIN_SPEECH_RUN: usize = 4;
/// ...and so is shorter sound this close to it.
const NEAR_SPEECH: usize = 15;
/// How far (10 ms frames) speech grows into a soft edge ([`grow_speech`]).
const MAX_EDGE_FRAMES: usize = 20;

/// Frame levels in dBFS (10 ms RMS).
pub fn frame_db(audio: &[f32]) -> Vec<f32> {
    audio
        .chunks(FRAME)
        .map(|c| {
            let e = c.iter().map(|v| v * v).sum::<f32>() / c.len().max(1) as f32;
            10.0 * (e + 1e-12).log10()
        })
        .collect()
}

/// The noise floor (10th percentile) and the speech level (95th percentile) of frame levels, in
/// dBFS; None without a frame above digital silence.
fn levels(db: &[f32]) -> Option<(f32, f32)> {
    // digital silence (< −100 dBFS) would drag the floor far below the recording's own noise
    let mut s: Vec<f32> = db.iter().copied().filter(|v| *v > -100.0).collect();
    if s.is_empty() {
        return None;
    }
    s.sort_by(f32::total_cmp);
    let q = |p: f32| s.get(((s.len() - 1) as f32 * p) as usize).copied().unwrap_or(-60.0);
    Some((q(0.10), q(0.95)))
}

/// The speech/silence threshold in dBFS.
pub fn threshold(db: &[f32]) -> f32 {
    match levels(db) {
        Some((floor, speech)) => (floor + 0.3 * (speech - floor)).max(floor + 6.0),
        None => -60.0,
    }
}

/// The level in dBFS a frame next to speech needs to belong to it: a soft consonant at a word's
/// edge (see the module docs). Below [`threshold`].
pub fn edge_threshold(db: &[f32]) -> f32 {
    match levels(db) {
        Some((floor, speech)) => (floor + 0.12 * (speech - floor)).max(floor + 4.5).min(threshold(db)),
        None => -66.0,
    }
}

/// Grow `speech` (per 10 ms frame) into the adjacent frames at or above `edge` (a soft consonant
/// at a word's edge), by at most 200 ms on each side of each stretch of speech.
pub fn grow_speech(db: &[f32], speech: &mut [bool], edge: f32) {
    let n = speech.len().min(db.len());
    let loud = |i: usize| db.get(i).is_some_and(|v| *v >= edge);
    // forward: after each stretch of speech
    let mut run = usize::MAX;
    for i in 0..n {
        if speech[i] {
            run = 0;
        } else if run < MAX_EDGE_FRAMES && loud(i) {
            speech[i] = true;
            run += 1;
        } else {
            run = usize::MAX;
        }
    }
    // backward: before each one (a grown frame from the forward pass is no stretch of its own:
    // only frames that were speech before start a run here)
    let core: Vec<bool> = speech.to_vec();
    let mut run = usize::MAX;
    for i in (0..n).rev() {
        if core[i] {
            run = 0;
        } else if run < MAX_EDGE_FRAMES && loud(i) {
            speech[i] = true;
            run += 1;
        } else {
            run = usize::MAX;
        }
    }
}

/// [`speech_frames`] grown into soft word edges ([`grow_speech`]).
pub fn speech_frames_with_edges(db: &[f32], th: f32, edge: f32) -> Vec<bool> {
    let mut speech = speech_frames(db, th);
    grow_speech(db, &mut speech, edge);
    speech
}

/// Per 10 ms frame of levels `db`: does it hold speech? Frames at or above `th` do when their run
/// of loud frames lasts [`MIN_SPEECH_RUN`] frames or more, or lies within [`NEAR_SPEECH`] frames of
/// such a run; shorter isolated sound is a click (see the module docs).
pub fn speech_frames(db: &[f32], th: f32) -> Vec<bool> {
    let n = db.len();
    // loud runs as (start, end)
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if db[i] < th {
            i += 1;
            continue;
        }
        let a = i;
        while i < n && db[i] >= th {
            i += 1;
        }
        runs.push((a, i));
    }
    let long: Vec<(usize, usize)> = runs.iter().copied().filter(|(a, b)| b - a >= MIN_SPEECH_RUN).collect();
    let mut out = vec![false; n];
    for (a, b) in runs {
        // the nearest long run: the last one starting before this run ends, and the one after it
        let k = long.partition_point(|r| r.0 < b);
        let near = |r: Option<&(usize, usize)>| r.is_some_and(|&(ra, rb)| rb + NEAR_SPEECH >= a && ra <= b + NEAR_SPEECH);
        if b - a >= MIN_SPEECH_RUN || near(k.checked_sub(1).and_then(|k| long.get(k))) || near(long.get(k)) {
            for f in out.get_mut(a..b).into_iter().flatten() {
                *f = true;
            }
        }
    }
    out
}

/// Move word bounds inward past silent frames (see the module docs).
pub fn tighten_words(audio: &[f32], words: &mut [Word]) {
    let db = frame_db(audio);
    tighten_words_speech(&speech_frames_with_edges(&db, threshold(&db), edge_threshold(&db)), words);
}

/// [`tighten_words`] with the frame levels ([`frame_db`]) and threshold already computed.
pub fn tighten_words_db(db: &[f32], th: f32, words: &mut [Word]) {
    tighten_words_speech(&speech_frames_with_edges(db, th, edge_threshold(db).min(th)), words);
}

/// [`tighten_words`] with the speech frames ([`speech_frames`]) already worked out.
pub fn tighten_words_speech(speech: &[bool], words: &mut [Word]) {
    if speech.is_empty() {
        return;
    }
    let quiet = |f: usize| !speech.get(f).copied().unwrap_or(false);
    let frame_ticks = TICKS_PER_SAMPLE * FRAME as i64;
    for w in words {
        let a = (w.start.0 / frame_ticks).max(0) as usize;
        let b = (((w.end.0 + frame_ticks - 1) / frame_ticks).max(0) as usize).min(speech.len());
        if b <= a + MIN_WORD_FRAMES {
            continue;
        }
        let mut s = a;
        while s + MIN_WORD_FRAMES < b && quiet(s) {
            s += 1;
        }
        let mut e = b;
        while e > s + MIN_WORD_FRAMES && quiet(e - 1) {
            e -= 1;
        }
        // nothing voiced at all: leave the word alone
        if (s..e).all(quiet) {
            continue;
        }
        if s > a {
            w.start = filmcraft_time::Tick(s as i64 * frame_ticks);
        }
        if e < b {
            w.end = filmcraft_time::Tick(e as i64 * frame_ticks).max(w.start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seconds_tick;

    #[test]
    fn silence_is_trimmed_from_words() {
        // 0.0–0.5 silence, 0.5–1.0 tone, 1.0–2.0 silence, 2.0–2.5 tone
        let mut audio: Vec<f32> = (0..40_000).map(|i| if i % 2 == 0 { 0.0001 } else { 0.0 }).collect();
        for i in (8_000..16_000).chain(32_000..40_000) {
            audio[i] = 0.3 * (i as f32 * 0.2).sin();
        }
        let mut w = vec![Word::new("a", seconds_tick(0.0), seconds_tick(2.0)), Word::new("b", seconds_tick(2.0), seconds_tick(2.5))];
        tighten_words(&audio, &mut w);
        assert_eq!(w[0].start, seconds_tick(0.5));
        assert_eq!(w[0].end, seconds_tick(1.0));
        assert_eq!((w[1].start, w[1].end), (seconds_tick(2.0), seconds_tick(2.5)));
    }

    /// 16 kHz samples: quiet noise, with a tone over `loud` (sample ranges).
    fn audio_with(len: usize, loud: &[std::ops::Range<usize>]) -> Vec<f32> {
        let mut audio: Vec<f32> = (0..len).map(|i| if i % 2 == 0 { 0.0001 } else { 0.0 }).collect();
        for r in loud {
            for i in r.clone() {
                audio[i] = 0.3 * (i as f32 * 0.2).sin();
            }
        }
        audio
    }

    #[test]
    fn a_click_in_the_pause_does_not_hold_the_word_open() {
        // speech 0.5–1.0, a 20 ms click at 1.6, speech again 2.0–2.5; the aligner ran the first word
        // up to the next one, over the click
        let audio = audio_with(40_000, &[8_000..16_000, 25_600..25_920, 32_000..40_000]);
        let mut w = vec![Word::new("a", seconds_tick(0.5), seconds_tick(2.0)), Word::new("b", seconds_tick(2.0), seconds_tick(2.5))];
        tighten_words(&audio, &mut w);
        assert_eq!(w[0].end, seconds_tick(1.0), "the pause after the word is free");
        assert_eq!((w[1].start, w[1].end), (seconds_tick(2.0), seconds_tick(2.5)));
    }

    #[test]
    fn a_final_consonant_after_its_closure_stays_in_the_word() {
        // vowel 0.5–0.9, closure, a 20 ms release burst at 1.0 (the t of "ist"), silence
        let audio = audio_with(32_000, &[8_000..14_400, 16_000..16_320]);
        let mut w = vec![Word::new("ist", seconds_tick(0.5), seconds_tick(1.8))];
        tighten_words(&audio, &mut w);
        assert_eq!(w[0].end, seconds_tick(1.02));
    }

    /// 16 kHz samples: room noise (about −63 dBFS), with tones of `amp` over sample ranges.
    fn audio_levels(len: usize, parts: &[(std::ops::Range<usize>, f32)]) -> Vec<f32> {
        let mut audio: Vec<f32> = (0..len).map(|i| if i % 2 == 0 { 0.001 } else { 0.0 }).collect();
        for (r, amp) in parts {
            for i in r.clone() {
                audio[i] = amp * (i as f32 * 0.2).sin();
            }
        }
        audio
    }

    /// A soft consonant (30 dB below the vowel, as an s or f is) before and after the vowel stays in
    /// the word: the start isn't moved past it, so deleting the pause before the word keeps it.
    #[test]
    fn soft_consonants_at_the_word_edges_stay_in_the_word() {
        // room noise, "s" 0.38–0.5 at about −50 dBFS, vowel 0.5–0.9 at −13.5, "s" 0.9–1.0, room noise:
        // the consonant is below the speech threshold (about −48) and above the edge one (−57)
        let audio = audio_levels(32_000, &[(6_080..8_000, 0.0045), (8_000..14_400, 0.3), (14_400..16_000, 0.0045)]);
        let db = frame_db(&audio);
        assert!(edge_threshold(&db) < -50.5 && -50.5 < threshold(&db), "{} {}", edge_threshold(&db), threshold(&db));
        let mut w = vec![Word::new("so", seconds_tick(0.2), seconds_tick(1.6))];
        tighten_words(&audio, &mut w);
        assert_eq!(w[0].start, seconds_tick(0.38), "the soft start is part of the word");
        assert_eq!(w[0].end, seconds_tick(1.0), "and so is the soft end");
        // without the edges the word would start at the vowel
        let mut core = vec![Word::new("so", seconds_tick(0.2), seconds_tick(1.6))];
        tighten_words_speech(&speech_frames(&db, threshold(&db)), &mut core);
        assert_eq!(core[0].start, seconds_tick(0.5));
    }

    /// Soft sound next to speech counts for at most 200 ms on each side: steady noise above the edge
    /// threshold (a hum, a breath) doesn't fill the pause.
    #[test]
    fn soft_edges_grow_at_most_200_ms() {
        let mut db = vec![-90.0f32; 200];
        for f in 50..100 {
            db[f] = -20.0;
        }
        for f in (0..50).chain(100..200) {
            db[f] = -60.0;
        }
        let sp = speech_frames_with_edges(&db, -40.0, -70.0);
        assert_eq!(sp.iter().position(|v| *v), Some(30), "200 ms before the speech");
        assert_eq!(sp.iter().rposition(|v| *v), Some(119), "200 ms after it");
        // hostile input: empty, mismatched lengths
        grow_speech(&[], &mut [true, false], -60.0);
        grow_speech(&[-10.0, -10.0], &mut [], -60.0);
        assert!(edge_threshold(&[]) < threshold(&[]));
    }

    #[test]
    fn short_sound_counts_as_speech_only_near_longer_sound() {
        let mut db = vec![-90.0f32; 100];
        for f in (10..30).chain(35..37).chain(70..72) {
            db[f] = -20.0;
        }
        let sp = speech_frames(&db, -60.0);
        assert!(sp[10..30].iter().all(|v| *v), "a long run");
        assert!(sp[35..37].iter().all(|v| *v), "a burst 50 ms after it");
        assert!(sp[70..72].iter().all(|v| !*v), "a click 400 ms away");
        assert!(!sp[0] && !sp[50]);
        assert!(speech_frames(&[], -60.0).is_empty());
    }
}
