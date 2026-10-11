//! Sound while the Program monitor plays at another speed than 1× (J/K/L shuttle, slow play,
//! `ui.playback {speed}`), as Premiere plays it: faster or slower at the same pitch with Settings ▸
//! Audio ▸ "Maintain pitch while shuttling" (on by default), time-stretched by
//! [`filmcraft_audio_dsp::stretch`]; with it off, varispeed like tape (the pitch follows the
//! speed). Backwards, the sound plays backwards.
//!
//! [`wrap`] turns the 1× mixer (`mix(cursor, buf, channels)`, the cursor in device frames on the
//! timeline) into one whose cursor counts frames played: output frame `start + k` is the timeline
//! near `start + speed × k`. The playhead follows the same clock (frames played × speed), so
//! picture and sound stay together. On desktop it runs on the play-ahead thread
//! ([`crate::play_ahead`]); a jump of its cursor (frames played as silence after an underrun) is a
//! seek, and the sound restarts where that lands on the timeline.

use filmcraft_audio_dsp::resample::StreamResampler;
use filmcraft_audio_dsp::stretch::{self, TimeStretch};

/// Fastest speed (either way) that plays sound; faster shuttling is silent.
pub const MAX_SPEED: f64 = stretch::MAX_SPEED;
/// Slowest speed (either way) that plays sound; slower is silent (the picture all but stands).
pub const MIN_SPEED: f64 = stretch::MIN_SPEED;
/// Backwards, the timeline is mixed forwards in blocks of this many frames (about 0.17 s at
/// 48 kHz), each played from its end: few enough mixer calls even at −8×.
const REVERSE_BLOCK: usize = 8192;
/// Varispeed plays `from / VARI_DENOM` timeline frames per frame.
const VARI_DENOM: u32 = 1_000_000;
/// Frames played that a cursor is taken to be at most (2⁴⁸: 186 years at 48 kHz).
const PLAYED_LIMIT: i64 = 1 << 48;

/// The 1× mixer `mix`, played from device frame `start` at `speed` (negative = backwards):
/// pitch kept when `keep_pitch`, else varispeed. `None` when `speed` plays no sound (not finite,
/// slower than [`MIN_SPEED`] or faster than [`MAX_SPEED`]) or the rate is zero.
pub fn wrap<M>(mix: M, start: i64, speed: f64, sample_rate: u32, keep_pitch: bool) -> Option<impl FnMut(i64, &mut [f32], usize) + Send + 'static>
where
    M: FnMut(i64, &mut [f32], usize) + Send + 'static,
{
    if !(MIN_SPEED..=MAX_SPEED).contains(&speed.abs()) || sample_rate == 0 {
        return None;
    }
    let mag = speed.abs();
    let mut timeline = Timeline { mix, start, reverse: speed < 0.0, block: Vec::new(), block_at: 0, block_frames: 0 };
    let mut engine = Engine::Idle;
    // the cursor the next call continues from
    let mut next = None;
    Some(move |cursor: i64, buf: &mut [f32], ch: usize| {
        let ch = ch.max(1);
        let n = buf.len() / ch;
        if ch > stretch::MAX_CHANNELS {
            buf.fill(0.0);
            return;
        }
        // frames played since the start (bounded, so that × speed stays far inside i64)
        let played = cursor.saturating_sub(start).clamp(-PLAYED_LIMIT, PLAYED_LIMIT);
        let seek = next != Some(cursor) || engine.channels() != Some(ch);
        next = Some(cursor.saturating_add(n as i64));
        if keep_pitch {
            if seek {
                let mut s = match std::mem::replace(&mut engine, Engine::Idle) {
                    Engine::Stretch(s) if s.channels() == ch => s,
                    _ => Box::new(TimeStretch::new(sample_rate, ch)),
                };
                s.set_speed(mag);
                s.reset(played as f64 * mag);
                engine = Engine::Stretch(s);
            }
            if let Engine::Stretch(s) = &mut engine {
                s.process(buf, |p, b| timeline.read(p, b, ch));
            }
            return;
        }
        if seek {
            engine = Engine::Vari(StreamResampler::new((mag * f64::from(VARI_DENOM)).round() as u32, VARI_DENOM), ch);
        }
        let Engine::Vari(r, _) = &mut engine else { return };
        let planar = r.process(played, n, |p, m| {
            let mut tmp = vec![0.0; m.saturating_mul(ch)];
            timeline.read(p, &mut tmp, ch);
            (0..ch).map(|c| tmp.iter().skip(c).step_by(ch).copied().collect()).collect()
        });
        for (i, frame) in buf.chunks_exact_mut(ch).enumerate() {
            for (c, x) in frame.iter_mut().enumerate() {
                *x = planar.get(c).and_then(|p| p.get(i)).copied().unwrap_or(0.0);
            }
        }
    })
}

