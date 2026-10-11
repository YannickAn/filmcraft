//! Text-based editing: the sequence transcript and the edits made through it.
//!
//! Clip transcripts (`filmcraft_project::Transcript`) hold words in **media time**. The sequence
//! transcript ([`sequence_words`]) maps them through the audio track items that play them:
//!
//! - audio tracks are read top first (A1, A2…); a word is taken from the first track whose
//!   transcribed clip covers the word's midpoint, so a dialogue clip duplicated on two tracks (or a
//!   stereo pair split over two mono tracks) reads once;
//! - a word belongs to a clip when its midpoint, mapped through the clip's speed, falls inside the
//!   clip; its timeline bounds are clamped to the clip, so a word cut by an edit is shown cut;
//! - disabled clips, reversed clips and frame holds contribute no words (they don't play speech).
//!
//! Text edits turn word ranges into timeline ranges ([`word_range`]) snapped outward to frames, then
//! reuse the ordinary [`crate::extract`] / [`crate::lift`] edits. Pause and filler-word removal
//! ([`find_pauses`], [`find_fillers`]) produce many ranges that [`ripple_delete_ranges`] removes in
//! one pass, right to left. Captions come from [`caption_blocks`].

use std::collections::BTreeMap;
use std::sync::Arc;

use filmcraft_project::{Caption, ClipId, ItemId, Sequence, TrackId, Transcript};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick, TimeRange};

use crate::EditCtx;

/// A word of the sequence transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct SeqWord {
    pub text: String,
    /// Sequence time.
    pub start: Tick,
    pub end: Tick,
    /// The track item it is heard through and that item's media.
    pub clip: ClipId,
    pub item: ItemId,
    /// Index of the word in the media item's transcript.
    pub index: usize,
    /// Audio track index (0 = A1).
    pub track: usize,
    pub speaker: Option<String>,
    pub confidence: f32,
}

impl SeqWord {
    pub fn normalized(&self) -> String {
        filmcraft_project::transcript::normalize_word(&self.text)
    }
    pub fn range(&self) -> TimeRange {
        TimeRange::from_bounds(self.start, self.end.max(self.start))
    }
}

/// Transcripts by media item (as in `Project::transcripts`).
pub type Transcripts = BTreeMap<ItemId, Arc<Transcript>>;

fn ticks(t: f64) -> Tick {
    Tick(t.round() as i64)
}

/// The sequence transcript: words of every transcribed clip on the audio tracks, in sequence
/// time order (see the module docs for the rules).
pub fn sequence_words(seq: &Sequence, transcripts: &Transcripts) -> Vec<SeqWord> {
    let mut out: Vec<SeqWord> = Vec::new();
    // timeline ranges already served by a higher track's transcribed clips
    let mut claimed: Vec<TimeRange> = Vec::new();
    for (ti, track) in seq.audio_tracks.iter().enumerate() {
        let mut mine = Vec::new();
        for it in &track.items {
            if !it.enabled || it.reverse || it.frame_hold.is_some() || it.speed <= 0.0 {
                continue;
            }
            let Some(tr) = transcripts.get(&it.item) else { continue };
            mine.push(it.range());
            let speed = it.speed;
            let media_end = it.source_in + ticks(it.duration.0 as f64 * speed);
            let to_tl = |m: Tick| it.start + ticks((m - it.source_in).0 as f64 / speed);
            for wi in tr.words_in(TimeRange::from_bounds(it.source_in, media_end.max(it.source_in))) {
                let w = &tr.words[wi];
                let (a, b) = (to_tl(w.start), to_tl(w.end.max(w.start)));
                let mid = Tick(a.0 + (b.0 - a.0) / 2);
                if mid < it.start || mid >= it.end() || claimed.iter().any(|r| r.contains(mid)) {
                    continue;
                }
                out.push(SeqWord {
                    text: w.text.clone(),
                    start: a.max(it.start),
                    end: b.min(it.end()).max(a.max(it.start)),
                    clip: it.id,
                    item: it.item,
                    index: wi,
                    track: ti,
                    speaker: tr.speaker_name(w),
                    confidence: w.confidence,
                });
            }
        }
        claimed.extend(mine);
    }
    out.sort_by_key(|w| (w.start, w.track));
    out
}

/// Index of the word being spoken at `t` (the last word starting at or before `t` whose end is
/// after `t`).
pub fn word_at(words: &[SeqWord], t: Tick) -> Option<usize> {
    let i = words.partition_point(|w| w.start <= t).checked_sub(1)?;
    (t < words[i].end).then_some(i)
}

