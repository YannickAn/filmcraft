//! One track's height, as in Premiere: Alt + drag on a track header makes that track (and no
//! other) taller or shorter; a double-click expands or collapses it. Both run
//! `timeline.setTrackHeight`, the height lives in `ui.timeline.track_heights` (readable with
//! `ui.inspect`, settable with `ui.set`) and is kept with the sequence's view in the project file.
//! Driven over the control channel as an agent would: `ui.drag` with `modifiers: "Alt"`.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn new() -> Self {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        // frames 1/60 s apart, so the two clicks of `ui.click {"count": 2}` are a double-click
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).with_max_steps(10_000).with_step_dt(1.0 / 60.0).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let v = self.call(method, params.clone());
        assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
        v["result"].clone()
    }

    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }

    fn app(&self) -> &FilmcraftApp {
        self.harness.state()
    }

    /// The height every track is drawn with, by header name (`V1`, `A2`…).
    fn heights(&self) -> Vec<(String, f32)> {
        let layout = self.app().tl.layout.as_ref().expect("the Timeline was drawn");
        layout
            .rows
            .iter()
            .map(|r| (format!("{}{}", if r.kind == filmcraft_project::TrackKind::Video { "V" } else { "A" }, r.index + 1), r.rect.height()))
            .collect()
    }

    fn height(&self, track: &str) -> f32 {
        self.heights().into_iter().find(|(n, _)| n == track).map(|(_, h)| h).unwrap_or_else(|| panic!("no track {track}"))
    }

    /// The centre of a registered element.
    fn centre(&mut self, id: &str) -> (f32, f32) {
        let els = self.ok("ui.elements", json!({"prefix": id}));
        let e = els.as_array().unwrap().iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("no element {id}: {els}")).clone();
        let r: Vec<f32> = e["rect"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect();
        (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
    }

    /// Drag from the centre of element `id` by `dy` points, with `modifiers`; the commands it ran.
    fn drag(&mut self, id: &str, dy: f32, modifiers: Value) -> Vec<String> {
        let (x, y) = self.centre(id);
        let r = self.ok("ui.drag", json!({"from": {"x": x, "y": y}, "to": {"x": x, "y": y + dy}, "modifiers": modifiers}));
        self.frames(2);
        r["ran"].as_array().map(|a| a.iter().filter_map(|c| c["id"].as_str().map(str::to_string)).collect()).unwrap_or_default()
    }

    fn track_id(&mut self, kind: &str, index: usize) -> u64 {
        self.exec("sequence.inspect", json!({}))[kind][index]["id"].as_u64().unwrap()
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.6
}

#[test]
fn alt_drag_on_a_track_header_resizes_that_track_only() {
    let mut d = Driver::new();
    d.ok("ui.set", json!({"timeline": {"videoTrackHeight": 40.0, "audioTrackHeight": 40.0}}));
    d.frames(2);
    let before = d.heights();
    assert!(before.iter().filter(|(n, _)| n.starts_with('A')).count() >= 2, "the demo has two audio tracks: {before:?}");
    let a1 = d.track_id("audio", 0);

    // Alt + drag down on A1's header: A1 alone gets 36 points taller
    let ran = d.drag("timeline.track.A1.header", 36.0, json!("Alt"));
    assert!(ran.iter().any(|c| c == "timeline.setTrackHeight"), "the gesture runs the command: {ran:?}");
    assert!(close(d.height("A1"), 76.0), "A1: {}", d.height("A1"));
    for (name, h) in &before {
        if name != "A1" {
            assert!(close(d.height(name), *h), "{name} kept its height");
        }
    }
    assert_eq!(d.app().ui.timeline.audio_track_h, 40.0, "the audio tracks' height is unchanged");
    // the height is serde UI state, read back with ui.inspect
    let ui = d.ok("ui.inspect", json!({}));
    assert!(close(ui["ui"]["timeline"]["track_heights"][a1.to_string()].as_f64().unwrap() as f32, 76.0), "{}", ui["ui"]["timeline"]);

    // video tracks stack up from V1: dragging up makes V1 taller, V2 keeps its height
    let v2 = d.height("V2");
    d.drag("timeline.track.V1.header", -24.0, json!({"alt": true}));
    assert!(close(d.height("V1"), 64.0), "V1: {}", d.height("V1"));
    assert!(close(d.height("V2"), v2));

    // dragging up shrinks an audio track, down to the smallest height
    d.drag("timeline.track.A1.header", -300.0, json!("Alt"));
    assert!(close(d.height("A1"), 22.0), "A1: {}", d.height("A1"));

    // without Alt a drag on the header resizes nothing
    let now = d.heights();
    let ran = d.drag("timeline.track.A2.header", 30.0, json!(null));
    assert!(ran.is_empty(), "{ran:?}");
    assert_eq!(d.heights(), now);

    // Alt + drag that starts on a header button resizes the track and leaves the button alone
    let locked = |d: &Driver| d.app().session.active_sequence().unwrap().audio_tracks[1].locked;
    let was = locked(&d);
    d.drag("timeline.track.A2.locked", 20.0, json!("Alt"));
    assert!(close(d.height("A2"), 60.0), "A2: {}", d.height("A2"));
    assert_eq!(locked(&d), was, "the lock button was not clicked");

    // a double-click on the header expands or collapses that track only
    let (x, y) = d.centre("timeline.track.A2.header");
    d.ok("ui.click", json!({"x": x, "y": y, "count": 2}));
    d.frames(2);
    assert!(close(d.height("A2"), 30.0), "A2 collapsed: {}", d.height("A2"));
    assert!(close(d.height("A1"), 22.0), "A1 untouched");
}

#[test]
fn track_heights_by_command_ui_set_and_all_tracks_commands() {
    let mut d = Driver::new();
    d.ok("ui.set", json!({"timeline": {"videoTrackHeight": 40.0, "audioTrackHeight": 40.0}}));
    let v2 = d.track_id("video", 1);

    // by command: id or header name; height, delta, reset
    let r = d.exec("timeline.setTrackHeight", json!({"track": "A2", "height": 90}));
    assert_eq!((r["height"].as_f64(), r["own"].as_bool()), (Some(90.0), Some(true)));
    d.exec("timeline.setTrackHeight", json!({"track": v2, "delta": 15}));
    d.frames(2);
    assert!(close(d.height("A2"), 90.0) && close(d.height("V2"), 55.0) && close(d.height("A1"), 40.0), "{:?}", d.heights());
    // hostile numbers are kept in range, a track that is not there is an error
    assert_eq!(d.exec("timeline.setTrackHeight", json!({"track": "A2", "height": 1e30}))["height"], json!(400.0));
    assert_eq!(d.exec("timeline.setTrackHeight", json!({"track": "A2", "height": -5}))["height"], json!(22.0));
    for bad in [
        json!({"track": "Z9", "height": 50}),
        json!({"track": "A99", "height": 50}),
        json!({"track": "A1"}),
        json!({"height": 50}),
        json!({"track": "A0", "height": 50}),
    ] {
        let v = d.call("engine.execute", json!({"command": "timeline.setTrackHeight", "params": bad}));
        assert_eq!(v["ok"], json!(false), "{bad} should fail: {v}");
    }
    d.exec("timeline.setTrackHeight", json!({"track": "A2", "reset": true}));
    d.frames(2);
    assert!(close(d.height("A2"), 40.0), "A2 has the audio tracks' height again");

    // ui.set: by header name, null takes the height of the kind again
    d.ok("ui.set", json!({"timeline": {"trackHeights": {"A1": 70, "V2": null}}}));
    d.frames(2);
    assert!(close(d.height("A1"), 70.0) && close(d.height("V2"), 40.0), "{:?}", d.heights());
    assert_eq!(d.call("ui.set", json!({"timeline": {"trackHeights": {"A1": "tall"}}}))["ok"], json!(false));

    // Increase Audio Tracks Height: every audio track, A1 keeps its extra height
    d.ok("engine.execute", json!({"command": "timeline.increaseAudioHeight"}));
    d.frames(2);
    assert!(close(d.height("A1"), 82.0) && close(d.height("A2"), 52.0), "{:?}", d.heights());

    // Expand All Tracks: every track the same height; again: as before, A1 too
    d.exec("timeline.expandAllTracks", json!({}));
    d.frames(2);
    assert!(d.heights().iter().all(|(_, h)| close(*h, 120.0)), "{:?}", d.heights());
    d.exec("timeline.expandAllTracks", json!({}));
    d.frames(2);
    assert!(close(d.height("A1"), 82.0) && close(d.height("A2"), 52.0), "{:?}", d.heights());

    // the view goes into the project file and comes back with it
    let dir = std::env::temp_dir().join(format!("fc-track-heights-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("heights.fcproj").to_string_lossy().to_string();
    d.exec("file.saveAs", json!({"path": path}));
    d.ok("ui.set", json!({"timeline": {"trackHeights": {"A1": null}}}));
    d.exec("file.open", json!({"path": path}));
    d.frames(4);
    assert!(close(d.height("A1"), 82.0), "A1 after reopening: {:?}", d.heights());
    let _ = std::fs::remove_dir_all(&dir);
}