/// What turns the timeline into sound at the speed played.
enum Engine {
    Idle,
    Stretch(Box<TimeStretch>),
    /// Varispeed, and the channels it was started with.
    Vari(StreamResampler, usize),
}

impl Engine {
    fn channels(&self) -> Option<usize> {
        match self {
            Engine::Idle => None,
            Engine::Stretch(s) => Some(s.channels()),
            Engine::Vari(_, ch) => Some(*ch),
        }
    }
}

/// The timeline as a stream: frame `p` is timeline frame `start + p`, or backwards `start − 1 − p`.
struct Timeline<M> {
    mix: M,
    start: i64,
    reverse: bool,
    /// Backwards: the last block mixed (interleaved, timeline order), whose last frame is stream
    /// frame `block_at`.
    block: Vec<f32>,
    block_at: i64,
    block_frames: usize,
}

impl<M: FnMut(i64, &mut [f32], usize)> Timeline<M> {
    /// Stream frames from `p` on into `buf` (interleaved, `ch` channels).
    fn read(&mut self, p: i64, buf: &mut [f32], ch: usize) {
        if !self.reverse {
            mix_at(&mut self.mix, self.start.saturating_add(p), buf, ch);
            return;
        }
        for (k, frame) in buf.chunks_exact_mut(ch).enumerate() {
            let q = p.saturating_add(k as i64);
            let held = q.checked_sub(self.block_at).and_then(|d| usize::try_from(d).ok()).filter(|d| *d < self.block_frames);
            let i = match held {
                Some(i) => i,
                None => {
                    // stream frames [q, q + B) are timeline frames [start − q − B, start − q)
                    self.block.clear();
                    self.block.resize(REVERSE_BLOCK * ch, 0.0);
                    mix_at(&mut self.mix, self.start.saturating_sub(q).saturating_sub(REVERSE_BLOCK as i64), &mut self.block, ch);
                    self.block_at = q;
                    self.block_frames = REVERSE_BLOCK;
                    0
                }
            };
            let j = self.block_frames.saturating_sub(1 + i);
            match self.block.get(j * ch..(j + 1) * ch) {
                Some(src) => frame.copy_from_slice(src),
                None => frame.fill(0.0),
            }
        }
    }
}

