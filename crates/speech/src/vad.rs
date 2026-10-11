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
//!
//! Breath is louder than both thresholds too: an inhale before a sentence or an exhale after it
//! held a word's edge out in the pause, so a deleted pause left the breath and the cut sat well off
//! where the voice starts or stops. [`fit_words_to_voice`] then fits each word to its **voice**: it
//! starts at its first voiced frame and ends at its last, each widened by the sound joined to it
//! without a gap. A frame is voiced when its waveform repeats with a pitch period, as on a vowel
//! (breath is noise), and it is at most 40 dB below the loudest frame within a second of it: a
//! noise gate lets the room tone fade out over 300–500 ms after a sentence, and the hum in it
//! repeats with a period too, but far below the voice (counted as voice, it held words 300 ms
//! into the fade). The voice runs on through gaps of up to 50 ms (an unvoiced consonant between
//! syllables), so a word the recogniser placed late or early still gets all of it; a word reaches
//! at most 500 ms past its recognised bounds (sound that goes on, a tone or a music bed under the
//! voice, would carry it to the middle of every gap). Joined sound:
//!
//! - **loud** sound (at most 20 dB below the word's loudest frame and the voice around it: a
//!   rough or creaky voice, a plosive's burst) up to that reach;
//! - **soft** sound at most 150 ms before (a soft "s", "f", "sch", "h") no quieter than 35 dB
//!   below the word's loudest frame, and at most 120 ms after (a fading end, the "ch" of "nicht":
//!   as quiet as an exhale and as noisy, so up to 120 ms of an exhale stays too);
//! - a **hiss** ("s", "f", "sch": mostly high frequencies, so the waveform crosses zero at least
//!   every other sample; breath, lower, crosses half as often) no quieter than 45 dB below, for up
//!   to 200 ms after the voice: a final "s" hisses on longer than a voice fades;
//! - a final stop released after its closure (the t of "ist": a burst of up to 60 ms within 150 ms,
//!   or an aspirated one hissing for up to 200 ms).
//!
//! An inhale with a gap before the voice, or longer than that, is no longer part of the word, and
//! goes when the pause is deleted.
//!
//! **Words placed off their sound** ([`place_words`]). Around a pause the recogniser's times can
//! be far off: it started the first word after a pause in the silence before it (Parakeet
//! shortens dead air before recognising, and a word started there was mapped back to the start of
//! the silence: "Oh," ran over 5.6 s of it and the pause was never found), or put the last word
//! before a pause after the voice had ended. A word without voice of its own near its bounds, and
//! no sound within 25 dB of the voice around it, goes to the nearer of the sound before it and the
//! sound after it (within a second); the words next to it make room and never pass each other. A
//! word without voice but with sound of its own (a whisper) is fitted to that sound; one with none
//! at all keeps its bounds less the dead air at their ends (frames below −80 dBFS: digital
//! silence or a closed gate, where nothing is said).

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
/// How far (frames) past its recognised bounds a word's voice is looked for ([`fit_words_to_voice`];
/// never past the middle of the gap to a neighbour).
const VOICE_SEARCH: usize = 15;
/// Unvoiced sound joined to a word's first voiced frame belongs to it for at most this long
/// (frames)...
const SOFT_START: usize = 15;
/// ...and to its last one (a hiss: [`SOFT_END_HISS`]).
const SOFT_END: usize = 12;
const SOFT_END_HISS: usize = 20;
/// ...when it is at most this far (dB) below the word's loudest frame (sound at most [`LOUD_DB`]
/// below it belongs to the word however long)...
const SOFT_DB: f32 = 35.0;
const LOUD_DB: f32 = 20.0;
/// ...or a hiss ([`HISS_ZCR`]) at most this far.
const HISS_DB: f32 = 45.0;
/// Zero crossings per sample from which a frame is a hiss ("s", "f", "sch"; energy mostly above
/// about 3 kHz) rather than breath.
const HISS_ZCR: f32 = 0.5;
/// A final stop released after its closure: a burst at most this long (frames)...
const RELEASE_FRAMES: usize = 6;
/// ...starting at most this long (frames) after the word's sound.
const CLOSURE_FRAMES: usize = 15;
/// A frame is voiced when its waveform repeats this well one pitch period later ([`periodicity`]).
const VOICED: f32 = 0.6;
/// ...and is at most this far (dB) below the loudest frame within [`LOCAL_PEAK_FRAMES`] of it: the
/// room tone a noise gate lets fade out after a sentence, or a hum, repeats with a period too, but
/// is far below the voice next to it.
const VOICE_RANGE_DB: f32 = 40.0;
/// How far (frames) on each side the loudest frame for [`VOICE_RANGE_DB`] is looked for.
pub(crate) const LOCAL_PEAK_FRAMES: usize = 100;
/// A word with less voice than this (frames) near its recognised bounds has none of its own
/// ([`place_words`])...
const MIN_VOICE_FRAMES: usize = 4;
/// ...and goes to voice at most this far (frames) before or after it...
const STRAY_SEARCH: usize = 100;
/// ...this long (frames) at most, before it is fitted. Its frames have no sound within this many dB
/// of the loudest frame near them ([`Voice::sounds`]).
const STRAY_FRAMES: usize = 10;
const STRAY_DB: f32 = 25.0;
/// The most frames of a word looked at for its sound (a hostile transcript can hold a word of
/// hours).
const MAX_WORD_LOOK: usize = 6_000;
/// A word's voice runs on through gaps this short (frames: an unvoiced consonant between syllables)...
const VOICE_BRIDGE: usize = 5;
/// ...and the word, with the sound joined to it, reaches at most this far (frames) past its bounds,
/// unless its voice starts or ends there with [`QUIET_FRAMES`] of quiet next to it: sound that goes
/// on and on (a tone, a music bed) is no reason to stretch a word to the middle of every gap.
const MAX_REACH: usize = 50;
const QUIET_FRAMES: usize = 10;
/// Dead air: frames quieter than this (dBFS) are digital silence or a closed noise gate; nothing
/// is said there.
const DEAD_AIR_DB: f32 = -80.0;
/// Pitch periods looked at: 500 Hz down to about 70 Hz (samples; a higher voice repeats at twice
/// its period too).
const MIN_LAG: usize = 32;
const MAX_LAG: usize = 228;
/// The stretch of waveform compared with itself one period later (30 ms).
const WINDOW: usize = 480;

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

