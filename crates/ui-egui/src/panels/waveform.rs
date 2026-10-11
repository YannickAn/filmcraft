//! Audio waveforms for the Timeline and the Source Monitor: a summary of a sound, made once in the
//! background, that is drawn at any zoom in time proportional to the pixels drawn.
//!
//! The summary keeps, for every channel and every 256 samples at 48 kHz (5.3 ms), the peak (the
//! largest |sample|) and the RMS level. Coarser levels, each 4× coarser than the one before, let a
//! zoomed-out clip read a few cells per pixel instead of thousands. Levels are stored as 16-bit
//! fractions of +12 dBFS (steps of −84 dBFS), so a two-hour stereo recording takes about 14 MB.
//!
//! Drawing (Premiere's look): the peak in the clip's waveform colour, the RMS level over it in a
//! lighter tint, so loud and quiet passages, speech and music look different; a logarithmic (dB)
//! scale (View ▸ Dynamic Audio Waveforms) shows quiet passages too. A stereo clip on a tall
//! enough track gets one lane per channel. The outline is a triangle strip with a one-point
//! feather on its edges, so it is anti-aliased at any slope.

use egui::{Color32, Mesh, pos2};

/// Samples (at 48 kHz) per cell of the finest level.
pub const BUCKET: usize = 256;
/// The largest level stored: +12 dBFS (a hot mix, a float source).
const FULL: f32 = 3.981_072;
/// Each level has a cell per this many cells of the level below.
const FACTOR: usize = 4;
/// At most this many channels are kept (a file can claim any number).
const MAX_CHANNELS: usize = 32;
/// At most this many cells (all channels) at the finest level: about 24 hours of stereo. The
/// duration comes from the media file, so it is not trusted.
const MAX_CELLS: usize = 1 << 25;
/// The quietest level the logarithmic scale shows, in dBFS.
pub const DB_FLOOR: f32 = -48.0;
/// A channel gets a lane of its own when every lane is at least this tall (points).
pub const MIN_LANE_H: f32 = 24.0;
/// At most this many lanes; a sound with more channels is drawn in one.
const MAX_LANES: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cell {
    peak: u16,
    rms: u16,
}

/// A level (1.0 = 0 dBFS) as stored: anything not a positive finite number is silence.
fn quantize(v: f32) -> u16 {
    if !(v.is_finite() && v > 0.0) {
        return 0;
    }
    (v / FULL * 65535.0).round().min(65535.0) as u16
}

fn level_of(q: u16) -> f32 {
    q as f32 * (FULL / 65535.0)
}

/// Peak and RMS level over a stretch of sound (1.0 = 0 dBFS).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level {
    pub peak: f32,
    pub rms: f32,
}

/// The waveform summary of one sound (an audio stream of a media file, or a nested sequence's mix).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Waveform {
    channels: usize,
    /// `levels[k][cell * channels + channel]`; level 0 has a cell per [`BUCKET`] samples at 48 kHz,
    /// level k one per 4^k of those.
    levels: Vec<Vec<Cell>>,
    /// The largest |sample| of the whole sound.
    peak: f32,
}