/// Paragraphs (Text panel segments): runs of words split where the speaker changes or at a pause
/// of at least `gap`.
pub fn paragraphs(words: &[SeqWord], gap: Tick) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut a = 0;
    for i in 1..=words.len() {
        if i == words.len() || words[i].speaker != words[i - 1].speaker || words[i].start - words[i - 1].end >= gap {
            if a < i {
                out.push(a..i);
            }
            a = i;
        }
    }
    out
}

/// Matches of `query` (one or more words, case and punctuation ignored; the last query word may be
/// a prefix) as word index ranges.
pub fn search(words: &[SeqWord], query: &str) -> Vec<std::ops::Range<usize>> {
    search_with(words, query, SearchOptions::default())
}

/// The Text panel's search settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// "Find whole words only": the last query word must be a whole word too.
    pub whole_words: bool,
    /// "Match capitalization".
    pub match_case: bool,
}

/// [`search`] with search settings (punctuation is always ignored).
pub fn search_with(words: &[SeqWord], query: &str, opts: SearchOptions) -> Vec<std::ops::Range<usize>> {
    let norm = |s: &str| {
        let t = s.trim_matches(|c: char| !c.is_alphanumeric());
        if opts.match_case { t.to_string() } else { t.to_lowercase() }
    };
    let q: Vec<String> = query.split_whitespace().map(norm).filter(|s| !s.is_empty()).collect();
    let Some(last) = q.len().checked_sub(1) else { return Vec::new() };
    let text: Vec<String> = words.iter().map(|w| norm(&w.text)).collect();
    let mut out = Vec::new();
    for i in 0..text.len().saturating_sub(last) {
        let ok = q.iter().enumerate().all(|(k, qw)| {
            let Some(w) = text.get(i + k) else { return false };
            if k == last && !opts.whole_words { w.starts_with(qw.as_str()) } else { w == qw }
        });
        if ok {
            out.push(i..i + q.len());
        }
    }
    out
}

/// Timeline range of words `a..=b`, snapped outward to frames.
pub fn word_range(words: &[SeqWord], a: usize, b: usize, rate: FrameRate) -> Option<TimeRange> {
    let (a, b) = (a.min(b), a.max(b));
    let (s, e) = (words.get(a)?.start, words.get(b)?.end);
    let s = rate.snap(s);
    let mut e2 = rate.snap(e);
    if e2 < e || e2 <= s {
        e2 += rate.frame_duration();
    }
    Some(TimeRange::from_bounds(s, e2))
}

/// Media range of words `a..=b` of a clip transcript, snapped outward to the media's frames (for
/// Source-monitor In/Out from a text selection).
pub fn media_word_range(t: &Transcript, a: usize, b: usize, rate: FrameRate) -> Option<TimeRange> {
    let (a, b) = (a.min(b), a.max(b));
    let (s, e) = (t.words.get(a)?.start, t.words.get(b)?.end);
    let s = rate.snap(s);
    let mut e2 = rate.snap(e);
    if e2 < e || e2 <= s {
        e2 += rate.frame_duration();
    }
    Some(TimeRange::from_bounds(s, e2))
}

/// What the audio of the transcribed clips says about the sequence: where it is silent (no
/// speech, and no other sound either: a laugh, a cough), and where that was measured (the
/// timeline spans of clips whose transcript holds [`Transcript::silences`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hearing {
    /// Silent ranges, sequence time, sorted and merged.
    pub silent: Vec<TimeRange>,
    /// Ranges where the audio was measured, sorted and merged.
    pub known: Vec<TimeRange>,
}

/// `r` less the parts `cut` covers (both sorted).
fn subtract(r: TimeRange, cut: &[TimeRange]) -> Vec<TimeRange> {
    let mut out = Vec::new();
    let mut a = r.start;
    for c in cut {
        if c.end() <= a || c.start >= r.end() {
            continue;
        }
        if c.start > a {
            out.push(TimeRange::from_bounds(a, c.start));
        }
        a = a.max(c.end());
    }
    if a < r.end() {
        out.push(TimeRange::from_bounds(a, r.end()));
    }
    out
}