/// `mix` from timeline frame `t` into `buf`; frames before the timeline's start are silent.
fn mix_at(mix: &mut impl FnMut(i64, &mut [f32], usize), t: i64, buf: &mut [f32], ch: usize) {
    buf.fill(0.0);
    let before = usize::try_from(t.saturating_neg()).unwrap_or(0).min(buf.len() / ch);
    if let Some(rest) = buf.get_mut(before * ch..).filter(|r| !r.is_empty()) {
        mix(t.saturating_add(before as i64), rest, ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const SR: u32 = 48_000;

    /// A timeline that plays 300 Hz before frame 72 000 and 600 Hz from there (stereo, both
    /// channels alike), silent before 0; records the spans it was asked for.
    fn two_tones(calls: Arc<Mutex<Vec<(i64, usize)>>>) -> impl FnMut(i64, &mut [f32], usize) + Send + 'static {
        move |cursor, buf, ch| {
            calls.lock().unwrap().push((cursor, buf.len() / ch));
            for (k, f) in buf.chunks_mut(ch).enumerate() {
                let t = cursor.saturating_add(k as i64);
                let hz = if t < 72_000 { 300.0 } else { 600.0 };
                let x = if t < 0 { 0.0 } else { (0.5 * (std::f64::consts::TAU * hz * t as f64 / SR as f64).sin()) as f32 };
                f.fill(x);
            }
        }
    }

    /// Plays `frames` from `start` in device-sized calls; returns the left channel.
    fn play(fill: &mut impl FnMut(i64, &mut [f32], usize), start: i64, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut cursor = start;
        while out.len() < frames {
            let mut buf = vec![f32::NAN; 1024 * 2];
            fill(cursor, &mut buf, 2);
            cursor += 1024;
            out.extend(buf.iter().step_by(2));
        }
        out
    }

    fn frequency(x: &[f32]) -> f64 {
        let ups: Vec<f64> =
            x.windows(2).enumerate().filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0).map(|(i, w)| i as f64 + f64::from(w[0] / (w[0] - w[1]))).collect();
        (ups.len() - 1) as f64 * SR as f64 / (ups[ups.len() - 1] - ups[0])
    }

    fn near(f: f64, hz: f64) -> bool {
        (f - hz).abs() < 0.01 * hz
    }

    /// 2× forwards: the same tones at their own pitch, each for half as long.
    #[test]
    fn double_speed_keeps_the_pitch_and_halves_the_time() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut fill = wrap(two_tones(calls.clone()), 48_000, 2.0, SR, true).unwrap();
        let out = play(&mut fill, 48_000, 24_000);
        // timeline 48 000‥72 000 (300 Hz) plays in 12 000 frames, then 600 Hz
        assert!(near(frequency(&out[1_000..11_000]), 300.0), "{}", frequency(&out[1_000..11_000]));
        assert!(near(frequency(&out[13_000..23_000]), 600.0), "{}", frequency(&out[13_000..23_000]));
        let calls = calls.lock().unwrap();
        assert!(calls.windows(2).all(|w| w[0].0 + w[0].1 as i64 == w[1].0), "the mixer sees one continuous stream");
    }

    /// Settings ▸ Audio ▸ "Maintain pitch while shuttling" off: varispeed, an octave up at 2×.
    #[test]
    fn without_maintain_pitch_the_pitch_follows_the_speed() {
        let mut fill = wrap(two_tones(Arc::default()), 48_000, 2.0, SR, false).unwrap();
        let out = play(&mut fill, 48_000, 24_000);
        assert!(near(frequency(&out[1_000..11_000]), 600.0));
        assert!(near(frequency(&out[13_000..23_000]), 1_200.0));
    }

    /// −2× from 96 000: the 600 Hz part (timeline 96 000 → 72 000) first, then the 300 Hz part,
    /// both at their own pitch.
    #[test]
    fn reverse_plays_backwards_at_the_same_pitch() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut fill = wrap(two_tones(calls.clone()), 96_000, -2.0, SR, true).unwrap();
        let out = play(&mut fill, 96_000, 24_000);
        assert!(near(frequency(&out[1_000..11_000]), 600.0), "{}", frequency(&out[1_000..11_000]));
        assert!(near(frequency(&out[13_000..23_000]), 300.0), "{}", frequency(&out[13_000..23_000]));
        let calls = calls.lock().unwrap();
        assert!(calls.windows(2).all(|w| w[1].0 < w[0].0), "mixed in blocks going back: {calls:?}");
        // and to 0.5× backwards with varispeed (an octave down)
        let mut fill = wrap(two_tones(Arc::default()), 96_000, -0.5, SR, false).unwrap();
        let out = play(&mut fill, 96_000, 24_000);
        assert!(near(frequency(&out[1_000..23_000]), 300.0));
    }

    /// Frames played as silence after an underrun move the device cursor on: the sound picks up
    /// where that lands on the timeline (start + speed × frames played).
    #[test]
    fn a_cursor_jump_lands_where_the_speed_puts_it() {
        let mut fill = wrap(two_tones(Arc::default()), 0, 4.0, SR, true).unwrap();
        let before = play(&mut fill, 0, 4096);
        assert!(near(frequency(&before[500..]), 300.0));
        // played 20 000 frames → timeline 80 000: 600 Hz
        let after = play(&mut fill, 20_000, 8192);
        assert!(near(frequency(&after[500..]), 600.0), "{}", frequency(&after[500..]));
    }

    #[test]
    fn speeds_without_sound_and_odd_buffers() {
        for speed in [0.0, f64::NAN, f64::INFINITY, -f64::INFINITY, 16.5, -100.0, 1e-9, -0.06] {
            assert!(wrap(two_tones(Arc::default()), 0, speed, SR, true).is_none(), "{speed}");
        }
        assert!(wrap(two_tones(Arc::default()), 0, 2.0, 0, true).is_none());
        for keep in [true, false] {
            for speed in [16.0, -16.0, 0.0625, -1.0, 1.0, 3.3] {
                let mut fill = wrap(two_tones(Arc::default()), i64::MAX - 10, speed, SR, keep).unwrap();
                for (cursor, len, ch) in [(0, 0, 0), (i64::MIN, 7, 2), (i64::MAX, 3000, 3), (5, 100, 65), (5, 101, 1), (9, 2048, 6)] {
                    let mut buf = vec![0.0; len];
                    fill(cursor, &mut buf, ch);
                    assert!(buf.iter().all(|x| x.is_finite()));
                }
            }
        }
    }

    type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

    /// An output that keeps the callback it was started with.
    struct Out(Arc<Mutex<Option<Fill>>>);

    impl crate::AudioOut for Out {
        fn start(&mut self, fill: Fill) -> Result<u32, String> {
            *self.0.lock().unwrap() = Some(fill);
            Ok(SR)
        }
        fn stop(&mut self) {
            *self.0.lock().unwrap() = None;
        }
        fn sample_rate(&self) -> u32 {
            SR
        }
        fn played_frames(&self) -> Option<u64> {
            None
        }
    }

    /// The demo project's music at 2× (L L) and in slow reverse: playback used to go silent at any
    /// speed but 1×.
    #[test]
    fn the_program_plays_its_sound_at_other_speeds() {
        for (speed, keep, sound) in [(2.0, true, true), (4.0, true, true), (-2.0, true, true), (0.25, false, true), (-0.25, true, true), (32.0, true, false)] {
            let mut s = filmcraft_engine::Session::default();
            s.execute("file.openDemoProject", serde_json::json!({})).unwrap();
            s.prefs.audio.maintain_pitch_shuttling = keep;
            let mut app = crate::FilmcraftApp::new(s);
            let fill = Arc::new(Mutex::new(None));
            app.audio = Some(Box::new(Out(fill.clone())));
            let rate = app.session.sequence_rate();
            app.session.set_playhead(rate.tick_of(60));
            app.playback.speed = speed;
            app.start_audio();
            assert_eq!(app.playback.audio_clock, sound, "{speed}x: the device clock drives");
            let mut g = fill.lock().unwrap();
            let Some(f) = g.as_mut() else {
                assert!(!sound, "{speed}x: no stream");
                continue;
            };
            let mut buf = vec![0.0; 1024 * 2];
            f(&mut buf, 2);
            let level: f32 = buf.iter().map(|x| x.abs()).sum();
            assert!(level > 1.0, "{speed}x: the music is heard ({level})");
        }
    }
}