/// The level of the voice in a clip (95th percentile of the frame levels), in dBFS; None without a
/// frame above digital silence.
pub(crate) fn speech_level(db: &[f32]) -> Option<f32> {
    levels(db).map(|(_, speech)| speech)
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

/// The best normalised autocorrelation of the 30 ms of `audio` (16 kHz) around sample `centre`
/// with itself one pitch period (2–14 ms) later: near 1 where the voice sounds (a vowel, a voiced
/// laugh), low for noise (breath, "s", "f", a cough); 0 where too little audio is left.
pub fn periodicity(audio: &[f32], centre: usize) -> f32 {
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

/// Zero crossings per sample of 10 ms frame `f` (0 past the end).
pub(crate) fn zero_crossings(audio: &[f32], f: usize) -> f32 {
    let a = f.saturating_mul(FRAME);
    let Some(x) = audio.get(a..a.saturating_add(FRAME).min(audio.len())) else { return 0.0 };
    let n = x.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
    n as f32 / x.len().max(1) as f32
}

/// The middle sample of 10 ms frame `f`.
pub(crate) fn frame_centre(f: usize) -> usize {
    f.saturating_mul(FRAME).saturating_add(FRAME / 2)
}

/// The loudest of the frame levels `db` within `radius` frames of each frame.
pub(crate) fn local_peaks(db: &[f32], radius: usize) -> Vec<f32> {
    let n = db.len();
    let mut out = Vec::with_capacity(n);
    // indices of a falling run of levels: the front is the loudest in the window
    let mut run: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let level = |f: usize| db.get(f).copied().unwrap_or(f32::NEG_INFINITY);
    let mut next = 0usize;
    for i in 0..n {
        while next < n && next <= i.saturating_add(radius) {
            while run.back().is_some_and(|&b| level(b) <= level(next)) {
                run.pop_back();
            }
            run.push_back(next);
            next += 1;
        }
        while run.front().is_some_and(|&f| f.saturating_add(radius) < i) {
            run.pop_front();
        }
        out.push(run.front().map_or(f32::NEG_INFINITY, |&f| level(f)));
    }
    out
}

/// Where the voice sounds, per 10 ms frame, worked out once a frame is asked about: at or above
/// the speech threshold, at most [`VOICE_RANGE_DB`] below the loudest frame around it, and
/// periodic (see the module docs).
struct Voice<'a> {
    audio: &'a [f32],
    db: &'a [f32],
    peaks: Vec<f32>,
    th: f32,
    /// 0: not asked yet, 1: voiced, 2: not
    seen: Vec<u8>,
}

impl<'a> Voice<'a> {
    fn new(audio: &'a [f32], db: &'a [f32], th: f32) -> Self {
        Self { audio, db, peaks: local_peaks(db, LOCAL_PEAK_FRAMES), th, seen: vec![0; db.len()] }
    }

    fn level(&self, f: usize) -> f32 {
        self.db.get(f).copied().unwrap_or(f32::NEG_INFINITY)
    }

    fn voiced(&mut self, f: usize) -> bool {
        match self.seen.get(f) {
            None => false,
            Some(1) => true,
            Some(2) => false,
            Some(_) => {
                let floor = self.th.max(self.peaks.get(f).copied().unwrap_or(f32::NEG_INFINITY) - VOICE_RANGE_DB);
                let v = self.level(f) >= floor && periodicity(self.audio, frame_centre(f)) >= VOICED;
                if let Some(s) = self.seen.get_mut(f) {
                    *s = if v { 1 } else { 2 };
                }
                v
            }
        }
    }

    /// Whether frames `a..b` hold sound near the level of the voice around them: their
    /// [`MIN_VOICE_FRAMES`]th loudest frame at most [`STRAY_DB`] below the loudest frame near
    /// them. A word placed in the silence next to its sound (or in the room tone fading out after
    /// it) has none; a whispered word, or a voice too rough to be found periodic, has.
    fn sounds(&self, a: usize, b: usize) -> bool {
        let b = b.min(a.saturating_add(MAX_WORD_LOOK)).min(self.db.len());
        let Some(levels) = self.db.get(a..b) else { return false };
        let mut v = levels.to_vec();
        v.sort_by(|x, y| y.total_cmp(x));
        let Some(&held) = v.get(MIN_VOICE_FRAMES - 1).or(v.last()) else { return false };
        let near = self.peaks.get(a..b).into_iter().flatten().copied().fold(f32::NEG_INFINITY, f32::max);
        held >= DEAD_AIR_DB && held >= near - STRAY_DB
    }

    /// Whether frame `f` is sound near the level of the voice around it (at most [`STRAY_DB`] below
    /// the loudest frame near it, and at or above the speech threshold).
    fn audible(&self, f: usize) -> bool {
        let level = self.level(f);
        level >= self.th && level >= self.peaks.get(f).copied().unwrap_or(f32::NEG_INFINITY) - STRAY_DB
    }

    /// `f0..=f1` widened through the voice around it (gaps up to [`VOICE_BRIDGE`] frames bridged),
    /// within `lo..hi`: a word the recogniser placed late or early still gets all of its voice.
    fn grow(&mut self, f0: usize, f1: usize, lo: usize, hi: usize) -> (usize, usize) {
        let mut a = f0;
        while let Some(f) = (a.saturating_sub(VOICE_BRIDGE).max(lo)..a).rev().find(|&f| self.voiced(f)) {
            a = f;
        }
        let mut b = f1;
        while let Some(f) = (b.saturating_add(1)..b.saturating_add(VOICE_BRIDGE + 1).min(hi)).find(|&f| self.voiced(f)) {
            b = f;
        }
        (a, b)
    }

    /// Whether frames `a..b` (inside the clip) hold no sound near the voice's level.
    fn quiet(&self, a: usize, b: usize) -> bool {
        a < b && b <= self.db.len() && (a..b).all(|f| !self.audible(f))
    }

    /// The first and last frame of `a..b` with sound near the voice's level ([`Voice::audible`]).
    fn audible_span(&self, a: usize, b: usize) -> Option<(usize, usize)> {
        let f0 = (a..b).find(|&f| self.audible(f))?;
        let f1 = (f0..b).rev().find(|&f| self.audible(f))?;
        Some((f0, f1))
    }

    /// The first and last voiced frame of `a..b`.
    fn span(&mut self, a: usize, b: usize) -> Option<(usize, usize)> {
        let f0 = (a..b).find(|&f| self.voiced(f))?;
        let f1 = (f0..b).rev().find(|&f| self.voiced(f))?;
        Some((f0, f1))
    }
}

/// The frames around word `i` of `bounds` where its voice is looked for: up to [`VOICE_SEARCH`]
/// past its bounds, no further than the middle of the gaps to its neighbours.
fn search_window(bounds: &[(usize, usize)], i: usize, n: usize) -> Option<(usize, usize)> {
    let &(s, e) = bounds.get(i)?;
    let lo = i.checked_sub(1).and_then(|k| bounds.get(k)).map_or(0, |&(_, pe)| pe.max(s.saturating_sub(s.saturating_sub(pe) / 2))).min(s);
    let hi = bounds.get(i + 1).map_or(n, |&(ns, _)| e.saturating_add(ns.saturating_sub(e) / 2)).min(n);
    Some((s.saturating_sub(VOICE_SEARCH).max(lo), e.saturating_add(VOICE_SEARCH).min(hi.max(e)).min(n)))
}

/// Move the words without voice of their own (less than [`MIN_VOICE_FRAMES`] of it near their
/// recognised bounds) to the voice they belong to: the recogniser starts the first word after a
/// pause too early, in the silence before its sound, or puts the last word before a pause after
/// the voice has ended. Such a word goes to the nearer of the voice before it (its end) and the
/// voice after it (its start), when one lies within [`STRAY_SEARCH`], [`STRAY_FRAMES`] long at
/// most; the words next to it make room. Bounds in frames, in time order; they stay in order.
fn place_words(voice: &mut Voice, bounds: &[(usize, usize)], n: usize) -> Vec<(usize, usize)> {
    let mut out = bounds.to_vec();
    // which way a word moved: to the voice before it (true) or after it (false)
    let mut back = vec![false; bounds.len()];
    for (i, &(s, e)) in bounds.iter().enumerate() {
        let Some((wa, wb)) = search_window(bounds, i, n) else { continue };
        if voice.span(wa, wb).is_some_and(|(f0, f1)| f1 >= f0 + MIN_VOICE_FRAMES - 1) || voice.sounds(s, e) {
            continue;
        }
        // never past a neighbour: the words keep their order
        let first = i.checked_sub(1).and_then(|k| bounds.get(k)).map_or(0, |&(ps, _)| ps.saturating_add(1)).max(s.saturating_sub(STRAY_SEARCH));
        let last = bounds.get(i + 1).map_or(n, |&(_, ne)| ne.saturating_sub(1)).min(e.saturating_add(STRAY_SEARCH)).min(n);
        // the sound next to it (a rough voice is not found periodic everywhere)
        let before = (first..s.min(n)).rev().find(|&f| voice.audible(f)).map(|f| f + 1);
        let after = (e.min(n)..last).find(|&f| voice.audible(f));
        let len = e.saturating_sub(s).clamp(1, STRAY_FRAMES);
        let to_before = match (before, after) {
            (Some(b), Some(a)) => s.saturating_sub(b) <= a.saturating_sub(e),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => continue,
        };
        let (Some(slot), Some(flag)) = (out.get_mut(i), back.get_mut(i)) else { continue };
        *slot = match (to_before, before, after) {
            (true, Some(b), _) => (b.saturating_sub(len).max(first).min(b), b),
            (false, _, Some(a)) => (a, a.saturating_add(len).min(n)),
            _ => continue,
        };
        *flag = to_before;
    }
    // a word moved on pushes the ones after it on...
    for i in 1..out.len() {
        let (Some(&(_, pe)), Some(&moved_back)) = (out.get(i - 1), back.get(i)) else { continue };
        if let Some(w) = out.get_mut(i)
            && !moved_back
            && w.0 < pe
        {
            *w = (pe, w.1.max(pe.saturating_add(1)));
        }
    }
    // ...and one moved back pulls the ones before it back
    for i in (1..out.len()).rev() {
        let Some(&(ns, _)) = out.get(i) else { continue };
        if let Some(w) = out.get_mut(i - 1)
            && w.1 > ns
        {
            *w = (w.0.min(ns.saturating_sub(1)), ns);
        }
    }
    out
}

/// Fit each word's bounds to its voice (see the module docs): a word without voice of its own is
/// first moved to the voice next to it ([`place_words`]); then each runs from its first voiced
/// frame to its last, looked for up to 150 ms past its bounds (and no further than the middle of
/// the gap to a neighbour), widened by the soft sound joined to them. A word without voice near it
/// (a whisper) keeps its bounds, less the dead air at their ends. Words in time order; they stay in
/// order and apart.
pub fn fit_words_to_voice(audio: &[f32], words: &mut [Word]) {
    let db = frame_db(audio);
    let n = db.len();
    if n == 0 {
        return;
    }
    let (th, edge) = (threshold(&db), edge_threshold(&db));
    let ft = TICKS_PER_SAMPLE * FRAME as i64;
    let floor = |t: filmcraft_time::Tick| usize::try_from(t.0.max(0) / ft).unwrap_or(usize::MAX).min(n);
    let ceil = |t: filmcraft_time::Tick| usize::try_from(t.0.max(0).saturating_add(ft - 1) / ft).unwrap_or(usize::MAX).min(n);
    let tick = |f: usize| filmcraft_time::Tick(i64::try_from(f).unwrap_or(i64::MAX).saturating_mul(ft));
    let level = |f: usize| db.get(f).copied().unwrap_or(f32::NEG_INFINITY);
    let dead = |f: usize| level(f) < DEAD_AIR_DB;
    let mut voice = Voice::new(audio, &db, th);
    // the recognised bounds in frames, the words without voice of their own moved to it
    let recognised: Vec<(usize, usize)> = words.iter().map(|w| (floor(w.start), ceil(w.end).max(floor(w.start)))).collect();
    let bounds = place_words(&mut voice, &recognised, n);
    // where the word before ends (fitted): no word starts before it
    let mut prev_end = 0usize;
    for (i, w) in words.iter_mut().enumerate() {
        let (Some(&(s, e)), Some((wa, wb))) = (bounds.get(i), search_window(&bounds, i, n)) else { break };
        // the soft sound joined to the voice reaches no further than the middle of the gaps
        let lo = i.checked_sub(1).and_then(|k| bounds.get(k)).map_or(0, |&(_, pe)| pe.max(s.saturating_sub(s.saturating_sub(pe) / 2))).min(s).max(prev_end);
        let hi = bounds.get(i + 1).map_or(n, |&(ns, _)| e.saturating_add(ns.saturating_sub(e) / 2)).min(n);
        // its voice, else (a whisper, or the voice of the word next to it) its sound
        let found = voice.span(wa.max(prev_end), wb);
        let found = found.or_else(|| voice.sounds(s, e).then(|| voice.audible_span(s.max(prev_end), e.min(wb))).flatten());
        let Some((f0, f1)) = found else {
            // no sound: the bounds stay, less the dead air at their ends (nothing is said there),
            // after the word before
            let (mut a, mut b) = (s.max(prev_end), e.max(s.max(prev_end)));
            while a < b && dead(a) {
                a += 1;
            }
            while b > a && dead(b - 1) {
                b -= 1;
            }
            if (a, b) != (floor(w.start), ceil(w.end)) {
                w.start = tick(a);
                w.end = tick(b);
            }
            prev_end = b;
            continue;
        };
        let (f0, f1) = voice.grow(f0, f1, lo, hi);
        // the word's loudness: its voice, and its bounds (a rough voice is not periodic everywhere)
        let peak = (f0..=f1).chain(s..e.min(n)).map(level).fold(f32::NEG_INFINITY, f32::max);
        // the soft sound joined to the voice: not far below it, or a hiss
        let (soft, hiss) = (edge.max(peak - SOFT_DB), edge.max(peak - HISS_DB));
        let hissing = |f: usize| level(f) >= hiss && zero_crossings(audio, f) >= HISS_ZCR;
        let joined = |f: usize| level(f) >= soft || hissing(f);
        // loud sound joined to the voice is part of the word up to its reach (a rough or creaky
        // voice, a plosive's burst); the quieter soft sound is held to a shorter length
        let loud = |f: usize| level(f) >= th.max(peak - LOUD_DB).max(voice.peaks.get(f).copied().unwrap_or(f32::NEG_INFINITY) - LOUD_DB);
        let mut a = f0;
        let mut soft_run = 0;
        while a > lo && joined(a - 1) {
            soft_run = if loud(a - 1) { 0 } else { soft_run + 1 };
            if soft_run > SOFT_START {
                break;
            }
            a -= 1;
        }
        let limit = hi.max(f1 + 1);
        let mut b = f1 + 1;
        // a final "s", "sch" or "f" hisses on longer than a fading voice or breath
        let mut soft_run = 0;
        while b < limit && joined(b) {
            soft_run = if loud(b) { 0 } else { soft_run + 1 };
            if soft_run > SOFT_END_HISS || soft_run > SOFT_END && !hissing(b) {
                break;
            }
            b += 1;
        }
        // a final stop released after its closure: a short burst soon after, then quiet (an
        // aspirated one hisses on longer)
        if let Some(r) = (b..b.saturating_add(CLOSURE_FRAMES).min(limit)).find(|&f| level(f) >= soft && (loud(f) || hissing(f)))
            && let Some(r_end) = (r..limit).find(|&f| level(f) < soft)
            && (r_end - r <= RELEASE_FRAMES || r_end - r <= SOFT_END_HISS && (r..r_end).filter(|&f| hissing(f)).count() * 2 > r_end - r)
        {
            b = r_end;
        }
        // a word reaches at most MAX_REACH past its bounds, unless its sound starts (ends) there,
        // with quiet before (after) it: a drawn-out "Daaa" the recogniser started late; a music bed
        // or a tone under the voice would otherwise carry it to the middle of every gap
        let (near_lo, near_hi) = (s.saturating_sub(MAX_REACH), e.saturating_add(MAX_REACH));
        let a = if a < near_lo && !voice.quiet(a.saturating_sub(QUIET_FRAMES), a) { near_lo } else { a };
        let b = if b > near_hi && !voice.quiet(b, b.saturating_add(QUIET_FRAMES)) { near_hi.max(a.saturating_add(1)) } else { b };
        // a short word keeps the least length a word has, from where its sound starts
        let b = b.max(a.saturating_add(MIN_WORD_FRAMES).min(n));
        w.start = tick(a);
        w.end = tick(b);
        prev_end = b;
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

    const SR: usize = 16_000;

    fn at(s: f64) -> usize {
        (s * SR as f64).round() as usize
    }

    /// Noise of amplitude `amp` (pseudo-random, `seed`).
    fn noise(len: usize, amp: f32, seed: u32) -> Vec<f32> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 2.0 * amp
            })
            .collect()
    }

    /// Breath: noise with the high end taken out (it crosses zero far less often than a hiss).
    fn breath(len: usize, amp: f32, seed: u32) -> Vec<f32> {
        let n = noise(len + 3, 2.0 * amp, seed);
        n.windows(4).map(|w| w.iter().sum::<f32>() / 4.0).collect()
    }

    /// A hiss ("s"): noise with the low end taken out.
    fn hiss(len: usize, amp: f32, seed: u32) -> Vec<f32> {
        let n = noise(len + 1, amp, seed);
        n.windows(2).map(|w| w[1] - w[0]).collect()
    }

    /// A voice at 140 Hz (ten harmonics), about −16 dBFS, with 5 ms fades.
    fn voice(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / SR as f32;
                let v: f32 = (1..=10).map(|k| (2.0 * std::f32::consts::PI * 140.0 * k as f32 * t).sin() / k as f32).sum();
                let fade = (i.min(len - i) as f32 / 80.0).min(1.0);
                0.18 * v * fade
            })
            .collect()
    }

    fn add(audio: &mut [f32], from: f64, part: &[f32]) {
        for (a, p) in audio.iter_mut().skip(at(from)).zip(part) {
            *a += p;
        }
    }

    /// Room noise (about −65 dBFS) with the voice over `voiced` (seconds).
    fn room_with_voice(secs: f64, voiced: &[(f64, f64)]) -> Vec<f32> {
        let mut audio = noise(at(secs), 0.001, 1);
        for &(a, b) in voiced {
            add(&mut audio, a, &voice(at(b - a)));
        }
        audio
    }

    fn fit(audio: &[f32], bounds: &[(f64, f64)]) -> Vec<(f64, f64)> {
        let mut w: Vec<Word> = bounds.iter().map(|&(a, b)| Word::new("w", seconds_tick(a), seconds_tick(b))).collect();
        fit_words_to_voice(audio, &mut w);
        w.iter().map(|w| (w.start.seconds(), w.end.seconds())).collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 0.011
    }

    /// The case of a deleted pause that left the breath: the recogniser started "Nee" in the inhale
    /// before it (a gap, then the voice). Fitted, the word starts where the voice does.
    #[test]
    fn an_inhale_before_a_word_is_not_part_of_it() {
        let mut audio = room_with_voice(2.0, &[(1.0, 1.6)]);
        add(&mut audio, 0.5, &noise(at(0.4), 0.02, 7));
        // tightening alone keeps the inhale in the word: it is louder than the speech threshold
        let mut tight = vec![Word::new("Nee", seconds_tick(0.55), seconds_tick(1.6))];
        tighten_words(&audio, &mut tight);
        assert!(tight[0].start.seconds() < 0.6, "{:?}", tight[0]);
        let w = fit(&audio, &[(0.55, 1.6)]);
        assert!(close(w[0].0, 1.0) && close(w[0].1, 1.6), "{w:?}");
        // an inhale joined to the voice without a gap: at most 150 ms of it stays
        let mut audio = room_with_voice(2.0, &[(1.0, 1.6)]);
        add(&mut audio, 0.6, &noise(at(0.4), 0.02, 7));
        let w = fit(&audio, &[(0.6, 1.6)]);
        assert!(close(w[0].0, 0.85), "{w:?}");
    }

    /// Breath let out after a word holds its end out for at most 120 ms.
    #[test]
    fn an_exhale_after_a_word_is_cut_short() {
        let mut audio = room_with_voice(2.5, &[(1.0, 1.6)]);
        add(&mut audio, 1.6, &breath(at(0.5), 0.02, 3));
        let w = fit(&audio, &[(1.0, 2.1)]);
        assert!(close(w[0].0, 1.0) && close(w[0].1, 1.72), "{w:?}");
        // with a gap before it, none of it
        let mut audio = room_with_voice(2.5, &[(1.0, 1.6)]);
        add(&mut audio, 1.7, &breath(at(0.4), 0.02, 3));
        let w = fit(&audio, &[(1.0, 2.1)]);
        assert!(close(w[0].1, 1.6), "{w:?}");
    }

    /// The other way round: a word the recogniser started late (after its soft "s") or ended early
    /// grows to its sound, so deleting the pause never cuts it.
    #[test]
    fn a_soft_start_placed_late_and_an_end_placed_early_are_found() {
        let mut audio = room_with_voice(2.0, &[(1.0, 1.6)]);
        add(&mut audio, 0.88, &hiss(at(0.12), 0.03, 5));
        let w = fit(&audio, &[(1.06, 1.5)]);
        assert!(close(w[0].0, 0.88) && close(w[0].1, 1.6), "{w:?}");
        // a weak final "f" (40 dB below the vowel) the recogniser cut off: back in the word
        let mut audio = room_with_voice(2.0, &[(1.0, 1.5)]);
        add(&mut audio, 1.5, &hiss(at(0.11), 0.0017, 6));
        let w = fit(&audio, &[(1.0, 1.45)]);
        assert!(close(w[0].1, 1.61), "{w:?}");
    }

    /// The t of "ist" after its closure stays (an aspirated one, hissing on for 150 ms, too); a word
    /// without voice (a whisper) is fitted to its sound.
    #[test]
    fn a_final_stop_and_a_whisper_stay() {
        let mut audio = room_with_voice(2.0, &[(1.0, 1.4)]);
        add(&mut audio, 1.5, &noise(at(0.02), 0.3, 9));
        let w = fit(&audio, &[(1.0, 1.8)]);
        assert!(close(w[0].1, 1.52), "{w:?}");
        let mut audio = room_with_voice(2.0, &[(1.0, 1.4)]);
        add(&mut audio, 1.45, &hiss(at(0.15), 0.08, 9));
        let w = fit(&audio, &[(1.0, 1.8)]);
        assert!(close(w[0].1, 1.6), "{w:?}");
        let mut audio = room_with_voice(2.0, &[]);
        add(&mut audio, 0.5, &hiss(at(0.5), 0.05, 2));
        let w = fit(&audio, &[(0.4, 1.1)]);
        assert!(close(w[0].0, 0.5) && close(w[0].1, 1.0), "{w:?}");
    }

    /// A long final "s" hisses on past where a fading voice would end; breath, which crosses zero
    /// far less often, does not.
    #[test]
    fn a_final_s_stays_but_breath_does_not() {
        let mut audio = room_with_voice(2.5, &[(1.0, 1.4)]);
        add(&mut audio, 1.4, &hiss(at(0.2), 0.06, 4));
        let w = fit(&audio, &[(1.0, 1.45)]);
        assert!(close(w[0].1, 1.6), "the whole s: {w:?}");
        let mut audio = room_with_voice(2.5, &[(1.0, 1.4)]);
        add(&mut audio, 1.4, &breath(at(0.3), 0.012, 4));
        let w = fit(&audio, &[(1.0, 1.7)]);
        assert!(w[0].1 <= 1.4 + 0.121, "at most 120 ms of breath: {w:?}");
    }

    /// A recording with a noise gate (the user's OBS recordings have one): dead air (exact zeros)
    /// between phrases; the gate opens 50 ms before the voice and lets the room tone (about
    /// -65 dBFS) and a hum (-58 dBFS, periodic like a voice but far quieter) fade out over 400 ms
    /// after it.
    fn gated(secs: f64, phrases: &[(f64, f64)]) -> Vec<f32> {
        let mut audio = vec![0.0f32; at(secs)];
        for &(a, b) in phrases {
            add(&mut audio, a - 0.05, &noise(at(b - a + 0.05), 0.001, 11));
            add(&mut audio, a, &voice(at(b - a)));
            let tail: Vec<f32> = noise(at(0.4), 0.001, 12)
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let t = i as f32 / SR as f32;
                    let hum = 0.0018 * (2.0 * std::f32::consts::PI * 100.0 * t).sin();
                    (n + hum) * 10f32.powf(-1.75 * t / 0.4)
                })
                .collect();
            add(&mut audio, b, &tail);
        }
        audio
    }

    fn words(bounds: &[(f64, f64)]) -> Vec<Word> {
        bounds.iter().enumerate().map(|(i, &(a, b))| Word::new(format!("w{i}"), seconds_tick(a), seconds_tick(b))).collect()
    }

    fn spans(w: &[Word]) -> Vec<(f64, f64)> {
        w.iter().map(|w| (w.start.seconds(), w.end.seconds())).collect()
    }

    /// The room tone a gate lets fade out after a sentence hums with a period, as a voice does, but
    /// far below the voice: the word ends with its voice, not 300 ms into the fade (a deleted
    /// pause left that much air after the word).
    #[test]
    fn the_room_tone_fading_after_a_sentence_is_not_voice() {
        let audio = gated(3.0, &[(0.5, 1.0), (2.0, 2.5)]);
        let w = fit(&audio, &[(0.5, 1.4), (2.0, 2.5)]);
        assert!(close(w[0].1, 1.0), "{w:?}");
    }

    /// Parakeet shortens dead air before recognising and started the first word after it in that
    /// silence: "Oh," ran over 5.6 s of it, and the pause before it was never found. A word placed
    /// in dead air goes to the voice it belongs to (the nearer one).
    #[test]
    fn a_word_placed_in_dead_air_goes_to_its_voice() {
        let audio = gated(5.0, &[(0.5, 1.0), (3.5, 4.2)]);
        let mut w = words(&[(0.5, 1.0), (1.1, 3.45), (3.5, 3.8), (3.8, 4.2)]);
        fit_words_to_voice(&audio, &mut w);
        let s = spans(&w);
        assert!(s[1].0 >= 3.49 && s[1].1 <= 3.65, "on the voice after the silence: {s:?}");
        assert!(s[1].0 - s[0].1 > 2.4, "the pause is free: {s:?}");
        assert!(s.windows(2).all(|p| p[0].1 <= p[1].0), "in order: {s:?}");
        // one placed in the silence just after a phrase goes back to it
        let mut w = words(&[(0.5, 0.8), (0.8, 1.0), (1.05, 1.35), (3.5, 4.2)]);
        fit_words_to_voice(&audio, &mut w);
        let s = spans(&w);
        assert!(s[2].1 <= 1.02 && s[2].0 < 1.0 && s[2].0 >= s[1].1, "back on the voice before: {s:?}");
    }

    /// Several words between two stretches of voice with nothing of theirs in between: each goes to
    /// the nearer voice, and the transcript keeps its order (moved words never pass each other).
    #[test]
    fn words_without_voice_keep_their_order() {
        let audio = room_with_voice(3.0, &[(0.5, 0.8), (1.6, 2.0)]);
        let mut w = words(&[(0.5, 0.8), (0.85, 1.0), (1.0, 1.2), (1.25, 1.4), (1.4, 1.55), (1.6, 2.0)]);
        fit_words_to_voice(&audio, &mut w);
        let names: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(names, ["w0", "w1", "w2", "w3", "w4", "w5"]);
        let s = spans(&w);
        assert!(s.windows(2).all(|p| p[0].1 <= p[1].0 && p[0].0 <= p[0].1), "{s:?}");
    }

    /// Sound that goes on and on (a tone, a music bed under the voice) is no reason to stretch a
    /// word to the middle of every gap: a word reaches at most 500 ms past its recognised bounds
    /// (the first word of a clip reached back to the clip's start and left the sequence).
    #[test]
    fn a_tone_under_the_words_does_not_stretch_them() {
        let audio = room_with_voice(5.0, &[(0.0, 5.0)]);
        let w = fit(&audio, &[(2.0, 2.3), (3.5, 3.8)]);
        assert!(w[0].0 >= 1.49 && w[0].1 <= 2.81 && w[1].0 >= 2.99 && w[1].1 <= 4.31, "{w:?}");
    }

    /// A rough, loud start (a creaky voice, a burst: not periodic) before the voice is part of the
    /// word however long; the recogniser started this word 350 ms late.
    #[test]
    fn a_loud_rough_start_stays_in_the_word() {
        let mut audio = room_with_voice(2.5, &[(1.0, 1.6)]);
        add(&mut audio, 0.8, &noise(at(0.2), 0.12, 4));
        let w = fit(&audio, &[(1.15, 1.6)]);
        assert!(close(w[0].0, 0.8), "{w:?}");
    }

    /// Two words: each keeps to its own voice, none reaches past the middle of the gap to the other,
    /// and they stay in order. Hostile input does no harm.
    #[test]
    fn neighbours_keep_to_their_own_voice() {
        let audio = room_with_voice(3.0, &[(0.5, 1.0), (1.3, 1.9)]);
        // the recogniser ran the first word up to the second
        let w = fit(&audio, &[(0.5, 1.3), (1.3, 1.9)]);
        assert!(close(w[0].1, 1.0) && close(w[1].0, 1.3), "{w:?}");
        // touching words over one stretch of voice stay as they are
        let audio = room_with_voice(3.0, &[(0.5, 1.5)]);
        let w = fit(&audio, &[(0.5, 1.0), (1.0, 1.5)]);
        assert!(close(w[0].0, 0.5) && close(w[0].1, 1.0) && close(w[1].0, 1.0) && close(w[1].1, 1.5), "{w:?}");
        let mut odd = vec![
            Word::new("a", seconds_tick(2.5), seconds_tick(0.1)),
            Word::new("b", filmcraft_time::Tick(-9), filmcraft_time::Tick(i64::MAX)),
            Word::new("c", seconds_tick(9.0), seconds_tick(9.5)),
        ];
        fit_words_to_voice(&audio, &mut odd);
        fit_words_to_voice(&[], &mut odd);
        assert_eq!(periodicity(&[0.5; 10], 5), 0.0);
        assert!(periodicity(&voice(4000), 2000) > 0.9);
        assert!(periodicity(&noise(4000, 0.1, 4), 2000) < 0.3);
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