/// The parts of `r` inside `ranges` (sorted).
fn intersect(r: TimeRange, ranges: &[TimeRange]) -> Vec<TimeRange> {
    ranges
        .iter()
        .filter_map(|x| {
            let (a, b) = (x.start.max(r.start), x.end().min(r.end()));
            (b > a).then(|| TimeRange::from_bounds(a, b))
        })
        .collect()
}

/// The audio of the sequence as far as transcripts measured it: the silences of every transcribed
/// clip on the audio tracks mapped to sequence time, read top first like the words (a clip on a
/// higher track is what is heard where it plays).
pub fn sequence_hearing(seq: &Sequence, transcripts: &Transcripts) -> Hearing {
    let mut h = Hearing::default();
    let mut claimed: Vec<TimeRange> = Vec::new();
    for track in &seq.audio_tracks {
        let mut mine = Vec::new();
        for it in &track.items {
            if !it.enabled || it.reverse || it.frame_hold.is_some() || it.speed <= 0.0 {
                continue;
            }
            let Some(tr) = transcripts.get(&it.item) else { continue };
            mine.push(it.range());
            let Some(silences) = &tr.silences else { continue };
            claimed.sort_by_key(|r| r.start);
            let free = subtract(it.range(), &merge_ranges(claimed.clone()));
            let speed = it.speed;
            let to_tl = |m: Tick| it.start + ticks((m - it.source_in).0 as f64 / speed);
            h.known.extend(free.iter().copied());
            for sil in silences {
                let (a, b) = (to_tl(sil.start).max(it.start), to_tl(sil.end()).min(it.end()));
                if b > a {
                    h.silent.extend(intersect(TimeRange::from_bounds(a, b), &free));
                }
            }
        }
        claimed.extend(mine);
    }
    h.silent = merge_ranges(h.silent);
    h.known = merge_ranges(h.known);
    h
}

/// How far a pause reaches into the words next to it where the audio there is silent: a word
/// whose transcribed end is later than its sound (or start earlier) gives that time to the pause.
pub const PAUSE_REACH_SECONDS: f64 = 0.5;

/// A pause: the silence between word `after` of the sequence transcript and the next word (the
/// Text panel shows it inline as "[...]"). Where the audio was measured ([`Hearing`]) the pause
/// is its silence: it starts where the sound of the word before it stops, ends where the next
/// sound starts, and other sound between the words (a laugh) is not part of it ([`pause_spans`]).
/// Otherwise it is the time between the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pause {
    /// Index of the word before the pause.
    pub after: usize,
    /// Sequence time: the first and the last silent moment (without measured audio: the end of
    /// word `after` and the start of the next word).
    pub start: Tick,
    pub end: Tick,
}

/// The silent stretches of at least `min` between words `after` and `after + 1`: the measured
/// silence touching the time between them (reaching up to [`PAUSE_REACH_SECONDS`] into the words
/// where they are silent), and that time itself where nothing was measured.
fn silent_spans(words: &[SeqWord], h: &Hearing, after: usize, min: Tick) -> Vec<TimeRange> {
    let (Some(wa), Some(wb)) = (words.get(after), after.checked_add(1).and_then(|i| words.get(i))) else { return Vec::new() };
    let (a, b) = (wa.end, wb.start);
    if b <= a {
        return Vec::new();
    }
    let gap = TimeRange::from_bounds(a, b);
    let reach = ticks(PAUSE_REACH_SECONDS * TICKS_PER_SECOND as f64);
    let window = TimeRange::from_bounds((a - reach).max(wa.start).min(a), (b + reach).min(wb.end).max(b));
    let mut spans: Vec<TimeRange> = intersect(window, &h.silent).into_iter().filter(|r| r.start < b && r.end() > a).collect();
    spans.extend(subtract(gap, &h.known));
    merge_ranges(spans).into_iter().filter(|r| r.duration >= min.max(Tick(1))).collect()
}

/// The silent stretches of pause `p` of at least `min` (see [`Pause`]): what deleting it removes,
/// less the silence kept next to the sound ([`pause_ranges`]).
pub fn pause_spans(words: &[SeqWord], h: &Hearing, p: &Pause, min: Tick) -> Vec<TimeRange> {
    silent_spans(words, h, p.after, min).into_iter().filter(|r| r.start >= p.start && r.end() <= p.end).collect()
}

/// The pause after word `after` with silence of at least `min` in it, as heard.
pub fn pause_after_heard(words: &[SeqWord], h: &Hearing, after: usize, min: Tick) -> Option<Pause> {
    let spans = silent_spans(words, h, after, min);
    let (first, last) = (spans.first()?, spans.last()?);
    Some(Pause { after, start: first.start, end: last.end() })
}

