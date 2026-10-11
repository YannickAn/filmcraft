//! Time-stretching for playback at other speeds (J/K/L shuttle, slow motion, `ui.playback
//! {speed}`): the sound plays faster or slower and keeps its pitch, as Preferences ▸ Audio ▸
//! "Maintain pitch while shuttling" asks.
//!
//! WSOLA, waveform-similarity overlap-add (W. Verhelst and M. Roelands, "An overlap-add technique
//! based on waveform similarity (WSOLA) for high quality time-scale modification of speech",
//! ICASSP 1993). The output is a chain of Hann-windowed input segments [`TimeStretch::hop`] output
//! frames apart, overlapping by half (the periodic Hann window sums to one). Segment `m` is centred
//! where output frame `m × hop` lands at the speed played, `start + speed × m × hop`, then moved by
//! up to ±[`TOLERANCE_S`] so that its waveform best continues the previous segment: the move
//! maximises the normalised cross-correlation between the candidate and the input that followed
//! the previous segment. Periods then line up across every overlap, so a voice keeps its pitch and
//! does not phase or flutter. At speed 1 every segment is the previous one's natural continuation
//! and the output is the input.
//!
//! The search runs on the sum of the channels (they move together and stay in step), first on a
//! copy decimated to about [`COARSE_RATE`] Hz, then at full rate around the best coarse lag, so its
//! cost per output frame does not depend on the speed.
//!
//! Input is pulled through a callback in contiguous spans of increasing position, so whatever
//! produces it (the mixer, with its effect state) sees one continuous stream; reverse playback
//! feeds it the timeline backwards. Unlike the effects, audio here is interleaved, as it is in the
//! playback path. All memory is sized at construction; [`TimeStretch::process`] never allocates.

/// Segment length (s): a few periods of a low voice.
pub const WINDOW_S: f64 = 0.024;
/// How far a segment may move from where the speed puts it (s): half the period of a 50 Hz voice.
pub const TOLERANCE_S: f64 = 0.010;
/// Rate (Hz) the coarse similarity search runs at.
pub const COARSE_RATE: u32 = 12_000;
/// Slowest speed; slower ones are clamped to it.
pub const MIN_SPEED: f64 = 1.0 / 16.0;
/// Fastest speed; faster ones are clamped to it.
pub const MAX_SPEED: f64 = 16.0;
/// Most channels stretched together (more are clamped to it).
pub const MAX_CHANNELS: usize = 64;
/// Sample rates outside this range are treated as the nearest end (sizes stay small and non-zero).
const MIN_RATE: u32 = 1_000;
const MAX_RATE: u32 = 768_000;
/// Input frames asked for at least per pull, so the producer is not called for tiny spans.
const PULL_FRAMES: usize = 1024;
/// Mean square (per frame, channels summed) below which the previous segment counts as silence and
/// the next one goes where the speed puts it.
const SILENCE: f32 = 1e-10;

/// Streaming WSOLA time-stretcher for interleaved audio.
#[derive(Debug, Clone)]
pub struct TimeStretch {
    ch: usize,
    hop: usize,
    tol: usize,
    /// Decimation factor of the coarse search.
    dec: usize,
    /// Periodic Hann window, `2 × hop` long.
    window: Vec<f32>,
    speed: f64,
    /// Input frames `[in_start, in_start + input.len() / ch)`, interleaved.
    input: Vec<f32>,
    in_start: i64,
    /// Most input frames held at once (the capacity of `input`).
    cap: usize,
    /// Input position the centre of the next segment lands on.
    center: f64,
    /// Where the last segment started: its natural continuation starts `hop` later.
    prev: Option<i64>,
    /// Overlap-add accumulator: the next `2 × hop` output frames.
    acc: Vec<f32>,
    /// Finished output frames; `[done_pos, done_len)` are still to be played.
    done: Vec<f32>,
    done_pos: usize,
    done_len: usize,
    /// Output frames to drop after a reset: the lead-in of the first segment, before the start.
    discard: usize,
    /// Search scratch: channel sums of the continuation and of the search span, and their
    /// decimated copies.
    mono_t: Vec<f32>,
    mono_x: Vec<f32>,
    dec_t: Vec<f32>,
    dec_x: Vec<f32>,
}

