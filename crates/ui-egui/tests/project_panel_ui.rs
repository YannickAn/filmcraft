//! Headless UI tests of the M12.7 Project panel and Media Browser: List view (columns, sorting,
//! resizing, inline rename), Icon view (hover scrub sets In / Out, thumbnail size), Freeform view
//! (dragging places a card, stacks), the panel menu and Metadata Display… dialog, view presets,
//! bins opened in place / in a tab / in a window, and the Media Browser (navigation, Favorites,
//! Edit Columns…, import, drag to the Project panel, Open In Source Monitor).
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also render offscreen with wgpu and write
//! `project-*.png` / `media-browser-*.png`.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::project_panel::{FREEFORM_POS, FREEFORM_STACK, ViewMode};
use filmcraft_engine::{FsServices, Services, Session};
use filmcraft_project::{BinEntry, ItemId};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use filmcraft_ui_egui::dock::PanelKind;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<std::path::PathBuf>,
}

impl Driver {
    fn with(session: Session) -> Self {
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(std::path::PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000);
        if snapshots.is_some() {
            b = b.wgpu();
        }
        let harness = b.build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, snapshots };
        d.frames(4);
        d
    }

    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        Self::with(session)
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

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(2);
    }

    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }

    fn has(&mut self, id: &str) -> bool {
        self.ids(id).iter().any(|i| i == id)
    }

    fn rect(&mut self, id: &str) -> [f32; 4] {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        let e = v.as_array().unwrap().iter().find(|e| e["id"] == json!(id)).unwrap_or_else(|| panic!("no element {id}")).clone();
        let r = &e["rect"];
        [r[0].as_f64().unwrap() as f32, r[1].as_f64().unwrap() as f32, r[2].as_f64().unwrap() as f32, r[3].as_f64().unwrap() as f32]
    }

    fn label(&mut self, id: &str) -> String {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array().unwrap().iter().find(|e| e["id"] == json!(id)).and_then(|e| e["label"].as_str().map(str::to_string)).unwrap_or_default()
    }

    /// Step frames until `id` is registered.
    fn wait_for(&mut self, id: &str) {
        for _ in 0..400 {
            if self.has(id) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.frames(1);
        }
        panic!("{id} never appeared: {:?}", self.ids(id.split('.').next().unwrap()));
    }

    /// Step frames until the background import `job` is done.
    fn wait_import(&mut self, job: u64) {
        for _ in 0..3000 {
            if self.app().session.imports.result(job).is_some() {
                self.frames(1);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.frames(1);
        }
        panic!("import job {job} did not finish");
    }

    /// Step frames until `id` is gone (a background job is done).
    fn wait_gone(&mut self, id: &str) {
        for _ in 0..3000 {
            if !self.has(id) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.frames(1);
        }
        panic!("{id} is still there: {}", self.label(id));
    }

    fn key(&mut self, key: &str) {
        self.ok("ui.key", json!({"key": key}));
        self.frames(2);
    }

    fn app(&mut self) -> &mut FilmcraftApp {
        self.harness.state_mut()
    }

    /// The demo's Footage bin and its media items.
    fn footage(&mut self) -> (u64, Vec<u64>) {
        let p = &self.app().session.project;
        let bin = p
            .root
            .children
            .iter()
            .find_map(|e| match e {
                BinEntry::Bin(b) if b.name == "Footage" => Some(b.clone()),
                _ => None,
            })
            .expect("Footage bin");
        let items = bin.children.iter().filter_map(|e| if let BinEntry::Item(i) = e { Some(i.0) } else { None }).collect();
        (bin.id.0, items)
    }

    /// Every text the last frame painted, as laid out (with "…" when cut).
    fn painted_texts(&self) -> Vec<Painted> {
        fn walk(s: &egui::Shape, out: &mut Vec<Painted>) {
            match s {
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(t) => {
                    let text = t.galley.rows.iter().flat_map(|r| r.glyphs.iter().map(|g| g.chr)).collect();
                    let rect = t.galley.rect.translate(t.pos.to_vec2());
                    out.push(Painted { source: t.galley.job.text.clone(), text, rect, elided: t.galley.elided });
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        self.harness.output().shapes.iter().for_each(|c| walk(&c.shape, &mut out));
        out
    }

    fn snapshot(&mut self, name: &str, prefix: Option<&str>) {
        let Some(dir) = self.snapshots.clone() else { return };
        let crop = prefix.map(|p| self.rect(p));
        self.frames(2);
        let img = match self.harness.render() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("snapshot {name} skipped: {e}");
                return;
            }
        };
        let ppp = img.width() as f32 / 1600.0;
        let img = match crop {
            Some([x, y, w, h]) => {
                let (x0, y0) = ((x * ppp).max(0.0) as u32, (y * ppp).max(0.0) as u32);
                let (x1, y1) = ((((x + w) * ppp) as u32).min(img.width()), (((y + h) * ppp) as u32).min(img.height()));
                image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image()
            }
            None => img,
        };
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.png"));
        img.save(&path).unwrap();
        eprintln!("snapshot: {}", path.display());
    }
}

/// A painted text: what was asked for, what was drawn, where, and whether it was cut.
#[derive(Debug)]
struct Painted {
    source: String,
    text: String,
    rect: egui::Rect,
    elided: bool,
}

/// The demo project with the Project panel maximized (room for the List view's columns).
fn project_driver() -> Driver {
    let mut d = Driver::demo();
    d.ok("ui.panel.show", json!({"panel": "Project"}));
    d.ok("ui.set", json!({"focused": "Project"}));
    d.ok("ui.menu.invoke", json!({"id": "window.maximizeFrame"}));
    d.frames(3);
    d
}

/// Double-click a bin (synthetic input runs one event per frame, too slow for a double-click, so
/// this sends what the double-click does: open as Settings ▸ General ▸ Bins says).
fn double_click_bin(d: &mut Driver, bin: u64, alt: bool) {
    d.ok("ui.menu.invoke", json!({"id": "projectPanel.openBin", "params": {"bin": bin, "alt": alt}}));
    d.frames(3);
}

#[test]
fn list_view_columns_sort_resize_and_rename() {
    let mut d = project_driver();
    d.exec("project.view.set", json!({"view": "list"}));
    let (bin, items) = d.footage();
    d.frames(2);
    // expand the Footage bin
    d.click(&format!("project.bin.{bin}.toggle"));
    assert!(d.app().ui.expanded_bins.contains(&bin));
    d.wait_for(&format!("project.item.{}", items[0]));
    for c in ["Name", "Frame Rate", "Media Start", "Media Duration"] {
        assert!(d.has(&format!("project.list.header.{c}")), "{c} header");
    }
    assert!(d.has(&format!("project.item.{}.label", items[0])), "label chip");
    let y = |d: &mut Driver, i: u64| d.rect(&format!("project.item.{i}"))[1];
    // Name ascending: rows ordered by name
    let mut by_name: Vec<(String, u64)> = items.iter().map(|i| (d.app().session.project.item(ItemId(*i)).unwrap().name.to_lowercase(), *i)).collect();
    by_name.sort();
    let (first, last) = (by_name[0].1, by_name.last().unwrap().1);
    assert!(y(&mut d, first) < y(&mut d, last));
    d.snapshot("project-list", Some("panel.Project"));
    // clicking the Name header reverses the order
    d.click("project.list.header.Name");
    assert!(d.app().session.prefs.project_panel.view.sort.descending);
    d.frames(2);
    assert!(y(&mut d, first) > y(&mut d, last));
    // drag the Frame Rate column wider (saved in preferences)
    let w0 = d.app().session.prefs.project_panel.view.columns.iter().find(|c| c.name == "Frame Rate").unwrap().width;
    let r = d.rect("project.list.resize.Frame Rate");
    let (cx, cy) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
    d.ok("ui.drag", json!({"from": {"x": cx, "y": cy}, "to": {"x": cx + 60.0, "y": cy}}));
    d.frames(3);
    let w1 = d.app().session.prefs.project_panel.view.columns.iter().find(|c| c.name == "Frame Rate").unwrap().width;
    assert!(w1 > w0 + 30.0, "{w0} → {w1}");
    // inline rename: Rename, type, Enter (one undo step)
    d.exec("project.select", json!({"items": [items[0]]}));
    d.ok("ui.menu.invoke", json!({"id": "projectPanel.rename"}));
    d.frames(3);
    assert!(d.has("project.rename"));
    d.key("Cmd+A");
    d.ok("ui.type", json!({"text": "Hero shot"}));
    d.frames(2);
    d.key("Enter");
    d.frames(2);
    assert_eq!(d.app().session.project.item(ItemId(items[0])).unwrap().name, "Hero shot");
    assert!(d.app().ui.project_panel.rename.is_none());
    d.exec("edit.undo", json!({}));
    assert_ne!(d.app().session.project.item(ItemId(items[0])).unwrap().name, "Hero shot");
}

#[test]
fn icon_view_hover_scrub_sets_in_and_out() {
    let mut d = project_driver();
    d.exec("project.view.set", json!({"view": "icon"}));
    let (bin, items) = d.footage();
    // open the Footage bin in place (Settings ▸ Bins ▸ Double-click: Open in place)
    d.click(&format!("project.bin.{bin}"));
    double_click_bin(&mut d, bin, false);
    assert_eq!(d.app().ui.project_panel.bin, Some(bin));
    assert!(d.label("project.breadcrumb").ends_with("/ Footage"));
    let card = format!("project.item.{}", items[0]);
    d.wait_for(&card);
    d.ok("ui.set", json!({"focused": "Project"}));
    // hover a quarter in, press I; three quarters in, press O
    d.ok("ui.move", json!({"id": card, "fx": 0.25, "fy": 0.5}));
    d.frames(2);
    let h = d.app().ui.project_panel.hover.expect("hover scrub");
    assert_eq!(h.item, items[0]);
    d.key("I");
    d.ok("ui.move", json!({"id": card, "fx": 0.75, "fy": 0.5}));
    d.frames(2);
    d.key("O");
    let it = d.app().session.project.item(ItemId(items[0])).unwrap().clone();
    let m = it.as_media().unwrap();
    let (i, o) = (m.mark_in.expect("In set"), m.mark_out.expect("Out set"));
    let dur = it.duration().0 as f64;
    assert!((i.0 as f64 / dur - 0.25).abs() < 0.05, "In at {}", i.0 as f64 / dur);
    assert!((o.0 as f64 / dur - 0.75).abs() < 0.05, "Out at {}", o.0 as f64 / dur);
    d.frames(2);
    assert!(d.has(&format!("project.item.{}.inOut", items[0])), "In/Out bar on the card");
    // the sequence's marks are untouched
    assert!(d.app().session.active_sequence().unwrap().mark_in.is_none());
    d.snapshot("project-icon", Some("panel.Project"));
    // Shift+] / Shift+[ change the thumbnail size
    let s0 = d.app().session.prefs.project_panel.view.icon_size;
    d.key("Shift+]");
    assert!(d.app().session.prefs.project_panel.view.icon_size > s0);
    d.key("Shift+[");
    assert!((d.app().session.prefs.project_panel.view.icon_size - s0).abs() < 1.0);
    // Hover Scrub off: no hover, I marks the sequence again
    d.key("Shift+H");
    assert!(!d.app().session.prefs.project_panel.hover_scrub);
    d.ok("ui.move", json!({"id": card, "fx": 0.5, "fy": 0.5}));
    d.frames(4);
    d.key("I");
    assert!(d.app().session.active_sequence().unwrap().mark_in.is_some());
    // up one level
    d.click("project.up");
    assert_eq!(d.app().ui.project_panel.bin, None);
}

/// Long names in the Icon view are cut with "…" before a bin's item count and a clip's duration
/// (they used to run into the count: "Dreh 2026-10" under "5 items"), and the full name shows in
/// a tooltip over the name.
#[test]
fn icon_view_cuts_long_names_before_count_and_duration() {
    const BIN_NAME: &str = "Dreh 2026-10-11 Interviews Kamera A";
    const CLIP_NAME: &str = "A001_C001_0101_Interview_Kamera_A.mov";
    let mut d = project_driver();
    let (bin, items) = d.footage();
    d.exec("project.renameBin", json!({"bin": bin, "name": BIN_NAME}));
    d.exec("clip.rename", json!({"item": items[0], "name": CLIP_NAME}));
    d.exec("project.view.set", json!({"view": "icon"}));
    let card = format!("project.bin.{bin}");
    d.wait_for(&card);
    d.frames(2);
    let [x, y, w, h] = d.rect(&card);
    let texts = d.painted_texts();
    let name = texts.iter().find(|t| t.source == BIN_NAME).expect("bin name painted");
    let count = texts.iter().find(|t| t.source.ends_with(" items") && (t.rect.center().y - name.rect.center().y).abs() < 4.0).expect("item count");
    assert!(name.elided && name.text.ends_with('…'), "{name:?}");
    assert!(name.rect.max.x <= count.rect.min.x, "{name:?} runs into {count:?}");
    assert!(name.rect.min.x >= x && count.rect.max.x <= x + w + 0.5, "{name:?} {count:?} outside the card at {x}+{w}");
    // the full name in a tooltip over the name
    d.ok("ui.move", json!({"x": x + 10.0, "y": y + h + 11.0}));
    d.frames(6);
    assert!(d.painted_texts().iter().any(|t| t.source == BIN_NAME && !t.elided), "no tooltip with the full bin name");

    // a clip card: the name stops before the duration
    double_click_bin(&mut d, bin, false);
    let card = format!("project.item.{}", items[0]);
    d.wait_for(&card);
    d.frames(2);
    let [x, y, w, h] = d.rect(&card);
    let texts = d.painted_texts();
    let name = texts.iter().find(|t| t.source == CLIP_NAME).expect("clip name painted");
    let dur = texts
        .iter()
        .find(|t| t.source.contains(':') && t.source.chars().all(|c| c.is_ascii_digit() || c == ':') && (t.rect.center().y - name.rect.center().y).abs() < 4.0)
        .expect("duration");
    assert!(name.elided && name.text.ends_with('…'), "{name:?}");
    assert!(name.rect.max.x <= dur.rect.min.x, "{name:?} runs into {dur:?}");
    assert!(dur.rect.max.x <= x + w + 0.5, "{dur:?} outside the card at {x}+{w}");
    d.ok("ui.move", json!({"x": x + 20.0, "y": y + h + 11.0}));
    d.frames(6);
    assert!(d.painted_texts().iter().any(|t| t.source == CLIP_NAME && !t.elided), "no tooltip with the full clip name");
    // a short name is drawn whole, with no tooltip
    d.exec("clip.rename", json!({"item": items[0], "name": "A001"}));
    d.frames(6);
    let texts = d.painted_texts();
    let short: Vec<_> = texts.iter().filter(|t| t.source == "A001").collect();
    assert_eq!(short.len(), 1, "{short:?}");
    assert!(!short[0].elided && short[0].text == "A001");
}

#[test]
fn freeform_view_places_and_stacks_cards() {
    let mut d = project_driver();
    let (bin, items) = d.footage();
    d.exec("project.view.set", json!({"view": "freeform"}));
    d.ok("ui.menu.invoke", json!({"id": "projectPanel.openBin", "params": {"bin": bin, "how": "inPlace"}}));
    let card = format!("project.item.{}", items[0]);
    d.wait_for(&card);
    assert!(d.has("project.freeform"));
    let r = d.rect(&card);
    let canvas = d.rect("project.freeform");
    let from = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
    let to = (from.0 + 140.0, from.1 + 90.0);
    d.ok("ui.drag", json!({"from": {"x": from.0, "y": from.1}, "to": {"x": to.0, "y": to.1}, "steps": 16}));
    d.frames(4);
    let pos = d.app().session.project.item(ItemId(items[0])).unwrap().metadata.get(FREEFORM_POS).cloned().expect("card placed");
    let (x, y, _) = filmcraft_engine::project_panel::parse_pos(&pos).unwrap();
    let (ex, ey) = (r[0] - canvas[0] + 140.0, r[1] - canvas[1] + 90.0);
    assert!((x - ex).abs() < 4.0 && (y - ey).abs() < 4.0, "placed at {x},{y}, expected {ex},{ey}");
    d.frames(2);
    let r2 = d.rect(&card);
    assert!((r2[0] - r[0] - 140.0).abs() < 4.0, "card drawn where it was dropped");
    d.snapshot("project-freeform", Some("panel.Project"));
    // the move is one undo step
    d.exec("edit.undo", json!({}));
    assert!(!d.app().session.project.item(ItemId(items[0])).unwrap().metadata.contains_key(FREEFORM_POS));
    // stack two cards from the card menu
    d.exec("project.select", json!({"items": [items[0], items[1]]}));
    d.ok("ui.click", json!({"id": card, "button": "right"}));
    d.frames(2);
    d.click("project.cardMenu.stack");
    assert_eq!(d.app().session.project.item(ItemId(items[1])).unwrap().metadata.get(FREEFORM_STACK).map(String::as_str), Some(items[0].to_string().as_str()));
    d.frames(2);
    assert!(d.has(&format!("project.stack.{}", items[0])), "×2 badge");
    // Freeform View Options… from the panel menu
    d.click("panel.menu.Project");
    d.click("project.menu.freeformOptions");
    assert!(d.has("projectDialog.snap"));
    d.click("projectDialog.snap");
    d.click("projectDialog.ok");
    assert!(d.app().session.prefs.project_panel.freeform.snap);
    assert!(d.app().ui.project_panel.dialog.is_none());
}

#[test]
fn metadata_display_dialog_and_view_presets() {
    let mut d = project_driver();
    d.exec("project.view.set", json!({"view": "list"}));
    let (bin, _) = d.footage();
    d.ok("ui.menu.invoke", json!({"id": "projectPanel.openBin", "params": {"bin": bin, "how": "inPlace"}}));
    d.frames(2);
    // the panel menu lists Premiere's items
    d.click("panel.menu.Project");
    for id in [
        "project.menu.closeProject",
        "project.menu.newBin",
        "project.menu.newSearchBin",
        "project.menu.rename",
        "project.menu.automateToSequence",
        "project.menu.view.List",
        "project.menu.view.Freeform",
        "project.menu.previewArea",
        "project.menu.hoverScrub",
        "project.menu.fontSize",
        "project.menu.refreshSortOrder",
        "project.menu.metadataDisplay",
        "project.menu.saveViewPresetAs",
        "project.menu.restoreViewPreset",
        "project.menu.freeformOptions",
    ] {
        assert!(d.has(id), "{id}");
    }
    d.click("project.menu.metadataDisplay");
    d.frames(4);
    assert!(matches!(d.app().ui.project_panel.dialog, Some(filmcraft_ui_egui::panels::project::ProjectDialog::MetadataDisplay { .. })));
    d.click("projectDialog.field.Label");
    // fields further down: narrow the list with the search box
    d.click("projectDialog.filter");
    d.ok("ui.type", json!({"text": "usage"}));
    d.frames(2);
    d.click("projectDialog.field.Video Usage");
    d.click("projectDialog.filter");
    d.key("Cmd+A");
    d.ok("ui.type", json!({"text": "tape"}));
    d.frames(2);
    d.click("projectDialog.field.Tape Name");
    // move Label up next to Name
    for _ in 0..12 {
        if !d.has("projectDialog.up.Label") {
            break;
        }
        d.click("projectDialog.up.Label");
    }
    d.snapshot("project-metadata-display", None);
    d.click("projectDialog.ok");
    let cols: Vec<String> = d.app().session.prefs.project_panel.view.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(cols[0], "Name");
    assert_eq!(cols[1], "Label", "{cols:?}");
    assert!(cols.contains(&"Video Usage".to_string()) && !cols.contains(&"Tape Name".to_string()));
    d.frames(2);
    assert!(d.has("project.list.header.Label") && d.has("project.list.header.Video Usage"));

    // Save As New View Preset (named), change the view, restore it from Manage
    d.click("panel.menu.Project");
    d.click("project.menu.saveViewPresetAs");
    d.ok("ui.set", json!({"projectPanel": {"dialog": {"savePresetAs": {"name": "Logging"}}}}));
    d.frames(2);
    d.click("projectDialog.ok");
    assert_eq!(d.app().session.prefs.project_panel.presets[0].as_ref().unwrap().name, "Logging");
    d.exec("project.view.set", json!({"view": "icon", "fontSize": "large"}));
    d.click("panel.menu.Project");
    d.click("project.menu.manageViewPresets");
    d.click("projectDialog.preset.1");
    d.click("projectDialog.restore");
    assert_eq!(d.app().session.prefs.project_panel.view.mode, ViewMode::List);
    d.click("projectDialog.ok");
    assert!(d.app().ui.project_panel.dialog.is_none());
    // Font Size and Preview Area
    d.exec("project.view.set", json!({"fontSize": "extraLarge", "previewArea": true}));
    let first = d.footage().1[0];
    d.exec("project.select", json!({"items": [first]}));
    d.frames(3);
    assert!(d.has("project.previewArea"));
    d.snapshot("project-list-preview-xl", Some("panel.Project"));
}

#[test]
fn bins_open_in_place_in_a_tab_or_in_a_window() {
    let mut d = project_driver();
    d.exec("project.view.set", json!({"view": "list"}));
    let (bin, items) = d.footage();
    d.app().session.prefs.general.bins_double_click = "openNewTab".into();
    d.frames(2);
    double_click_bin(&mut d, bin, false);
    assert_eq!(d.app().ui.project_panel.tabs.len(), 1);
    assert!(d.has("project.tab.0") && d.has("project.tab.project"));
    // the tab shows the bin's items under its own ids
    d.wait_for(&format!("projectBin.0.item.{}", items[0]));
    d.click("project.tab.project");
    assert_eq!(d.app().ui.project_panel.active_tab, None);
    d.click("project.tab.0.close");
    assert!(d.app().ui.project_panel.tabs.is_empty());
    // Opt+double-click: Open in New Window (a floating bin panel)
    double_click_bin(&mut d, bin, true);
    assert!(d.app().ui.project_panel.tabs.first().is_some_and(|t| t.floating), "{:?}", d.app().ui.project_panel.tabs);
    d.wait_for("projectBin.0.window");
    // a second Project panel on the project
    d.ok("ui.menu.invoke", json!({"id": "projectPanel.newPanel"}));
    d.frames(3);
    assert!(d.has("projectBin.1.window"));
    d.snapshot("project-bin-windows", None);
    // inline rename of a bin
    d.ok("ui.menu.invoke", json!({"id": "projectPanel.rename", "params": {"bin": bin}}));
    d.frames(3);
    d.key("Cmd+A");
    d.ok("ui.type", json!({"text": "Camera A"}));
    d.key("Enter");
    let name = d.app().session.project.root.find_bin(filmcraft_project::BinId(bin)).unwrap().name.clone();
    assert_eq!(name, "Camera A");
}

#[test]
fn footer_buttons_are_wired() {
    let mut d = project_driver();
    let bins0 = d.app().session.project.root.children.len();
    d.click("project.button.file.newBin");
    assert_eq!(d.app().session.project.root.children.len(), bins0 + 1);
    d.click("project.button.find");
    assert!(d.app().ui.extras.dialog.is_some(), "Find… dialog");
    d.app().ui.extras.dialog = None;
    d.click("project.button.new-item");
    d.wait_for("project.newItem.file.newColorMatte");
    let n0 = d.app().session.project.items.len();
    d.click("project.newItem.file.newColorMatte");
    // the New Color Matte dialog asks for the color first (#29)
    assert!(d.app().ui.extras.dialog.is_some(), "New Color Matte dialog");
    d.click("colorMatte.ok");
    assert_eq!(d.app().session.project.items.len(), n0 + 1);
    d.click("project.button.project.delete");
    assert_eq!(d.app().session.project.items.len(), n0, "Clear removes the selected new matte");
    for v in ["List", "Icon", "Freeform"] {
        d.click(&format!("project.view.{v}"));
        assert_eq!(format!("{:?}", d.app().session.prefs.project_panel.view.mode), v);
    }
    d.click("project.view.Icon");
    assert!(d.has("project.sortIcons"));
    d.click("project.sortIcons");
    d.click("project.sortIcons.Name");
    assert_eq!(d.app().session.prefs.project_panel.view.icon_sort.column, "Name");
}

// ------------------------------------------------------------------------------------ Media Browser

fn media_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fc-mb-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Selects")).unwrap();
    let wav = filmcraft_media::wav::write_wav16(&[0.0; 9600], 2, 48_000);
    for n in ["a-interview.wav", "b-roll.wav"] {
        std::fs::write(dir.join(n), &wav).unwrap();
    }
    std::fs::write(dir.join("Selects").join("pick.wav"), &wav).unwrap();
    std::fs::write(dir.join("readme.txt"), b"not media").unwrap();
    dir
}