/// The pauses with a silence of at least `min` (and at least one tick), as heard, in order.
pub fn pauses_heard(words: &[SeqWord], h: &Hearing, min: Tick) -> Vec<Pause> {
    (0..words.len().saturating_sub(1)).filter_map(|i| pause_after_heard(words, h, i, min)).collect()
}

/// The timeline ranges removing `spans` (a pause's silence) except `keep` next to the sound on
/// both sides of each, snapped inward to frames; spans with less than a frame left go.
pub fn pause_ranges(spans: &[TimeRange], keep: Tick, rate: FrameRate) -> Vec<TimeRange> {
    spans.iter().filter_map(|r| pause_range(&Pause { after: 0, start: r.start, end: r.end() }, keep, rate)).collect()
}

/// The pauses with a silence of at least `min`, as heard, as the ranges to remove: each silent
/// stretch of at least `min` less `keep` next to the sound on both sides ([`pause_ranges`]).
pub fn find_pauses_heard(words: &[SeqWord], h: &Hearing, min: Tick, keep: Tick, rate: FrameRate) -> Vec<TimeRange> {
    pauses_heard(words, h, min).iter().flat_map(|p| pause_ranges(&pause_spans(words, h, p, min), keep, rate)).collect()
}

impl Pause {
    pub fn duration(&self) -> Tick {
        self.end - self.start
    }
}

/// The pause after word `after` (whatever its length), if the next word starts later.
pub fn pause_after(words: &[SeqWord], after: usize) -> Option<Pause> {
    let (a, b) = (words.get(after)?.end, words.get(after.checked_add(1)?)?.start);
    (b > a).then_some(Pause { after, start: a, end: b })
}

/// The pauses of at least `min` (and at least one tick) between consecutive words, in order.
pub fn pauses(words: &[SeqWord], min: Tick) -> Vec<Pause> {
    (0..words.len().saturating_sub(1)).filter_map(|i| pause_after(words, i)).filter(|p| p.duration() >= min).collect()
}

/// The timeline range removing pause `p` except `keep` of silence next to both words, snapped
/// inward to frames; None when less than a frame is left.
pub fn pause_range(p: &Pause, keep: Tick, rate: FrameRate) -> Option<TimeRange> {
    let (s, e) = (p.start + keep, p.end - keep);
    let mut s2 = rate.snap(s);
    if s2 < s {
        s2 += rate.frame_duration();
    }
    let e2 = rate.snap(e);
    (e2 > s2).then(|| TimeRange::from_bounds(s2, e2))
}

/// Pauses between consecutive words of at least `min`, as the ranges to remove: each keeps `keep`
/// of silence next to both words and is snapped inward to frames (pauses shorter than one frame
/// after that are skipped).
pub fn find_pauses(words: &[SeqWord], min: Tick, keep: Tick, rate: FrameRate) -> Vec<TimeRange> {
    pauses(words, min).iter().filter_map(|p| pause_range(p, keep, rate)).collect()
}

/// The filler words of English transcripts (and of transcripts without a language).
pub const DEFAULT_FILLERS: &[&str] = &["um", "uh", "umm", "uhm", "erm", "er", "ah", "hmm", "mm", "mhm"];

/// The filler words of German transcripts. "er", "ah" and "eh" are words in German, so they are
/// not on the list.
pub const GERMAN_FILLERS: &[&str] = &["äh", "ähm", "ähh", "ähmm", "öh", "öhm", "ehm", "hm", "hmm", "mm", "mhm", "um", "uh", "uhm"];

/// The filler words of other languages: only hesitation sounds that are words in none of them
/// ("er" is Dutch for "there").
pub const NEUTRAL_FILLERS: &[&str] = &["um", "uh", "umm", "uhm", "erm", "ehm", "hmm", "mm", "mhm", "äh", "ähm"];

/// The filler words for a transcript language (ISO 639-1, optionally with a region: `de-AT`).
pub fn default_fillers(language: &str) -> &'static [&'static str] {
    let primary = language.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    match primary.as_str() {
        "" | "en" => DEFAULT_FILLERS,
        "de" => GERMAN_FILLERS,
        _ => NEUTRAL_FILLERS,
    }
}