impl TimeStretch {
    /// A stretcher at speed 1 starting at input frame 0. A sample rate of zero or an odd one is
    /// treated as the nearest supported rate, `channels` is clamped to `1..=`[`MAX_CHANNELS`].
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let sr = sample_rate.clamp(MIN_RATE, MAX_RATE);
        let ch = channels.clamp(1, MAX_CHANNELS);
        let hop = ((WINDOW_S * f64::from(sr) / 2.0).round() as usize).max(1);
        let len = 2 * hop;
        let tol = (TOLERANCE_S * f64::from(sr)).round() as usize;
        let dec = ((sr / COARSE_RATE) as usize).max(1);
        let window = (0..len).map(|k| (0.5 - 0.5 * (std::f64::consts::TAU * k as f64 / len as f64).cos()) as f32).collect();
        // what one segment can need at once: its span and the search either side, the previous
        // segment's continuation up to MAX_SPEED hops behind, and one pull
        let cap = len + 2 * tol + (MAX_SPEED as usize + 1) * hop + PULL_FRAMES;
        let mut s = Self {
            ch,
            hop,
            tol,
            dec,
            window,
            speed: 1.0,
            input: Vec::with_capacity(cap * ch),
            in_start: 0,
            cap,
            center: 0.0,
            prev: None,
            acc: vec![0.0; len * ch],
            done: vec![0.0; hop * ch],
            done_pos: 0,
            done_len: 0,
            discard: 0,
            mono_t: vec![0.0; len],
            mono_x: vec![0.0; len + 2 * tol],
            dec_t: vec![0.0; len / dec],
            dec_x: vec![0.0; (len + 2 * tol) / dec],
        };
        s.reset(0.0);
        s
    }

    /// Interleaved channels of the input and the output.
    pub fn channels(&self) -> usize {
        self.ch
    }

    /// Output frames between segment starts (half a segment).
    pub fn hop(&self) -> usize {
        self.hop
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// Input frames per output frame for the segments made from now on: the magnitude, clamped to
    /// [`MIN_SPEED`]`..=`[`MAX_SPEED`]; not finite = 1. (Backwards is the caller's: it feeds the
    /// input reversed.) Changing it while playing is seamless.
    pub fn set_speed(&mut self, speed: f64) {
        self.speed = if speed.is_finite() { speed.abs().clamp(MIN_SPEED, MAX_SPEED) } else { 1.0 };
    }

    /// Start afresh at input position `start` (in frames; not finite = 0): the next output frame
    /// is the sound at `start`, and output frame `k` after it the sound near `start + speed × k`.
    pub fn reset(&mut self, start: f64) {
        let start = if start.is_finite() { start } else { 0.0 };
        self.center = start;
        self.prev = None;
        self.input.clear();
        // the first segment begins half a segment before `start`; the search may look `tol` earlier
        self.in_start = frame_at(start - self.hop as f64).saturating_sub(self.tol as i64);
        self.acc.fill(0.0);
        self.done_pos = 0;
        self.done_len = 0;
        self.discard = self.hop;
    }

    /// Fill `out` (interleaved, [`Self::channels`] channels; a trailing partial frame is zeroed)
    /// with the next output frames. `pull(start, buf)` fills `buf` (interleaved, zeroed) with input
    /// frames from `start` on; spans are contiguous and their positions increase.
    pub fn process(&mut self, out: &mut [f32], mut pull: impl FnMut(i64, &mut [f32])) {
        let ch = self.ch;
        let whole = out.len() / ch * ch;
        let (frames, tail) = out.split_at_mut(whole);
        tail.fill(0.0);
        let mut o = 0;
        while o < frames.len() {
            if self.done_pos >= self.done_len {
                self.segment(&mut pull);
                continue;
            }
            let avail = self.done_len - self.done_pos;
            if self.discard > 0 {
                let d = self.discard.min(avail);
                self.discard -= d;
                self.done_pos += d;
                continue;
            }
            let take = avail.min((frames.len() - o) / ch);
            let src = self.done.get(self.done_pos * ch..(self.done_pos + take) * ch).unwrap_or_default();
            if let Some(dst) = frames.get_mut(o..o + take * ch) {
                if src.len() == dst.len() {
                    dst.copy_from_slice(src);
                } else {
                    dst.fill(0.0);
                }
            }
            o += take * ch;
            self.done_pos += take;
        }
    }

    /// Choose, window and overlap-add the next segment; its first `hop` output frames are done.
    fn segment(&mut self, pull: &mut impl FnMut(i64, &mut [f32])) {
        let (hop, tol) = (self.hop as i64, self.tol as i64);
        let len = 2 * hop;
        let nominal = frame_at(self.center - self.hop as f64);
        let start = match self.prev {
            None => {
                // keep `tol` before it: the next segment's search can reach back that far
                self.ensure(nominal.saturating_sub(tol), nominal.saturating_add(len), pull);
                nominal
            }
            Some(p) => {
                let follow = p.saturating_add(hop);
                let lo = nominal.saturating_sub(tol).min(follow);
                let hi = nominal.saturating_add(tol + len).max(follow.saturating_add(len));
                self.ensure(lo, hi, pull);
                self.best_start(follow, nominal)
            }
        };
        let ch = self.ch;
        // frames before the held input (none, as `ensure` holds them) add silence
        let before = usize::try_from(self.in_start.saturating_sub(start)).unwrap_or(0);
        let seg = offset(self.in_start, start.saturating_add(before as i64), ch).and_then(|o| self.input.get(o..)).unwrap_or_default();
        for ((a, x), w) in self.acc.chunks_exact_mut(ch).skip(before).zip(seg.chunks_exact(ch)).zip(self.window.iter().skip(before)) {
            for (a, x) in a.iter_mut().zip(x) {
                *a += w * x;
            }
        }
        let half = self.hop * ch;
        if let (Some(done), Some(head)) = (self.done.get_mut(..half), self.acc.get(..half)) {
            done.copy_from_slice(head);
        }
        self.acc.copy_within(half.., 0);
        if let Some(rest) = self.acc.get_mut(half..) {
            rest.fill(0.0);
        }
        self.done_pos = 0;
        self.done_len = self.hop;
        self.prev = Some(start);
        self.center += self.speed * self.hop as f64;
    }

    /// Hold input frames `[lo, hi)`: drop older ones, pull newer ones (contiguously, so frames
    /// before `lo` that were never held are pulled and dropped rather than skipped).
    fn ensure(&mut self, lo: i64, hi: i64, pull: &mut impl FnMut(i64, &mut [f32])) {
        let ch = self.ch;
        loop {
            self.trim(lo);
            let held = self.input.len() / ch;
            let end = self.in_start.saturating_add(held as i64);
            if end >= hi {
                return;
            }
            let want = usize::try_from(hi.saturating_sub(end)).unwrap_or(usize::MAX).max(PULL_FRAMES).min(self.cap.saturating_sub(held));
            if want == 0 {
                return;
            }
            let old = self.input.len();
            self.input.resize(old + want * ch, 0.0);
            if let Some(buf) = self.input.get_mut(old..) {
                pull(end, buf);
            }
        }
    }

    /// Drop held input frames before `lo`.
    fn trim(&mut self, lo: i64) {
        let held = (self.input.len() / self.ch) as i64;
        let drop = lo.saturating_sub(self.in_start).clamp(0, held);
        if drop > 0 {
            self.input.drain(..drop as usize * self.ch);
            self.in_start += drop;
        }
    }

    /// Where the segment near `nominal` (± tolerance) best continues the input from `follow`.
    fn best_start(&mut self, follow: i64, nominal: i64) -> i64 {
        let lo = nominal.saturating_sub(self.tol as i64);
        mono(&self.input, self.in_start, self.ch, follow, &mut self.mono_t);
        let energy: f32 = self.mono_t.iter().map(|x| x * x).sum();
        if energy.is_nan() || energy <= SILENCE * self.mono_t.len() as f32 {
            return nominal;
        }
        mono(&self.input, self.in_start, self.ch, lo, &mut self.mono_x);
        decimate(&self.mono_t, self.dec, &mut self.dec_t);
        decimate(&self.mono_x, self.dec, &mut self.dec_x);
        let coarse = best_lag(&self.dec_t, &self.dec_x, 0) * self.dec;
        let (from, to) = (coarse.saturating_sub(self.dec), (coarse + self.dec).min(2 * self.tol));
        let lag = best_lag(&self.mono_t, self.mono_x.get(from..(to + self.mono_t.len()).min(self.mono_x.len())).unwrap_or_default(), from);
        lo.saturating_add(lag as i64)
    }
}

