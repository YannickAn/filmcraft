//! Word bounds stage by stage, for tuning pause detection on real recordings: the recogniser's
//! own times (`raw`), after tightening (`tight`) and after FilmCraft's look at the audio
//! (`fitted`, with the laughs), as JSON with times in seconds.
//!
//! ```sh
//! ffmpeg -i clip.mp4 -map 0:a:0 -ar 16000 -ac 1 -f f32le clip.f32
//! cargo run --release -p filmcraft-speech --features parakeet --example pause_lab -- <model dir> clip.f32 out.json [--raw=raw.json]
//! ```
//!
//! `--raw=<file>` reads the recogniser's words from an earlier run's `out.json` instead of running
//! the model (the model directory is then not read), so changes after recognition are quick to try.

use filmcraft_project::{Transcript, Word};
use filmcraft_speech::parakeet::Parakeet;
use filmcraft_speech::seconds_tick;
use serde_json::{Value, json};

fn secs(t: filmcraft_time::Tick) -> f64 {
    t.0 as f64 / 254_016_000_000.0
}

fn dump(words: &[Word]) -> Value {
    words.iter().map(|w| json!({"text": w.text, "start": secs(w.start), "end": secs(w.end), "confidence": w.confidence})).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(model), Some(audio_path), Some(out)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("usage: pause_lab <model dir> <audio.f32> <out.json> [--raw=<earlier out.json>]");
        return;
    };
    let raw_from = args.iter().find_map(|a| a.strip_prefix("--raw="));
    let audio: Vec<f32> = std::fs::read(audio_path).expect("read audio").as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let raw: Vec<Word> = match raw_from {
        Some(f) => {
            let v: Value = serde_json::from_slice(&std::fs::read(f).expect("read raw")).expect("raw json");
            v["raw"]
                .as_array()
                .expect("raw words")
                .iter()
                .map(|w| {
                    let mut word = Word::new(
                        w["text"].as_str().unwrap_or(""),
                        seconds_tick(w["start"].as_f64().unwrap_or(0.0)),
                        seconds_tick(w["end"].as_f64().unwrap_or(0.0)),
                    );
                    word.confidence = w["confidence"].as_f64().unwrap_or(1.0) as f32;
                    word
                })
                .collect()
        }
        None => {
            let path = std::path::PathBuf::from(model);
            let t0 = std::time::Instant::now();
            let p = Parakeet::load(&path, "parakeet").expect("load model");
            eprintln!("loaded in {:.1}s", t0.elapsed().as_secs_f64());
            let t1 = std::time::Instant::now();
            let words = p.recognise(&audio, &mut |_| true).expect("recognise");
            eprintln!("recognised {} words in {:.1}s", words.len(), t1.elapsed().as_secs_f64());
            words
        }
    };
    let mut t = Transcript { language: "de".into(), source: "lab".into(), speakers: Vec::new(), words: raw.clone(), laughs: None, audio_analysis: 0 };
    t.normalize();
    filmcraft_speech::vad::tighten_words(&audio, &mut t.words);
    let tight = t.words.clone();
    filmcraft_speech::analyze_audio(&audio, &mut t);
    let laughs: Vec<Value> = t.laughs.iter().flatten().map(|l| json!({"start": secs(l.start), "duration": secs(l.duration)})).collect();
    let v = json!({"raw": dump(&raw), "tight": dump(&tight), "fitted": dump(&t.words), "laughs": laughs});
    std::fs::write(out, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
    eprintln!("wrote {out}");
}