/// Filler words and phrases normalized for matching (case and punctuation dropped, each split into
/// words), longest first so "you know" wins over a lone "you".
pub fn filler_phrases<S: AsRef<str>>(fillers: &[S]) -> Vec<Vec<String>> {
    let mut phrases: Vec<Vec<String>> = fillers
        .iter()
        .map(|f| f.as_ref().split_whitespace().map(filmcraft_project::transcript::normalize_word).filter(|s| !s.is_empty()).collect::<Vec<_>>())
        .filter(|p| !p.is_empty())
        .collect();
    phrases.sort_by_key(|p| std::cmp::Reverse(p.len()));
    phrases
}

/// Filler words: word index ranges matching one of `fillers` (each a word or a phrase such as
/// "you know"; case and punctuation ignored).
pub fn find_fillers(words: &[SeqWord], fillers: &[String]) -> Vec<std::ops::Range<usize>> {
    let phrases = filler_phrases(fillers);
    find_fillers_by(words, |_| phrases.as_slice())
}

/// [`find_fillers`] with the phrases (from [`filler_phrases`]) chosen per word, e.g. by the
/// language of the word's transcript; a phrase is looked up by its first word.
pub fn find_fillers_by<'a>(words: &[SeqWord], phrases_for: impl Fn(&SeqWord) -> &'a [Vec<String>]) -> Vec<std::ops::Range<usize>> {
    let norm: Vec<String> = words.iter().map(SeqWord::normalized).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let hit = phrases_for(w).iter().find(|p| norm.get(i..i.saturating_add(p.len())).is_some_and(|run| run == p.as_slice()));
        match hit {
            Some(p) => {
                out.push(i..i + p.len());
                i += p.len();
            }
            None => i += 1,
        }
    }
    out
}

/// Timeline ranges removing the filler words `hits`: each word range snapped to the nearest frames
/// (never into the neighbouring words' frames).
pub fn filler_ranges(words: &[SeqWord], hits: &[std::ops::Range<usize>], rate: FrameRate) -> Vec<TimeRange> {
    let mut out = Vec::new();
    for h in hits {
        let (a, b) = (h.start, h.end - 1);
        let mut s = rate.snap_nearest(words[a].start);
        let mut e = rate.snap_nearest(words[b].end);
        if a > 0 && s < words[a - 1].end {
            s = rate.snap(words[a - 1].end) + rate.frame_duration();
        }
        if let Some(n) = words.get(b + 1)
            && e > n.start
        {
            e = rate.snap(n.start);
        }
        if e > s {
            out.push(TimeRange::from_bounds(s, e));
        }
    }
    out
}

/// Sort and merge ranges (overlapping or touching).
pub fn merge_ranges(mut r: Vec<TimeRange>) -> Vec<TimeRange> {
    r.sort_by_key(|x| x.start);
    let mut out: Vec<TimeRange> = Vec::new();
    for x in r {
        match out.last_mut() {
            Some(l) if x.start <= l.end() => *l = TimeRange::from_bounds(l.start, l.end().max(x.end())),
            _ => out.push(x),
        }
    }
    out
}

/// Ripple-delete every range on every unlocked track (and sync-locked caption tracks), right to
/// left so earlier ranges keep their positions. Returns the total time removed.
pub fn ripple_delete_ranges(seq: &mut Sequence, ranges: Vec<TimeRange>, ctx: &mut EditCtx) -> Tick {
    let tracks: Vec<TrackId> = seq.all_tracks().filter(|t| !t.locked).map(|t| t.id).collect();
    let mut total = Tick::ZERO;
    for r in merge_ranges(ranges).into_iter().rev() {
        if r.duration <= Tick::ZERO {
            continue;
        }
        crate::extract(seq, &tracks, r, ctx);
        total += r.duration;
    }
    total
}

/// Rules for Create Captions from a transcript (Premiere's dialog defaults).
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionRules {
    /// Maximum characters per line.
    pub max_chars: usize,
    /// Lines per caption (1 = single, 2 = double).
    pub lines: usize,
    /// Minimum caption duration; a short caption is extended into the silence after it, never
    /// over the next one.
    pub min_duration: Tick,
    /// Longest caption.
    pub max_duration: Tick,
    /// Frames left empty between consecutive captions.
    pub gap_frames: i64,
    /// A pause at least this long starts a new caption.
    pub break_pause: Tick,
}