/// The frame nearest `x` (saturating; NaN = 0).
fn frame_at(x: f64) -> i64 {
    x.round() as i64
}

/// Sample offset of frame `pos` in a buffer that starts at frame `start`.
fn offset(start: i64, pos: i64, ch: usize) -> Option<usize> {
    usize::try_from(pos.checked_sub(start)?).ok()?.checked_mul(ch)
}

/// `dst[k]` = the sum of the channels of frame `pos + k` (zero where not held).
fn mono(input: &[f32], in_start: i64, ch: usize, pos: i64, dst: &mut [f32]) {
    let before = usize::try_from(in_start.saturating_sub(pos)).unwrap_or(0).min(dst.len());
    let (head, rest) = dst.split_at_mut(before);
    head.fill(0.0);
    let src = offset(in_start, pos.saturating_add(before as i64), ch).and_then(|o| input.get(o..)).unwrap_or_default();
    let mut frames = src.chunks_exact(ch);
    for d in rest {
        *d = frames.next().map_or(0.0, |f| f.iter().sum());
    }
}

/// `dst[j]` = the sum of `src[j × d .. (j + 1) × d]`.
fn decimate(src: &[f32], d: usize, dst: &mut [f32]) {
    let mut groups = src.chunks_exact(d.max(1));
    for x in dst.iter_mut() {
        *x = groups.next().map_or(0.0, |g| g.iter().sum());
    }
}