impl Waveform {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Cells of the finest level (each [`BUCKET`] samples at 48 kHz).
    pub fn len(&self) -> usize {
        self.levels.first().map_or(0, |l| l.len() / self.channels.max(1))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The largest |sample| of the whole sound (1.0 = 0 dBFS).
    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// Memory the summary takes, in bytes.
    pub fn bytes(&self) -> usize {
        self.levels.iter().map(|l| l.len() * std::mem::size_of::<Cell>()).sum()
    }

    /// Peak and RMS level of channel `channel` (`None`: the loudest channel at each moment) over the
    /// cells `a..b` of the finest level (fractions of a cell allowed). A range narrower than a cell
    /// reads the cell under its middle. None outside the sound, for a channel it does not have, or
    /// for a range that is not finite.
    pub fn level(&self, channel: Option<usize>, a: f64, b: f64) -> Option<Level> {
        let n = self.len();
        if n == 0 || !a.is_finite() || !b.is_finite() || channel.is_some_and(|c| c >= self.channels) {
            return None;
        }
        let (a, b) = (a.min(b).max(0.0), a.max(b).min(n as f64));
        if a >= n as f64 || b <= 0.0 {
            return None;
        }
        // the coarsest level that still has at least FACTOR cells across the range
        let span = b - a;
        let (mut k, mut size) = (0usize, 1.0f64);
        while k + 1 < self.levels.len() && size * (FACTOR * FACTOR) as f64 <= span {
            k += 1;
            size *= FACTOR as f64;
        }
        let cells = self.levels.get(k)?;
        let count = cells.len() / self.channels.max(1);
        if count == 0 {
            return None;
        }
        let (i0, i1) = if span < 1.0 {
            let m = ((a + b) / 2.0).floor() as usize;
            (m, m + 1)
        } else {
            ((a / size).floor() as usize, (b / size).ceil() as usize)
        };
        let i0 = i0.min(count - 1);
        let i1 = i1.max(i0 + 1).min(count);
        let mut out = Level::default();
        let channels = match channel {
            Some(c) => c..c + 1,
            None => 0..self.channels,
        };
        for c in channels {
            let (mut peak, mut sum, mut k) = (0u16, 0f64, 0u32);
            for i in i0..i1 {
                if let Some(cell) = cells.get(i * self.channels + c) {
                    peak = peak.max(cell.peak);
                    let r = level_of(cell.rms) as f64;
                    sum += r * r;
                    k += 1;
                }
            }
            out.peak = out.peak.max(level_of(peak));
            out.rms = out.rms.max((sum / k.max(1) as f64).sqrt() as f32);
        }
        Some(out)
    }
}

/// Makes a [`Waveform`] from the samples of a sound, given in pieces of any length.
pub struct Builder {
    channels: usize,
    bucket: usize,
    /// Per channel: peak and sum of squares of the cell being filled.
    acc: Vec<(f32, f64)>,
    filled: usize,
    cells: Vec<Cell>,
    peak: f32,
    max_cells: usize,
}

impl Builder {
    /// A summary of `channels` channels with a cell per `bucket` samples (256 at 48 kHz; at another
    /// sample rate as many samples as last as long).
    pub fn new(channels: usize, bucket: usize) -> Self {
        Self::with_limit(channels, bucket, MAX_CELLS)
    }

