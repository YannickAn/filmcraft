//! A recording with several audio streams (OBS writes one per source: mix, microphone, game…)
//! goes on A1, A2… one clip per stream. Each clip's waveform is its own stream's: the Timeline
//! cached waveforms by media item only and drew stream 0 on every track.

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_media::MediaSource;
use filmcraft_project::{ItemId, ItemKind, Label, MediaClip, MediaRef};
use filmcraft_time::FrameRate;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::panels::waveform::Waveform;
use serde_json::json;

/// A sequence with a two-stream item on A1 (stream 0, bars and tone) and A2 (stream 1). The
/// generator only plays stream 0, so stream 1 is silent.
fn two_streams() -> (Session, ItemId) {
    let mut s = Session::default();
    let mut p = (*s.project).clone();
    let rate = FrameRate::FPS_24;
    let seq = p.new_sequence("Two streams", Default::default(), 1, 1, None);
    let source = filmcraft_media::generators::GeneratorSource::new(filmcraft_media::Generator::BarsAndTone, 32, 32, rate, rate.tick_of(48));
    let mut info = source.info().clone();
    info.audio_streams.push(info.audio_streams[0].clone());
    let item = p.add_item(
        "two streams",
        Label::Iris,
        ItemKind::Media(MediaClip {
            media: MediaRef::Generator(filmcraft_media::Generator::BarsAndTone),
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        }),
        None,
    );
    s.project = std::sync::Arc::new(p);
    s.execute("sequence.open", json!({"item": seq.0})).unwrap();
    s.execute("source.open", json!({"item": item.0})).unwrap();
    s.execute("source.overwrite", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let streams: Vec<usize> = q.audio_tracks.iter().filter_map(|t| t.items.first()).map(|c| c.audio_stream).collect();
    assert_eq!(streams, vec![0, 1], "one clip per stream on A1 and A2");
    (s, item)
}

fn loudest(w: &Waveform) -> f32 {
    w.peak()
}

#[test]
fn each_audio_clip_draws_the_waveform_of_its_own_stream() {
    let (s, item) = two_streams();
    let app = FilmcraftApp::new(s);
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
    let mut got = (None, None);
    for _ in 0..1000 {
        h.run_steps(1);
        let peaks = h.state().tl.peaks.lock().unwrap().clone();
        got = (peaks.get(&(item, 0)).cloned(), peaks.get(&(item, 1)).cloned());
        if got.0.is_some() && got.1.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let (Some(a1), Some(a2)) = got else { panic!("no waveform for both streams: {got:?}") };
    assert!(loudest(&a1) > 0.05, "A1 plays stream 0, the tone");
    assert!(loudest(&a2) < 1e-4, "A2 plays stream 1, which is silent, not a copy of stream 0");
}