/// Dot product with eight partial sums, so it vectorises.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (a.get(..n).unwrap_or_default(), b.get(..n).unwrap_or_default());
    let mut acc = [0.0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
            *s += x * y;
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

/// `base` plus the lag of `x` whose `t.len()`-long window best matches `t` (normalised
/// cross-correlation, in phase); `base` when nothing matches.
fn best_lag(t: &[f32], x: &[f32], base: usize) -> usize {
    let n = t.len();
    let Some(lags) = x.len().checked_sub(n) else { return base };
    let mut energy: f32 = x.iter().take(n).map(|v| v * v).sum();
    let mut best = (f32::NEG_INFINITY, 0);
    for lag in 0..=lags {
        let score = dot(t, x.get(lag..).unwrap_or_default()) / (energy.max(0.0) + 1e-20).sqrt();
        if score > best.0 {
            best = (score, lag);
        }
        if let (Some(out), Some(inn)) = (x.get(lag), x.get(lag + n)) {
            energy += inn * inn - out * out;
        }
    }
    base + best.1
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Interleaved input: channel `c` is a sine of `freqs[c]` Hz from frame 0 to `len`, silent
    /// before and after.
    fn tones(freqs: &[f64], len: i64) -> impl FnMut(i64, &mut [f32]) + use<> {
        let freqs = freqs.to_vec();
        move |start, buf| {
            for (k, f) in buf.chunks_exact_mut(freqs.len()).enumerate() {
                let t = start + k as i64;
                for (x, hz) in f.iter_mut().zip(&freqs) {
                    *x = if (0..len).contains(&t) { (0.5 * (std::f64::consts::TAU * hz * t as f64 / SR as f64).sin()) as f32 } else { 0.0 };
                }
            }
        }
    }

    fn run(s: &mut TimeStretch, frames: usize, mut pull: impl FnMut(i64, &mut [f32])) -> Vec<f32> {
        let mut out = vec![f32::NAN; frames * s.channels()];
        s.process(&mut out, &mut pull);
        out
    }

    fn channel(x: &[f32], ch: usize, c: usize) -> Vec<f32> {
        x.iter().skip(c).step_by(ch).copied().collect()
    }

    /// Frequency from the spacing of upward zero crossings (interpolated).
    fn frequency(x: &[f32]) -> f64 {
        let ups: Vec<f64> =
            x.windows(2).enumerate().filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0).map(|(i, w)| i as f64 + f64::from(w[0] / (w[0] - w[1]))).collect();
        let (first, last) = (ups[0], ups[ups.len() - 1]);
        (ups.len() - 1) as f64 * SR as f64 / (last - first)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn speed_one_is_the_input() {
        let mut s = TimeStretch::new(SR, 2);
        s.reset(1000.0);
        let out = run(&mut s, 9000, tones(&[440.0, 3_000.0], 1 << 40));
        let mut want = vec![0.0; out.len()];
        tones(&[440.0, 3_000.0], 1 << 40)(1000, &mut want);
        let err = out.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(err < 1e-5, "max error {err}");
    }

    /// The point of it: faster or slower, a tone keeps its frequency, and a one-second tone lasts
    /// one second divided by the speed.
    #[test]
    fn a_sine_keeps_its_pitch_and_its_duration_scales() {
        for speed in [2.0, 4.0, 0.5, 8.0, 0.25, 1.5] {
            for hz in [110.0, 440.0, 2_500.0] {
                let mut s = TimeStretch::new(SR, 1);
                s.set_speed(speed);
                let len = SR as i64; // one second of tone
                let frames = (2.0 * SR as f64 / speed) as usize + 4096;
                let out = run(&mut s, frames, tones(&[hz], len));
                let expect = SR as f64 / speed;
                // where the tone stops: the last frame above half its amplitude
                let end = out.iter().rposition(|v| v.abs() > 0.25).unwrap() as f64;
                assert!((end - expect).abs() < 0.03 * expect + 1200.0, "{speed}x {hz} Hz: lasts {end} frames, want {expect}");
                let steady = &out[2400..(expect as usize).saturating_sub(2400)];
                let f = frequency(steady);
                assert!((f - hz).abs() < 0.01 * hz, "{speed}x {hz} Hz: plays at {f} Hz");
                // segments line up: no dips where they overlap, from the very first frame
                let level = rms(steady);
                assert!((level - 0.5 / 2f32.sqrt()).abs() < 0.03, "{speed}x {hz} Hz: rms {level}");
                for (i, w) in out[..steady.len() + 2400].chunks(480).enumerate() {
                    assert!(rms(w) > 0.3, "{speed}x {hz} Hz: a dip to {} at frame {}", rms(w), i * 480);
                }
            }
        }
    }

    #[test]
    fn channels_stay_apart_and_in_step() {
        let mut s = TimeStretch::new(44_100, 3);
        s.set_speed(2.0);
        let out = run(&mut s, 20_000, tones(&[300.0, 500.0, 300.0], 1 << 40));
        let (a, b, c) = (channel(&out, 3, 0), channel(&out, 3, 1), channel(&out, 3, 2));
        // tones() writes at 48 kHz phases; the stretcher only sees samples, so read them as such
        assert!((frequency(&a[2000..]) - 300.0).abs() < 3.0);
        assert!((frequency(&b[2000..]) - 500.0).abs() < 5.0);
        assert_eq!(a, c, "identical channels stay identical");
    }

    /// Output does not depend on how the stream is cut into blocks.
    #[test]
    fn block_size_does_not_matter() {
        let mut whole = TimeStretch::new(SR, 2);
        whole.set_speed(2.0);
        let want = run(&mut whole, 30_000, tones(&[220.0, 330.0], 1 << 40));
        let mut s = TimeStretch::new(SR, 2);
        s.set_speed(2.0);
        let mut got = Vec::new();
        let mut pull = tones(&[220.0, 330.0], 1 << 40);
        for n in [1usize, 7, 0, 512, 3, 4096].iter().cycle() {
            if got.len() >= want.len() {
                break;
            }
            let n = (*n).min((want.len() - got.len()) / 2);
            let mut buf = vec![0.0; n * 2];
            s.process(&mut buf, &mut pull);
            got.extend_from_slice(&buf);
        }
        assert_eq!(got, want);
    }

    /// The input is pulled contiguously, and after `k` output frames it has reached about
    /// `speed × k` (plus the look-ahead), also across speed changes.
    #[test]
    fn pulls_are_contiguous_and_follow_the_speed() {
        let mut s = TimeStretch::new(SR, 2);
        s.reset(500.0);
        let spans = std::cell::RefCell::new(Vec::new());
        let mut pull = |start: i64, buf: &mut [f32]| {
            spans.borrow_mut().push((start, buf.len() as i64 / 2));
            tones(&[200.0, 200.0], 1 << 40)(start, buf);
        };
        let mut played = 0.0;
        for speed in [2.0, 0.5, 4.0, 1.0, 0.25] {
            s.set_speed(speed);
            let mut buf = vec![0.0; 9600 * 2];
            s.process(&mut buf, &mut pull);
            played += speed * 9600.0;
            assert!(buf.iter().all(|x| x.is_finite()));
            let spans = spans.borrow();
            let reach = spans.last().map(|(a, n)| a + n).unwrap() as f64;
            let ahead = reach - (500.0 + played);
            assert!((-3_000.0..7_000.0).contains(&ahead), "{speed}x: input reached {reach}, played {played}");
        }
        let spans = spans.into_inner();
        assert!(spans.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0), "contiguous");
    }

    /// A voice-like tone (a 118 Hz fundamental with twenty harmonics, in syllables) keeps its
    /// fundamental: the segments follow its periods.
    #[test]
    fn a_voice_keeps_its_fundamental() {
        let f0 = 118.0;
        let voice = move |start: i64, buf: &mut [f32]| {
            for (k, x) in buf.iter_mut().enumerate() {
                let t = (start + k as i64) as f64 / SR as f64;
                let syllable = 0.3 + 0.7 * (std::f64::consts::TAU * 2.0 * t).sin().abs();
                let tone: f64 = (1..=20).map(|h| (std::f64::consts::TAU * f0 * h as f64 * t).sin() / h as f64).sum();
                *x = (0.2 * syllable * tone) as f32;
            }
        };
        for speed in [2.0, 4.0, 0.5] {
            let mut s = TimeStretch::new(SR, 1);
            s.set_speed(speed);
            let out = run(&mut s, 48_000, voice);
            // fundamental: the first lag whose normalised autocorrelation is near the best one
            for w in out[4800..].as_chunks::<4096>().0.iter().step_by(2) {
                let r = |lag: usize| {
                    let (a, b) = (&w[..4096 - 480], &w[lag..lag + 4096 - 480]);
                    dot(a, b) / (dot(a, a) * dot(b, b)).sqrt().max(1e-20)
                };
                let lags = 120..=480; // 400 … 100 Hz
                let best = lags.clone().map(r).fold(f32::MIN, f32::max);
                let mut lag = lags.clone().find(|l| r(*l) > 0.9 * best).unwrap();
                while r(lag + 1) > r(lag) {
                    lag += 1;
                }
                let hz = SR as f64 / lag as f64;
                assert!((hz - f0).abs() < 0.01 * f0, "{speed}x: fundamental {hz} Hz");
            }
        }
    }

    /// The search costs the same per output frame at any speed: well under real time.
    #[test]
    fn stretching_runs_far_faster_than_real_time() {
        for speed in [2.0, 8.0] {
            let mut s = TimeStretch::new(SR, 2);
            s.set_speed(speed);
            let t0 = std::time::Instant::now();
            let out = run(&mut s, 10 * SR as usize, tones(&[150.0, 900.0], 1 << 40));
            let took = t0.elapsed().as_secs_f64();
            assert!(out.iter().all(|x| x.is_finite()));
            assert!(took < 3.0, "{speed}x: 10 s of stereo took {took:.2} s");
        }
    }

    /// Hostile sizes and values: nothing panics, the output is finite and sized as asked.
    #[test]
    fn odd_rates_channels_speeds_and_buffers_never_panic() {
        for sr in [0, 1, 7, 999, 8_000, 22_050, 44_100, 88_200, 192_000, 768_001, u32::MAX] {
            for ch in [0, 1, 2, 3, 6, 65, usize::MAX] {
                let mut s = TimeStretch::new(sr, ch);
                let c = s.channels();
                for speed in [f64::NAN, f64::INFINITY, -2.0, 0.0, 1e-300, 1e300, 3.7, 0.3] {
                    s.set_speed(speed);
                    for start in [0.0, -1e9, f64::NAN, 1e300, f64::NEG_INFINITY] {
                        s.reset(start);
                        for n in [0, 1, c, 3 * c + 1, 5000] {
                            let mut out = vec![0.0; n];
                            s.process(&mut out, |_, b| b.fill(0.25));
                            assert!(out.iter().all(|x| x.is_finite()), "sr {sr} ch {ch} speed {speed} start {start}");
                        }
                    }
                }
                // a producer that writes nothing, or NaN
                let mut out = vec![0.0; 3000 * c];
                s.process(&mut out, |_, _| {});
                s.process(&mut out, |_, b| b.fill(f32::NAN));
            }
        }
    }
}