#[test]
fn file_dialog_import_uses_shown_bin_unless_explicit() {
    let dir = media_dir("import-bin");
    let path = dir.join("a-interview.wav").to_string_lossy().into_owned();
    let mut app = FilmcraftApp::new(Session::default());
    app.hooks.pick_files = Some(Box::new(move |_| vec![path.clone()]));
    let parent = app.session.execute("file.newBin", json!({"name": "Footage"})).unwrap()["bin"].as_u64().unwrap();
    let child = app.session.execute("file.newBin", json!({"name": "Selects", "parent": parent})).unwrap()["bin"].as_u64().unwrap();
    let root = app.session.project.root.id.0;
    app.ui.project_panel.tabs.push(filmcraft_ui_egui::panels::project::BinTab { bin: parent, ..Default::default() });
    for (shown, tab, params, expected) in [
        (None, None, json!({}), root),
        (Some(child), None, json!({}), child),
        (Some(child), Some(0), json!({}), parent),
        (Some(child), None, json!({"bin": null}), root),
        (Some(parent), None, json!({"bin": child}), child),
    ] {
        app.ui.project_panel.bin = shown;
        app.ui.project_panel.active_tab = tab;
        let result = app.file_dialog("file.import", &params).unwrap();
        assert!(result["errors"].as_array().unwrap().is_empty(), "{result}");
        let item = ItemId(result["items"][0].as_u64().unwrap());
        let bin = app.session.project.root.find_bin(filmcraft_project::BinId(expected)).unwrap();
        assert!(bin.children.contains(&BinEntry::Item(item)), "shown={shown:?}, tab={tab:?}, params={params}: expected bin {expected}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The disk through a slow card reader: opening a file waits while the gate is closed.
struct SlowCard(Arc<AtomicBool>);

impl SlowCard {
    fn wait(&self) {
        let t0 = std::time::Instant::now();
        while !self.0.load(Ordering::Relaxed) && t0.elapsed() < std::time::Duration::from_secs(30) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

impl Services for SlowCard {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.wait();
        FsServices.read_file(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        FsServices.write_file(path, data)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        FsServices.file_size(path)
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        FsServices.read_range(path, offset, len)
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        self.wait();
        FsServices.reader(path)
    }
    fn list_dir(&self, dir: &str) -> Option<std::io::Result<Vec<String>>> {
        FsServices.list_dir(dir)
    }
    fn list_entries(&self, dir: &str) -> Option<std::io::Result<Vec<filmcraft_engine::media_browser::DirEntry>>> {
        FsServices.list_entries(dir)
    }
    fn is_dir(&self, path: &str) -> bool {
        FsServices.is_dir(path)
    }
    fn canonical_path(&self, path: &str) -> Option<String> {
        FsServices.canonical_path(path)
    }
}

/// A folder dropped on the Project panel (as from Explorer or Finder) and File ▸ Import Folder…
/// import its media into bins named after its folders, inside the bin the panel shows. The files
/// are opened in the background: the window goes on, the Project panel and the status bar show
/// how far it got (with Cancel), and the bins come in, as one undo step, when it is done.
#[test]
fn dropped_folders_import_into_bins_in_the_background() {
    let dir = media_dir("drop-folder");
    let gate = Arc::new(AtomicBool::new(false));
    let mut d = Driver::with(Session::new(Arc::new(SlowCard(gate.clone()))));
    let shown = d.exec("file.newBin", json!({"name": "Dailies"}))["bin"].as_u64().unwrap();
    d.app().ui.project_panel.bin = Some(shown);
    let undo = d.app().session.history.undo.len();
    d.ok("ui.drop", json!({"paths": [dir.to_string_lossy()]}));
    d.frames(2);
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    // the card is slow: nothing came in yet, and the window says what it is doing
    d.wait_for("project.import.progress");
    let progress = d.label("project.import.progress");
    assert!(progress.starts_with(&format!("Importing {name}: ")), "{progress}");
    assert!(d.has("project.import.cancel") && d.has("status.job.cancel"), "{:?}", d.ids("status"));
    assert_eq!(d.app().ui.status, format!("Importing {name}…"));
    let in_dailies = |d: &mut Driver| d.app().session.project.root.find_bin(filmcraft_project::BinId(shown)).map_or(0, |b| b.children.len());
    assert_eq!(in_dailies(&mut d), 0);
    gate.store(true, Ordering::Relaxed);
    d.wait_gone("project.import.progress");
    let names = |b: &filmcraft_project::Bin, p: &filmcraft_project::Project| -> Vec<String> {
        let mut v: Vec<String> = b
            .children
            .iter()
            .map(|e| match e {
                BinEntry::Item(i) => p.item(*i).unwrap().name.clone(),
                BinEntry::Bin(b) => format!("{}/", b.name),
            })
            .collect();
        v.sort();
        v
    };
    let bin_in = |b: &filmcraft_project::Bin, n: &str| -> filmcraft_project::Bin {
        b.children.iter().find_map(|e| if let BinEntry::Bin(b) = e { (b.name == n).then(|| b.clone()) } else { None }).unwrap_or_else(|| panic!("no bin {n}"))
    };
    let p = d.app().session.project.clone();
    let dailies = p.root.find_bin(filmcraft_project::BinId(shown)).unwrap();
    let top = bin_in(dailies, &name);
    assert_eq!(names(&top, &p), ["Selects/", "a-interview.wav", "b-roll.wav"]);
    assert_eq!(names(&bin_in(&top, "Selects"), &p), ["pick.wav"]);
    assert_eq!(d.app().session.history.undo.len(), undo + 1);
    // readme.txt was skipped, and the status bar says so
    let status = d.app().ui.status.clone();
    assert!(status.contains("3 imported") && status.contains("1 skipped"), "{status}");
    // File ▸ Import Folder… asks for the folder
    let selects = dir.join("Selects").to_string_lossy().into_owned();
    d.app().hooks.pick_folder = Some(Box::new(move || Some(selects.clone())));
    d.app().ui.project_panel.bin = None;
    let r = d.ok("ui.menu.invoke", json!({"id": "file.importFolder"}));
    assert!(r["background"] == json!(true), "{r}");
    d.wait_import(r["job"].as_u64().unwrap());
    let p = d.app().session.project.clone();
    assert_eq!(names(&bin_in(&p.root, "Selects"), &p), ["pick.wav"]);
    assert_eq!(d.app().ui.status, "Folder import: 1 imported");
    // Cancel in the Project panel: nothing comes in
    gate.store(false, Ordering::Relaxed);
    d.ok("ui.drop", json!({"paths": [dir.to_string_lossy()]}));
    d.frames(2);
    d.wait_for("project.import.cancel");
    let before = d.app().session.project.clone();
    d.click("project.import.cancel");
    gate.store(true, Ordering::Relaxed);
    d.wait_gone("project.import.progress");
    assert!(Arc::ptr_eq(&d.app().session.project, &before));
    let status = d.app().ui.status.clone();
    assert!(status.starts_with("Cancelled"), "{status}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn media_browser_navigation_import_and_columns() {
    let dir = media_dir("nav");
    let d0 = dir.to_string_lossy().to_string();
    let mut d = Driver::with(Session::default());
    d.ok("ui.panel.show", json!({"panel": "MediaBrowser"}));
    d.ok("ui.set", json!({"focused": "MediaBrowser"}));
    d.ok("ui.menu.invoke", json!({"id": "window.maximizeFrame"}));
    d.exec("mediaBrowser.navigate", json!({"path": d0}));
    d.frames(3);
    d.wait_for("mediaBrowser.entry.a-interview.wav");
    assert!(d.has("mediaBrowser.entry.Selects") && !d.has("mediaBrowser.entry.readme.txt"));
    for id in [
        "mediaBrowser.back",
        "mediaBrowser.forward",
        "mediaBrowser.up",
        "mediaBrowser.path",
        "mediaBrowser.fileTypes",
        "mediaBrowser.ingest",
        "mediaBrowser.importAsImageSequence",
        "mediaBrowser.tree.localDrives",
    ] {
        assert!(d.has(id), "{id}");
    }
    // double-click a folder, then Back / Forward
    // type a directory into the path field
    d.click("mediaBrowser.path");
    d.key("Cmd+A");
    d.ok("ui.type", json!({"text": format!("{d0}/Selects")}));
    d.key("Enter");
    d.wait_for("mediaBrowser.entry.pick.wav");
    d.click("mediaBrowser.back");
    d.wait_for("mediaBrowser.entry.b-roll.wav");
    d.click("mediaBrowser.forward");
    d.wait_for("mediaBrowser.entry.pick.wav");
    d.click("mediaBrowser.up");
    d.wait_for("mediaBrowser.entry.b-roll.wav");
    // Favorites and Recent Directories in the tree
    d.click("panel.menu.MediaBrowser");
    d.click("mediaBrowser.menu.favorite");
    assert_eq!(d.app().session.prefs.media_browser.favorites, std::slice::from_ref(&d0));
    d.frames(2);
    let fav_name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(d.has(&format!("mediaBrowser.tree.favorites.{fav_name}")));
    assert!(d.has("mediaBrowser.tree.recent"));
    // select two files and Import from the panel menu
    d.click("mediaBrowser.entry.a-interview.wav");
    d.ok("ui.click", json!({"id": "mediaBrowser.entry.b-roll.wav", "command": true}));
    d.frames(2);
    assert_eq!(d.app().session.browser.selection.len(), 2);
    assert!(d.label("mediaBrowser.count").starts_with("2 of 2"));
    d.snapshot("media-browser-list", Some("panel.MediaBrowser"));
    d.click("panel.menu.MediaBrowser");
    d.click("mediaBrowser.menu.import");
    assert_eq!(d.app().session.project.items.len(), 2);
    // Shift+O opens the selection in the Source monitor (already imported: no duplicate)
    d.click("mediaBrowser.entry.b-roll.wav");
    d.ok("ui.set", json!({"focused": "MediaBrowser"}));
    d.key("Shift+O");
    assert!(d.app().session.state.source_item.is_some());
    assert_eq!(d.app().session.project.items.len(), 2);
    // Edit Columns…
    d.click("panel.menu.MediaBrowser");
    d.click("mediaBrowser.menu.editColumns");
    d.click("mediaBrowser.columns.Audio Info");
    d.click("mediaBrowser.columns.ok");
    assert!(d.app().session.prefs.media_browser.columns.contains(&"Audio Info".to_string()));
    d.frames(6);
    assert!(d.has("mediaBrowser.header.Audio Info"));
    // thumbnails view and the file-type filter
    d.click("mediaBrowser.view.thumbnails");
    assert_eq!(d.app().session.prefs.media_browser.view, "thumbnails");
    d.wait_for("mediaBrowser.entry.b-roll.wav");
    d.snapshot("media-browser-thumbnails", Some("panel.MediaBrowser"));
    d.exec("mediaBrowser.settings", json!({"fileTypes": "image", "view": "list"}));
    d.frames(3);
    assert!(!d.has("mediaBrowser.entry.b-roll.wav") && d.has("mediaBrowser.entry.Selects"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn media_browser_drag_imports_into_the_project_panel() {
    let dir = media_dir("drag");
    let mut session = Session::default();
    session.execute("file.openDemoProject", json!({})).unwrap();
    let mut d = Driver::with(session);
    // the Editing workspace has Project and Media Browser in one group: move the browser to the
    // Source monitor's group so both are on show
    d.ok("ui.set", json!({"workspace": "Editing"}));
    d.app().ui.dock.close(PanelKind::MediaBrowser);
    d.app().ui.dock.open_near(PanelKind::MediaBrowser, PanelKind::Source);
    d.ok("ui.panel.show", json!({"panel": "MediaBrowser"}));
    d.ok("ui.panel.show", json!({"panel": "Project"}));
    d.exec("mediaBrowser.navigate", json!({"path": dir.to_string_lossy()}));
    d.frames(3);
    d.wait_for("mediaBrowser.entry.b-roll.wav");
    let n0 = d.app().session.project.items.len();
    // two files dropped on the Media Browser itself: both imports are taken back (only the last
    // one used to be)
    d.click("mediaBrowser.entry.a-interview.wav");
    d.ok("ui.click", json!({"id": "mediaBrowser.entry.b-roll.wav", "command": true}));
    d.frames(2);
    assert_eq!(d.app().session.browser.selection.len(), 2);
    let e = d.rect("mediaBrowser.entry.b-roll.wav");
    let from = (e[0] + 40.0, e[1] + e[3] / 2.0);
    d.ok("ui.drag", json!({"from": {"x": from.0, "y": from.1}, "to": {"x": from.0 + 30.0, "y": from.1 + 60.0}}));
    d.frames(6);
    assert_eq!(d.app().session.project.items.len(), n0, "dropped nowhere");
    // dropped on the Timeline: imported and placed
    d.click("mediaBrowser.entry.b-roll.wav");
    let tl = d.rect("panel.Timeline");
    d.ok("ui.drag", json!({"from": {"x": from.0, "y": from.1}, "to": {"x": tl[0] + tl[2] * 0.6, "y": tl[1] + tl[3] * 0.75}, "steps": 20}));
    d.frames(6);
    assert_eq!(d.app().session.project.items.len(), n0 + 1, "kept after a drop on the Timeline");
    // a folder dropped on the Timeline imports nothing
    let f = d.rect("mediaBrowser.entry.Selects");
    let from = (f[0] + 40.0, f[1] + f[3] / 2.0);
    d.ok("ui.drag", json!({"from": {"x": from.0, "y": from.1}, "to": {"x": tl[0] + tl[2] * 0.6, "y": tl[1] + tl[3] * 0.75}, "steps": 20}));
    d.frames(6);
    assert_eq!(d.app().session.project.items.len(), n0 + 1, "a folder on the Timeline");
    // a folder dropped on a bin of the Project panel becomes a bin inside it, with its media, as
    // when dropped from the file manager (it used to be undraggable)
    let cards = d.exec("file.newBin", json!({"name": "Cards"}))["bin"].as_u64().unwrap();
    let target = format!("project.bin.{cards}");
    d.wait_for(&target);
    let b = d.rect(&target);
    d.ok("ui.drag", json!({"from": {"x": from.0, "y": from.1}, "to": {"x": b[0] + b[2] / 2.0, "y": b[1] + b[3] / 2.0}, "steps": 20}));
    d.frames(6);
    let p = d.app().session.project.clone();
    let in_cards = p.root.find_bin(filmcraft_project::BinId(cards)).unwrap();
    let selects = in_cards
        .children
        .iter()
        .find_map(|e| if let BinEntry::Bin(b) = e { (b.name == "Selects").then(|| b.clone()) } else { None })
        .unwrap_or_else(|| panic!("no Selects bin in Cards: {:?}", in_cards.children));
    let names: Vec<String> =
        selects.children.iter().filter_map(|e| if let BinEntry::Item(i) = e { Some(p.item(*i).unwrap().name.clone()) } else { None }).collect();
    assert_eq!(names, ["pick.wav"]);
    assert_eq!(d.app().ui.status, "Folder import: 1 imported");
    assert_eq!(d.app().session.project.items.len(), n0 + 2);
    let _ = std::fs::remove_dir_all(&dir);
}