    fn with_limit(channels: usize, bucket: usize, max_cells: usize) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);
        Builder { channels, bucket: bucket.max(1), acc: vec![(0.0, 0.0); channels], filled: 0, cells: Vec::new(), peak: 0.0, max_cells }
    }

    /// Whether the summary has all it can keep: more samples are ignored.
    pub fn full(&self) -> bool {
        self.cells.len() >= self.max_cells
    }

    /// Add the next samples: `channels[c]` are channel c's. A channel shorter than the longest is
    /// silent for the rest; channels beyond those the builder was made for are left out.
    pub fn push(&mut self, channels: &[Vec<f32>]) {
        let n = channels.iter().take(self.channels).map(Vec::len).max().unwrap_or(0);
        let mut i = 0;
        while i < n && !self.full() {
            let take = (self.bucket - self.filled).min(n - i);
            for (c, (peak, sum)) in self.acc.iter_mut().enumerate() {
                let Some(samples) = channels.get(c).and_then(|ch| ch.get(i..(i + take).min(ch.len()))) else { continue };
                for v in samples.iter().filter(|v| v.is_finite()) {
                    *peak = peak.max(v.abs());
                    *sum += (*v as f64) * (*v as f64);
                }
            }
            self.filled += take;
            i += take;
            if self.filled >= self.bucket {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        if self.filled == 0 {
            return;
        }
        for (peak, sum) in &mut self.acc {
            let rms = (*sum / self.filled as f64).sqrt() as f32;
            self.cells.push(Cell { peak: quantize(*peak), rms: quantize(rms) });
            self.peak = self.peak.max(*peak);
            (*peak, *sum) = (0.0, 0.0);
        }
        self.filled = 0;
    }

    pub fn finish(mut self) -> Waveform {
        if !self.full() {
            self.flush();
        }
        let channels = self.channels;
        let mut levels = vec![std::mem::take(&mut self.cells)];
        while let Some(prev) = levels.last() {
            let n = prev.len() / channels;
            if n <= 1 {
                break;
            }
            let mut next = Vec::with_capacity(n.div_ceil(FACTOR) * channels);
            for j in 0..n.div_ceil(FACTOR) {
                for c in 0..channels {
                    let (mut peak, mut sum, mut k) = (0u16, 0f64, 0u32);
                    for i in j * FACTOR..((j + 1) * FACTOR).min(n) {
                        if let Some(cell) = prev.get(i * channels + c) {
                            peak = peak.max(cell.peak);
                            let r = level_of(cell.rms) as f64;
                            sum += r * r;
                            k += 1;
                        }
                    }
                    next.push(Cell { peak, rms: quantize((sum / k.max(1) as f64).sqrt() as f32) });
                }
            }
            levels.push(next);
        }
        Waveform { channels, levels, peak: self.peak }
    }
}

/// How high (0…1 of the lane) a level is drawn: on the logarithmic scale [`DB_FLOOR`] is the bottom
/// and 0 dBFS the top; on the linear scale the amplitude itself.
pub fn display_height(level: f32, logarithmic: bool) -> f32 {
    if !(level.is_finite() && level > 0.0) {
        return 0.0;
    }
    if logarithmic { ((20.0 * level.log10() - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0) } else { level.min(1.0) }
}

/// How many lanes a sound with `channels` channels gets in a waveform area `height` points tall:
/// one per channel when each is at least [`MIN_LANE_H`], else one (its loudest channel).
pub fn lanes(channels: usize, height: f32) -> usize {
    if (2..=MAX_LANES).contains(&channels) && height >= channels as f32 * MIN_LANE_H { channels } else { 1 }
}

/// The name of lane `lane` of `lanes` in its corner box: L / R for stereo, else the channel number.
pub fn lane_name(lane: usize, lanes: usize) -> String {
    match (lanes, lane) {
        (2, 0) => "L".into(),
        (2, 1) => "R".into(),
        _ => (lane + 1).to_string(),
    }
}

/// Add one waveform lane to `mesh`: per pixel column starting at `x0`, the heights (points) of its
/// peak and RMS level, drawn from `base` towards `dir` (−1 up, +1 down). The peak in `peak_col`,
/// the RMS level over it in `rms_col`, joined between columns (a smooth outline, not steps) and
/// feathered over a point at the top and where the colours meet. Silent stretches add nothing.
pub fn paint_profile(mesh: &mut Mesh, x0: f32, columns: &[(f32, f32)], base: f32, dir: f32, peak_col: Color32, rms_col: Color32) {
    const SILENT: f32 = 0.25;
    let n = columns.len();
    if n == 0 || !(x0.is_finite() && base.is_finite() && dir.is_finite()) {
        return;
    }
    let clean = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
    let heard = |i: usize| columns.get(i).is_some_and(|c| clean(c.0) >= SILENT);
    // vertices at the pixel centres, plus the outer edges of the first and last pixel
    let at = |i: usize| -> (f32, (f32, f32)) {
        let j = i.saturating_sub(1).min(n - 1);
        let x = if i == 0 {
            x0
        } else if i > n {
            x0 + n as f32
        } else {
            x0 + j as f32 + 0.5
        };
        let (p, r) = columns.get(j).copied().unwrap_or_default();
        let p = clean(p);
        (x, (p, clean(r).min(p)))
    };
    let mut prev: Option<u32> = None;
    for i in 0..=n + 1 {
        let j = i.saturating_sub(1).min(n - 1);
        if !heard(j) && !(j > 0 && heard(j - 1)) && !heard(j + 1) {
            prev = None;
            continue;
        }
        let (x, (p, r)) = at(i);
        let first = mesh.vertices.len() as u32;
        let y = |h: f32| base + dir * h;
        let mid = (r + 1.0).min(p);
        mesh.colored_vertex(pos2(x, base), rms_col);
        mesh.colored_vertex(pos2(x, y(r)), rms_col);
        mesh.colored_vertex(pos2(x, y(mid)), peak_col);
        mesh.colored_vertex(pos2(x, y(p)), peak_col);
        mesh.colored_vertex(pos2(x, y(p + 1.0)), Color32::TRANSPARENT);
        if let Some(a) = prev {
            for k in 0..4 {
                mesh.add_triangle(a + k, a + k + 1, first + k);
                mesh.add_triangle(a + k + 1, first + k + 1, first + k);
            }
        }
        prev = Some(first);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, freq: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / 48_000.0).sin()).collect()
    }

    fn db(v: f32) -> f32 {
        20.0 * v.log10()
    }

    #[test]
    fn levels_are_stored_to_a_fraction_of_a_db() {
        // within 0.1 dB down to −40 dBFS (the scale shows down to −48), within 0.5 dB at −60
        for (v, tolerance) in [(1.0f32, 0.1), (0.5, 0.1), (0.1, 0.1), (0.01, 0.1), (0.001, 0.5), (2.0, 0.1), (FULL, 0.1)] {
            let back = level_of(quantize(v));
            assert!((db(back) - db(v)).abs() < tolerance, "{v} → {back}");
        }
        // what is not a level is silence, too loud is the top
        for v in [f32::NAN, f32::NEG_INFINITY, f32::INFINITY, -0.5, 0.0] {
            assert_eq!(quantize(v), 0, "{v}");
        }
        assert_eq!(quantize(1e30), u16::MAX);
    }

    #[test]
    fn a_sine_has_its_peak_and_its_rms() {
        let mut b = Builder::new(1, BUCKET);
        b.push(&[sine(0.5, 440.0, 48_000)]);
        let w = b.finish();
        assert_eq!(w.len(), 48_000usize.div_ceil(BUCKET));
        assert!((w.peak() - 0.5).abs() < 1e-3);
        let l = w.level(Some(0), 0.0, w.len() as f64).unwrap();
        assert!((l.peak - 0.5).abs() < 2e-3, "{l:?}");
        assert!((l.rms - 0.5 / 2f32.sqrt()).abs() < 5e-3, "{l:?}");
        // one cell: still the sine's level
        let c = w.level(Some(0), 10.0, 11.0).unwrap();
        assert!((c.rms - 0.354).abs() < 0.02 && c.peak > 0.45, "{c:?}");
    }

    #[test]
    fn the_pieces_the_samples_come_in_do_not_matter() {
        let s = sine(0.8, 97.0, 20_000);
        let mut whole = Builder::new(1, BUCKET);
        whole.push(std::slice::from_ref(&s));
        let mut pieces = Builder::new(1, BUCKET);
        for chunk in s.chunks(333) {
            pieces.push(&[chunk.to_vec()]);
        }
        assert_eq!(whole.finish(), pieces.finish());
    }

    #[test]
    fn each_channel_keeps_its_own_sound() {
        let mut b = Builder::new(2, BUCKET);
        b.push(&[sine(0.9, 220.0, 9_600), vec![0.0; 9_600]]);
        let w = b.finish();
        assert_eq!(w.channels(), 2);
        let n = w.len() as f64;
        assert!(w.level(Some(0), 0.0, n).unwrap().peak > 0.85);
        assert!(w.level(Some(1), 0.0, n).unwrap().peak < 1e-4, "the right channel is silent");
        assert!(w.level(None, 0.0, n).unwrap().peak > 0.85, "together: the loudest channel");
        assert_eq!(w.level(Some(2), 0.0, n), None, "no third channel");
    }

    /// The coarse levels give what the finest gives: the same peak, about the same RMS.
    #[test]
    fn a_zoomed_out_view_reads_the_same_levels() {
        // a level that changes every 1000 samples, so every range has its own
        let samples: Vec<f32> = (0..2_000_000).map(|i| ((i / 1000) % 17) as f32 / 17.0 * if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let mut b = Builder::new(1, BUCKET);
        b.push(&[samples]);
        let w = b.finish();
        assert!(w.levels.len() > 5, "{} levels", w.levels.len());
        let brute = |a: usize, b: usize| {
            let cells = &w.levels[0][a..b];
            let peak = cells.iter().map(|c| level_of(c.peak)).fold(0.0, f32::max);
            let rms = (cells.iter().map(|c| (level_of(c.rms) as f64).powi(2)).sum::<f64>() / cells.len() as f64).sqrt() as f32;
            (peak, rms)
        };
        for (a, b) in [(0usize, 64usize), (100, 1700), (16, 4096), (5, 7000), (3000, 7800)] {
            let l = w.level(Some(0), a as f64, b as f64).unwrap();
            let (peak, rms) = brute(a, b);
            // the coarse cells at the ends reach a little past the range
            assert!(l.peak >= peak - 1e-4, "{a}..{b}: {l:?} vs {peak}");
            assert!((l.rms - rms).abs() < 0.08, "{a}..{b}: rms {} vs {rms}", l.rms);
        }
    }

    #[test]
    fn hostile_ranges_and_inputs_do_not_panic() {
        let mut b = Builder::new(0, 0);
        b.push(&[]);
        b.push(&[vec![f32::NAN, f32::INFINITY, 0.5], vec![]]);
        let w = b.finish();
        assert_eq!(w.channels(), 1);
        assert!((w.peak() - 0.5).abs() < 1e-6, "not-a-number samples are left out");
        let w = {
            let mut b = Builder::new(2, BUCKET);
            b.push(&[sine(0.5, 300.0, 5_000), sine(0.5, 300.0, 1_000)]);
            b.finish()
        };
        for (a, b) in [(f64::NAN, 1.0), (0.0, f64::INFINITY), (-50.0, -1.0), (1e9, 2e9), (5.0, 1.0), (-1e300, 1e300), (19.9, 19.95)] {
            let _ = w.level(None, a, b);
        }
        assert_eq!(w.level(None, -50.0, -1.0), None);
        assert!(w.level(None, 5.0, 1.0).is_some(), "a reversed range is the same range");
        assert_eq!(Waveform::default().level(None, 0.0, 1.0), None);
        // a sound longer than the summary keeps: the start is kept, the rest left out
        let mut b = Builder::with_limit(1, 4, 10);
        b.push(&[vec![0.25; 1000]]);
        assert!(b.full());
        assert_eq!(b.finish().len(), 10);
    }

    #[test]
    fn the_log_scale_shows_quiet_sounds() {
        assert_eq!(display_height(1.0, true), 1.0);
        assert!((display_height(10f32.powf(-24.0 / 20.0), true) - 0.5).abs() < 1e-4, "−24 dB halfway up");
        assert_eq!(display_height(10f32.powf(-60.0 / 20.0), true), 0.0);
        // −30 dB: a quarter of the linear scale's 3 % height
        assert!(display_height(0.0316, true) > 0.35 && display_height(0.0316, false) < 0.04);
        assert_eq!(display_height(4.0, false), 1.0);
        for v in [f32::NAN, -1.0, 0.0, f32::NEG_INFINITY] {
            assert_eq!(display_height(v, true), 0.0);
        }
    }

    #[test]
    fn stereo_gets_two_lanes_on_a_tall_track() {
        assert_eq!(lanes(2, 30.0), 1);
        assert_eq!(lanes(2, 48.0), 2);
        assert_eq!(lanes(1, 400.0), 1);
        assert_eq!(lanes(6, 100.0), 1);
        assert_eq!(lanes(6, 144.0), 6);
        assert_eq!(lanes(64, 1e6), 1, "too many channels: one lane");
        assert_eq!((lane_name(0, 2), lane_name(1, 2), lane_name(3, 6)), ("L".to_string(), "R".to_string(), "4".to_string()));
    }

    /// `cargo test -p filmcraft-ui-egui --lib perf_two_hours -- --ignored --nocapture`: a two-hour
    /// stereo recording on a clip 1600 points wide (zoomed out), every column read per frame, against
    /// scanning the finest cells as the Timeline did before the coarser levels.
    #[test]
    #[ignore]
    fn perf_two_hours_of_stereo_zoomed_out() {
        let cells = 2 * 3600 * 48_000 / BUCKET;
        // a cell per sample (bucket 1) makes the summary fast to build here
        let mut b = Builder::new(2, 1);
        let ch: Vec<f32> = (0..cells).map(|i| ((i * 7919) % 1000) as f32 / 1000.0).collect();
        b.push(&[ch.clone(), ch]);
        let w = b.finish();
        let cols = 1600usize;
        let per = w.len() as f64 / cols as f64;
        let t = std::time::Instant::now();
        let mut sum = 0.0;
        for _ in 0..10 {
            for i in 0..cols {
                sum += w.level(None, i as f64 * per, (i + 1) as f64 * per).map_or(0.0, |l| l.peak + l.rms);
            }
        }
        let new = t.elapsed().as_secs_f64() * 100.0;
        let t = std::time::Instant::now();
        for _ in 0..10 {
            for i in 0..cols {
                let (a, b) = ((i as f64 * per) as usize * 2, ((i + 1) as f64 * per) as usize * 2);
                sum += w.levels[0][a..b].iter().map(|c| level_of(c.peak)).fold(0.0, f32::max);
            }
        }
        let old = t.elapsed().as_secs_f64() * 100.0;
        println!(
            "2 h stereo: {:.1} MB, {} levels; a frame of 1600 columns: {new:.3} ms (scanning the finest cells: {old:.3} ms) [{sum:.0}]",
            w.bytes() as f64 / 1e6,
            w.levels.len()
        );
    }

    #[test]
    fn the_outline_is_a_joined_strip_that_leaves_silence_out() {
        let (peak, rms) = (Color32::LIGHT_BLUE, Color32::WHITE);
        let mut m = Mesh::default();
        paint_profile(&mut m, 10.0, &[(10.0, 4.0), (12.0, 5.0), (8.0, 3.0)], 100.0, -1.0, peak, rms);
        // 3 columns plus both outer edges, 5 vertices each, joined by 4 quads
        assert_eq!(m.vertices.len(), 25);
        assert_eq!(m.indices.len(), 4 * 4 * 6);
        assert!(m.vertices.iter().all(|v| v.pos.x >= 10.0 && v.pos.x <= 13.0 && v.pos.y <= 100.0 && v.pos.y >= 100.0 - 13.0));
        assert!(m.is_valid());
        // silence in the middle: two separate pieces, nothing drawn where it is silent
        let mut m = Mesh::default();
        let cols: Vec<(f32, f32)> = (0..20).map(|i| if (5..15).contains(&i) { (0.0, 0.0) } else { (6.0, 2.0) }).collect();
        paint_profile(&mut m, 0.0, &cols, 50.0, 1.0, peak, rms);
        assert!(m.is_valid());
        assert!(m.vertices.iter().all(|v| !(6.5..13.5).contains(&v.pos.x)), "no vertices inside the silence");
        // hostile heights draw nothing wrong
        let mut m = Mesh::default();
        paint_profile(&mut m, 0.0, &[(f32::NAN, 1.0), (f32::INFINITY, -3.0), (5.0, 9.0)], 50.0, -1.0, peak, rms);
        assert!(m.is_valid() && m.vertices.iter().all(|v| v.pos.y.is_finite()));
        paint_profile(&mut m, f32::NAN, &[(5.0, 1.0)], 50.0, -1.0, peak, rms);
        paint_profile(&mut m, 0.0, &[], 50.0, -1.0, peak, rms);
    }
}