impl Default for CaptionRules {
    fn default() -> Self {
        Self {
            max_chars: 42,
            lines: 2,
            min_duration: Tick(TICKS_PER_SECOND),
            max_duration: Tick(7 * TICKS_PER_SECOND),
            gap_frames: 0,
            break_pause: Tick(TICKS_PER_SECOND),
        }
    }
}

/// A caption block made from words.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionBlock {
    pub start: Tick,
    pub end: Tick,
    /// Lines joined by `\n`.
    pub text: String,
    pub speaker: Option<String>,
    /// The word index range it shows.
    pub words: std::ops::Range<usize>,
}

/// Lay words out as caption blocks: words fill lines of at most `max_chars` (a longer single word
/// gets a line of its own) and blocks of `lines` lines; a new block starts at a speaker change,
/// at a pause of `break_pause`, after `max_duration`, or after sentence-ending punctuation once the
/// block is past half full. Times are snapped to frames, blocks never overlap and keep
/// `gap_frames` between them.
pub fn caption_blocks(words: &[SeqWord], rules: &CaptionRules, rate: FrameRate) -> Vec<CaptionBlock> {
    let max_chars = rules.max_chars.max(1);
    let max_lines = rules.lines.max(1);
    let cap_chars = max_chars * max_lines;
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    let mut a = 0;
    let mut lines: Vec<usize> = vec![0];
    for i in 0..words.len() {
        let w = &words[i];
        let len = w.text.chars().count();
        if i > a {
            let prev = &words[i - 1];
            let cur = *lines.last().unwrap_or(&0);
            let fits_line = cur + 1 + len <= max_chars;
            let fits = fits_line || lines.len() < max_lines;
            let used: usize = lines.iter().sum::<usize>() + lines.len() - 1;
            let sentence_end = prev.text.ends_with(['.', '?', '!']) && used * 2 >= cap_chars;
            let brk =
                !fits || w.speaker != prev.speaker || w.start - prev.end >= rules.break_pause || w.end - words[a].start > rules.max_duration || sentence_end;
            if brk {
                groups.push(a..i);
                a = i;
                lines = vec![len];
                continue;
            }
            if fits_line {
                if let Some(l) = lines.last_mut() {
                    *l += 1 + len;
                }
            } else {
                lines.push(len);
            }
        } else {
            lines = vec![len];
        }
    }
    if a < words.len() {
        groups.push(a..words.len());
    }
    // text, frame-snapped times
    let fd = rate.frame_duration();
    let gap = Tick(fd.0 * rules.gap_frames.max(0));
    let mut out: Vec<CaptionBlock> = Vec::new();
    for g in groups {
        let mut text_lines: Vec<String> = vec![String::new()];
        for w in &words[g.clone()] {
            let Some(l) = text_lines.last_mut() else { break };
            if l.is_empty() {
                l.push_str(&w.text);
            } else if l.chars().count() + 1 + w.text.chars().count() <= max_chars {
                l.push(' ');
                l.push_str(&w.text);
            } else {
                text_lines.push(w.text.clone());
            }
        }
        let start = rate.snap(words[g.start].start);
        let mut end = rate.snap(words[g.end - 1].end);
        if end < words[g.end - 1].end {
            end += fd;
        }
        out.push(CaptionBlock { start, end: end.max(start + fd), text: text_lines.join("\n"), speaker: words[g.start].speaker.clone(), words: g });
    }
    // no overlaps, minimum duration (into the silence after a block, never over the next one)
    for i in 0..out.len() {
        if i > 0 {
            let min_start = out[i - 1].end + gap;
            if out[i].start < min_start {
                out[i].start = min_start;
                out[i].end = out[i].end.max(min_start + fd);
            }
        }
        let next = out.get(i + 1).map(|n| n.start);
        let b = &mut out[i];
        if b.end - b.start < rules.min_duration {
            let want = b.start + rules.min_duration;
            let mut e = rate.snap(want);
            if e < want {
                e += fd;
            }
            b.end = e;
        }
        if let Some(n) = next {
            b.end = b.end.min(n - gap).max(b.start + fd);
        }
    }
    out
}

/// Caption blocks as captions (ids from `ctx`).
pub fn blocks_to_captions(blocks: &[CaptionBlock], ctx: &mut EditCtx) -> Vec<Caption> {
    blocks
        .iter()
        .map(|b| Caption {
            id: ClipId(ctx.alloc()),
            start: b.start,
            duration: b.end - b.start,
            text: b.text.clone(),
            speaker: b.speaker.clone(),
            cue_id: None,
            settings: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
