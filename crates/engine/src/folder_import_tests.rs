//! Importing folders (`file.import` of a folder path, File ▸ Import Folder…): media into bins named
//! after the folders, other files skipped and reported, one undo step, and hostile trees (link
//! loops, endless depth, huge folders) bounded.
//!
//! Fixtures (ffmpeg, `target/fixtures/engine/folder_import/`): a short H.264 movie and a WAV tone,
//! copied into a nested temporary folder. Background imports (`background: true`: progress,
//! Cancel, one edit when done) run on an in-memory card whose reads wait for the test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use filmcraft_media::MediaKind;
use filmcraft_project::{Bin, BinEntry};
use serde_json::{Value, json};

use crate::folder_import::{CANCELLED, MAX_DEPTH, MAX_ENTRIES, MAX_FILES, MAX_FOLDERS, Walk, folder_name, walk};
use crate::media_browser::DirEntry;
use crate::media_test_util::tmp_dir;
use crate::{Event, Services, Session};

fn fixtures() -> Option<(PathBuf, PathBuf)> {
    let ffmpeg = filmcraft_testkit::oracle::ffmpeg_or_skip("folder import")?;
    let dir = filmcraft_testkit::fixtures_dir("engine/folder_import");
    let run = |args: &[&str], tmp: &Path| {
        std::process::Command::new(&ffmpeg).args(["-y", "-loglevel", "error"]).args(args).arg(tmp).status().is_ok_and(|s| s.success())
    };
    let movie = filmcraft_testkit::fixtures::generate(&dir.join("clip_144x108.mp4"), |tmp| {
        run(&["-f", "lavfi", "-i", "testsrc2=size=144x108:rate=24:duration=0.5", "-c:v", "libx264", "-preset", "fast", "-pix_fmt", "yuv420p"], tmp)
    })?;
    let tone = filmcraft_testkit::fixtures::generate(&dir.join("tone_48k.wav"), |tmp| {
        run(&["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=0.5", "-c:a", "pcm_s16le"], tmp)
    })?;
    Some((movie, tone))
}

fn still(path: &Path) {
    image::RgbaImage::from_pixel(32, 18, image::Rgba([200, 99, 7, 255])).save(path).unwrap();
}

/// A link to the folder `target` at `link` (a junction on Windows, which needs no privileges).
/// `false` when the system refuses.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
            || std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().is_ok_and(|o| o.status.success())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

/// `{name, items: [names, sorted], bins: [...]}` of a bin and everything in it.
fn tree(s: &Session, b: &Bin) -> Value {
    let mut items = Vec::new();
    let mut bins = Vec::new();
    for c in &b.children {
        match c {
            BinEntry::Item(i) => items.push(s.project.item(*i).map(|it| it.name.clone()).unwrap_or_default()),
            BinEntry::Bin(b) => bins.push(tree(s, b)),
        }
    }
    items.sort();
    json!({"name": b.name, "items": items, "bins": bins})
}

fn bin_named<'a>(b: &'a Bin, name: &str) -> Option<&'a Bin> {
    b.children.iter().find_map(|c| match c {
        BinEntry::Bin(b) if b.name == name => Some(b),
        _ => None,
    })
}

fn file_names(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = r["skipped"].as_array().unwrap().iter().map(|k| folder_name(k["path"].as_str().unwrap())).collect();
    v.sort();
    v
}

#[test]
fn ffmpeg_a_folder_imports_into_nested_bins_as_one_undo_step() {
    let Some((movie, tone)) = fixtures() else { return };
    let root = tmp_dir("folder-import");
    let shoot = root.join("Shoot");
    for d in ["Audio", "Stills/Day 2", "Empty", "Junk"] {
        std::fs::create_dir_all(shoot.join(d)).unwrap();
    }
    std::fs::copy(&movie, shoot.join("A001.mp4")).unwrap();
    std::fs::copy(&movie, shoot.join("._A001.mp4")).unwrap(); // a resource fork on a camera card
    std::fs::write(shoot.join("notes.txt"), "call sheet").unwrap();
    std::fs::write(shoot.join("subs.srt"), "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
    std::fs::copy(&tone, shoot.join("Audio").join("tone.wav")).unwrap();
    still(&shoot.join("Stills").join("Day 2").join("still.png"));
    std::fs::write(shoot.join("Junk").join("readme.md"), "#").unwrap();
    // a link back to the top folder: entered once, not imported again and again
    let looped = link_dir(&shoot, &shoot.join("Stills").join("Back to Shoot"));

    let mut s = Session::default();
    s.execute("events.clear", json!({})).unwrap();
    let undo = s.history.undo.len();
    let r = s.execute("file.import", json!({"paths": [shoot.to_string_lossy()]})).unwrap();
    assert_eq!(r["errors"], json!([]), "{r}");
    assert_eq!(r["items"].as_array().unwrap().len(), 3, "{r}");
    assert_eq!(r["bins"].as_array().unwrap().len(), 4, "{r}");
    let mut skipped = vec!["notes.txt", "readme.md", "subs.srt"];
    if looped {
        skipped.insert(0, "Back to Shoot");
        let link = r["skipped"].as_array().unwrap().iter().find(|k| k["path"].as_str().unwrap().ends_with("Back to Shoot")).unwrap();
        assert!(link["reason"].as_str().unwrap().contains("already imported"), "{link}");
    }
    assert_eq!(file_names(&r), skipped, "{r}");
    assert_eq!(r["skippedCount"], json!(skipped.len()));

    // the bins mirror the folders; folders without media make none
    let b = bin_named(&s.project.root, "Shoot").unwrap_or_else(|| panic!("no Shoot bin: {r}"));
    assert_eq!(
        tree(&s, b),
        json!({"name": "Shoot", "items": ["A001.mp4"], "bins": [
            {"name": "Audio", "items": ["tone.wav"], "bins": []},
            {"name": "Stills", "items": [], "bins": [{"name": "Day 2", "items": ["still.png"], "bins": []}]},
        ]})
    );
    let kinds: Vec<(String, MediaKind)> = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            let it = s.project.item(filmcraft_project::ItemId(i.as_u64().unwrap())).unwrap();
            (it.name.clone(), it.as_media().unwrap().info.kind)
        })
        .collect();
    assert!(kinds.contains(&("A001.mp4".into(), MediaKind::Movie)), "{kinds:?}");
    assert!(kinds.contains(&("tone.wav".into(), MediaKind::AudioOnly)), "{kinds:?}");
    assert!(kinds.contains(&("still.png".into(), MediaKind::Still)), "{kinds:?}");

    // the Events panel says what was left out, and why
    let ev = s.execute("events.list", json!({"level": "warning"})).unwrap();
    let said = ev["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["source"] == "file.import" && e["message"].as_str().unwrap().contains("notes.txt (unsupported file type)"));
    assert!(said, "{ev}");

    // one undo step takes the whole folder back, and redo brings it again
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(s.history.undo.last().unwrap().0, "Import Shoot");
    s.execute("edit.undo", json!({})).unwrap();
    assert!(bin_named(&s.project.root, "Shoot").is_none());
    assert!(s.project.items.values().all(|i| i.as_media().is_none()));
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(tree(&s, bin_named(&s.project.root, "Shoot").unwrap())["items"], json!(["A001.mp4"]));

    // File ▸ Import Folder… into a chosen bin; a file is not a folder
    let dailies = s.execute("file.newBin", json!({"name": "Dailies"})).unwrap()["bin"].as_u64().unwrap();
    s.execute("file.importFolder", json!({"path": shoot.join("Audio").to_string_lossy(), "bin": dailies})).unwrap();
    let d = bin_named(&s.project.root, "Dailies").unwrap();
    assert_eq!(tree(&s, d), json!({"name": "Dailies", "items": [], "bins": [{"name": "Audio", "items": ["tone.wav"], "bins": []}]}));
    let e = s.execute("file.importFolder", json!({"path": shoot.join("A001.mp4").to_string_lossy()})).unwrap_err().to_string();
    assert!(e.contains("not a folder"), "{e}");

    // files and folders together: one step for all of it
    let undo = s.history.undo.len();
    let r = s.execute("file.import", json!({"paths": [shoot.join("A001.mp4").to_string_lossy(), shoot.join("Audio").to_string_lossy()]})).unwrap();
    assert_eq!(r["items"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!((s.history.undo.len(), s.history.undo.last().unwrap().0.as_str()), (undo + 1, "Import"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_folder_without_media_is_an_error_that_changes_nothing() {
    let root = tmp_dir("folder-import-empty");
    let docs = root.join("Docs");
    std::fs::create_dir_all(docs.join("Inner")).unwrap();
    std::fs::write(docs.join("a.txt"), "a").unwrap();
    std::fs::write(docs.join("Inner").join("b.pdf"), "b").unwrap();
    let mut s = Session::default();
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let e = s.execute("file.import", json!({"paths": [docs.to_string_lossy()]})).unwrap_err().to_string();
    assert!(e.contains("no media files") && e.contains("2 other files skipped"), "{e}");
    assert!(Arc::ptr_eq(&s.project, &before));
    assert_eq!(s.history.undo.len(), undo);
    // an empty folder too
    let e = s.execute("file.import", json!({"paths": [root.join("Docs").join("Inner").to_string_lossy()]})).unwrap_err().to_string();
    assert!(e.contains("no media files"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn numbered_stills_in_a_folder_follow_the_import_image_sequences_setting() {
    let root = tmp_dir("folder-import-seq");
    let dir = root.join("Render");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["shot_0001.png", "shot_0002.png", "shot_0003.png", "cover.png", "other_0007.png"] {
        still(&dir.join(name));
    }
    let path = dir.to_string_lossy().to_string();
    let mut s = Session::default();
    // off: every still is a clip
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    assert_eq!(r["items"].as_array().unwrap().len(), 5, "{r}");
    // on: a run of numbered stills is one image-sequence clip, from its first frame
    s.execute("prefs.set", json!({"key": "media.importImageSequences", "value": true})).unwrap();
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    assert_eq!(r["items"].as_array().unwrap().len(), 3, "{r}");
    assert_eq!(r["imageSequences"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["imageSequences"][0]["frames"], 3);
    let seq = s.project.item(filmcraft_project::ItemId(r["imageSequences"][0]["item"].as_u64().unwrap())).unwrap();
    assert_eq!((seq.name.as_str(), seq.as_media().unwrap().info.kind), ("shot_0001.png", MediaKind::ImageSequence));
    s.execute("prefs.set", json!({"key": "media.importImageSequences", "value": false})).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bin_names_are_the_last_folder_name() {
    assert_eq!(folder_name("/Volumes/Card/DCIM/"), "DCIM");
    assert_eq!(folder_name(r"C:\Users\me\Footage"), "Footage");
    assert_eq!(folder_name(r"E:\"), "E:");
    assert_eq!(folder_name("/"), "/");
    assert_eq!(folder_name("Shoot"), "Shoot");
}

/// A folder tree that never ends: every folder, whatever its path, holds `dirs` folders and
/// `files` files `f<i>.<ext>`. Without `resolves_links` it is a link loop as a host that can't
/// resolve links sees it; with it, every folder is the same one.
struct Endless {
    dirs: usize,
    files: usize,
    ext: &'static str,
    resolves_links: bool,
}

impl Services for Endless {
    fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
        Ok(b"not really media".to_vec())
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn is_dir(&self, path: &str) -> bool {
        !path.rsplit('/').next().unwrap_or_default().contains('.')
    }
    fn canonical_path(&self, _: &str) -> Option<String> {
        self.resolves_links.then(|| "/loop".to_string())
    }
    fn list_entries(&self, _: &str) -> Option<std::io::Result<Vec<DirEntry>>> {
        let dirs = (0..self.dirs).map(|i| DirEntry { name: format!("d{i}"), is_dir: true, ..Default::default() });
        let files = (0..self.files).map(|i| DirEntry { name: format!("f{i}.{}", self.ext), ..Default::default() });
        Some(Ok(dirs.chain(files).collect()))
    }
}

fn walked(fs: Endless) -> (Option<crate::folder_import::Folder>, Walk) {
    let mut w = Walk::default();
    let f = walk(&fs, "/top", "top".into(), 0, &mut w).unwrap();
    (f, w)
}

fn depth(f: &crate::folder_import::Folder) -> usize {
    1 + f.folders.iter().map(depth).max().unwrap_or(0)
}

#[test]
fn hostile_trees_stop_at_link_loops_depth_and_counts() {
    // a folder linked inside itself is entered once
    let (f, w) = walked(Endless { dirs: 3, files: 2, ext: "txt", resolves_links: true });
    let f = f.unwrap();
    assert!(f.folders.is_empty() && f.files.is_empty());
    assert_eq!(w.skipped_count, 5);
    assert_eq!(w.skipped.iter().filter(|k| k.1.contains("already imported")).count(), 3);
    // links it can't see through: the depth cap ends the chain
    let (f, w) = walked(Endless { dirs: 1, files: 1, ext: "wav", resolves_links: false });
    let f = f.unwrap();
    assert_eq!((depth(&f), w.folders), (MAX_DEPTH + 1, MAX_DEPTH + 1));
    assert!(w.skipped.iter().any(|k| k.1.contains(&format!("more than {MAX_DEPTH} levels"))), "{:?}", w.skipped);
    assert!(w.stopped.is_none());
    // a wide tree stops at the folder cap
    let (_, w) = walked(Endless { dirs: 4, files: 0, ext: "wav", resolves_links: false });
    assert_eq!(w.folders, MAX_FOLDERS);
    assert!(w.stopped.as_deref().unwrap().contains("folders"), "{:?}", w.stopped);
    // a huge folder stops at the entry cap; only the first skips are listed
    let (_, w) = walked(Endless { dirs: 0, files: MAX_ENTRIES + 10, ext: "txt", resolves_links: false });
    assert_eq!(w.skipped_count, MAX_ENTRIES);
    assert!(w.skipped.len() <= 200);
    assert!(w.stopped.as_deref().unwrap().contains("files and folders"), "{:?}", w.stopped);
    // and at the media file cap
    let (f, w) = walked(Endless { dirs: 0, files: MAX_FILES + 10, ext: "wav", resolves_links: false });
    assert_eq!(f.unwrap().files.len(), MAX_FILES);
    assert!(w.stopped.as_deref().unwrap().contains("media files"), "{:?}", w.stopped);
}

#[test]
fn hostile_folders_through_file_import_leave_the_project_as_it_was() {
    // nothing but documents, endlessly: an error, nothing changed
    let mut s = Session::new(Arc::new(Endless { dirs: 2, files: 2, ext: "txt", resolves_links: false }));
    let before = s.project.clone();
    let e = s.execute("file.import", json!({"paths": ["/top"]})).unwrap_err().to_string();
    assert!(e.contains("no media files"), "{e}");
    assert!(Arc::ptr_eq(&s.project, &before));
    assert!(s.history.undo.is_empty());
    // media names that don't decode: every file fails, and no empty bins are left behind
    let mut s = Session::new(Arc::new(Endless { dirs: 1, files: 1, ext: "wav", resolves_links: false }));
    let before = s.project.clone();
    let e = s.execute("file.import", json!({"paths": ["/top"]})).unwrap_err().to_string();
    assert!(e.contains("f0.wav"), "{e}");
    assert!(Arc::ptr_eq(&s.project, &before));
    assert!(s.project.root.children.is_empty() && s.history.undo.is_empty());
    // a folder given as an image sequence's first frame is not walked
    let e = s.execute("file.import", json!({"paths": ["/top"], "imageSequence": true})).unwrap_err().to_string();
    assert!(!e.is_empty());
    assert!(s.project.root.children.is_empty());
}

/// A camera card in memory: `/card` holds `folders` folders `Clip <k>` of `files` WAV files each
/// (and the `extra` files in its first folder). Reading a file waits while `gate` is closed (a
/// slow card reader), and `panic.wav` takes the host down.
struct Card {
    folders: usize,
    files: usize,
    extra: Vec<&'static str>,
    wav: Vec<u8>,
    gate: Arc<AtomicBool>,
}

impl Card {
    fn new(folders: usize, files: usize) -> Self {
        let wav = filmcraft_media::wav::write_wav16(&[0.0; 480], 1, 48_000);
        Card { folders, files, extra: Vec::new(), wav, gate: Arc::new(AtomicBool::new(true)) }
    }
}

impl Services for Card {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let t0 = Instant::now();
        while !self.gate.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!path.ends_with("panic.wav"), "the card reader fell over");
        Ok(self.wav.clone())
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn file_size(&self, _: &str) -> std::io::Result<u64> {
        Ok(self.wav.len() as u64)
    }
    fn is_dir(&self, path: &str) -> bool {
        !path.rsplit('/').next().unwrap_or_default().contains('.')
    }
    fn list_entries(&self, dir: &str) -> Option<std::io::Result<Vec<DirEntry>>> {
        if dir == "/card" {
            return Some(Ok((0..self.folders).map(|k| DirEntry { name: format!("Clip {k:02}"), is_dir: true, ..Default::default() }).collect()));
        }
        let k = dir.rsplit(' ').next().unwrap_or_default();
        let mut v: Vec<DirEntry> = (0..self.files).map(|i| DirEntry { name: format!("T{k}_{i:03}.wav"), ..Default::default() }).collect();
        if k == "00" {
            v.extend(self.extra.iter().map(|n| DirEntry { name: n.to_string(), ..Default::default() }));
        }
        Some(Ok(v))
    }
}

/// The `jobs.list` entry of `job` (an agent's view of it).
fn job_entry(s: &mut Session, job: u64) -> Value {
    let jobs = s.execute("jobs.list", json!({})).unwrap();
    jobs.as_array().unwrap().iter().find(|j| j["id"] == json!(job)).cloned().unwrap_or_else(|| panic!("no job {job}: {jobs}"))
}

/// Ask `jobs.list` (as an agent does, without a UI frame loop) until the job `job` is finished.
fn wait_job(s: &mut Session, job: u64) -> Value {
    let t0 = Instant::now();
    loop {
        let j = job_entry(s, job);
        if j["finished"] == json!(true) {
            return j;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "job {job} did not finish: {j}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_folder_imports_in_the_background_with_progress_as_one_undo_step() {
    let card = Card::new(3, 40);
    let gate = card.gate.clone();
    gate.store(false, Ordering::Relaxed);
    let mut s = Session::new(Arc::new(card));
    let (undo, revision) = (s.history.undo.len(), s.revision);
    let r = s.execute("file.import", json!({"paths": ["/card"], "background": true})).unwrap();
    assert_eq!((&r["background"], &r["label"]), (&json!(true), &json!("Import card")), "{r}");
    let job = r["job"].as_u64().unwrap();
    // the files are being opened: nothing came in yet, and the job says so
    let j = job_entry(&mut s, job);
    assert_eq!((&j["label"], &j["finished"]), (&json!("Import card"), &json!(false)), "{j}");
    assert!(j.get("import").is_none() && s.project.items.is_empty(), "{j}");
    gate.store(true, Ordering::Relaxed);
    let j = wait_job(&mut s, job);
    assert_eq!((&j["done"], &j["total"], &j["progress"]), (&json!(120), &json!(120), &json!(1.0)), "{j}");
    assert_eq!(j["result"]["frames"], json!(120), "{j}");
    assert!(j["status"].as_str().unwrap().starts_with("Imported 120 files"), "{j}");
    // what `file.import` returns, under `import`
    let v = &j["import"];
    assert_eq!((&v["job"], &v["errors"], &v["skippedCount"]), (&json!(job), &json!([]), &json!(0)), "{v}");
    assert_eq!((v["items"].as_array().unwrap().len(), v["bins"].as_array().unwrap().len()), (120, 4), "{v}");
    let b = bin_named(&s.project.root, "card").unwrap();
    assert_eq!(tree(&s, b)["bins"].as_array().unwrap().len(), 3);
    assert_eq!(tree(&s, bin_named(b, "Clip 01").unwrap())["items"].as_array().unwrap().len(), 40);
    // one edit for all 120 files (not a copy of the project per file), one undo step
    assert!(s.revision - revision <= 3, "{} edits", s.revision - revision);
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(s.history.undo.last().unwrap().0, "Import card");
    // the frontend hears of it, and the Events panel logs the job
    let events = s.drain_events();
    assert!(events.iter().any(|e| matches!(e, Event::Imported { job: j, result } if *j == job && result["items"] == v["items"])), "{events:?}");
    let ev = s.execute("events.list", json!({})).unwrap();
    assert!(ev["entries"].as_array().unwrap().iter().any(|e| e["message"] == json!("Finished: Import card")), "{ev}");
    // undo takes the whole folder back
    s.execute("edit.undo", json!({})).unwrap();
    assert!(bin_named(&s.project.root, "card").is_none() && s.project.items.is_empty());
}

#[test]
fn cancelling_a_background_import_brings_nothing_in() {
    let card = Card::new(2, 30);
    let gate = card.gate.clone();
    gate.store(false, Ordering::Relaxed);
    let mut s = Session::new(Arc::new(card));
    let before = s.project.clone();
    let job = s.execute("file.import", json!({"paths": ["/card"], "background": true})).unwrap()["job"].as_u64().unwrap();
    s.execute("jobs.cancel", json!({"job": job})).unwrap();
    gate.store(true, Ordering::Relaxed);
    let j = wait_job(&mut s, job);
    assert_eq!((&j["result"]["error"], &j["import"]["error"]), (&json!(CANCELLED), &json!(CANCELLED)), "{j}");
    assert!(Arc::ptr_eq(&s.project, &before) && s.history.undo.is_empty());
    assert!(s.drain_events().iter().any(|e| matches!(e, Event::Imported { job: j, result } if *j == job && result["error"] == json!(CANCELLED))));
    let ev = s.execute("events.list", json!({"level": "warning"})).unwrap();
    assert!(ev["entries"].as_array().unwrap().iter().any(|e| e["message"] == json!("Cancelled: Import card")), "{ev}");
}

#[test]
fn a_background_import_stops_when_its_project_is_closed() {
    let card = Card::new(2, 30);
    let gate = card.gate.clone();
    gate.store(false, Ordering::Relaxed);
    let mut s = Session::new(Arc::new(card));
    let job = s.execute("file.import", json!({"paths": ["/card"], "background": true})).unwrap()["job"].as_u64().unwrap();
    // File ▸ New ▸ Project keeps the media pool: the project it was for is gone all the same
    s.execute("file.newProject", json!({"name": "Other"})).unwrap();
    gate.store(true, Ordering::Relaxed);
    let j = wait_job(&mut s, job);
    let e = j["import"]["error"].as_str().unwrap_or_default();
    assert!(e.contains("project was closed"), "{j}");
    assert!(s.project.name == "Other" && s.project.items.is_empty() && s.history.undo.is_empty());
}

#[test]
fn a_file_that_takes_the_host_down_fails_on_its_own() {
    let mut card = Card::new(1, 3);
    card.extra = vec!["panic.wav", "notes.txt"];
    let mut s = Session::new(Arc::new(card));
    // `wait`: in the background, but returns what `file.import` returns when it is done
    let r = s.execute("file.import", json!({"paths": ["/card"], "background": true, "wait": true})).unwrap();
    assert!(r["job"].is_u64(), "{r}");
    assert_eq!((r["items"].as_array().unwrap().len(), &r["skippedCount"]), (3, &json!(1)), "{r}");
    let errors = r["errors"].as_array().unwrap();
    assert!(errors.len() == 1 && errors[0].as_str().unwrap().contains("panic.wav: internal error"), "{r}");
    // a flag that is not a boolean is no flag: imported now
    let r = s.execute("file.import", json!({"paths": ["/card"], "background": "yes"})).unwrap();
    assert!(r.get("job").is_none() && r["items"].as_array().unwrap().len() == 3, "{r}");
    assert_eq!(s.history.undo.len(), 2);
}

/// How long a folder import holds up the session's thread, in one go and in the background (where
/// it only brings the opened files in, one poll per UI frame). Media of your own:
/// `FILMCRAFT_FOLDER_IMPORT_BENCH=<folder> cargo test -p filmcraft-engine --lib folder_import_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn folder_import_bench() {
    let Ok(dir) = std::env::var("FILMCRAFT_FOLDER_IMPORT_BENCH") else { return };
    Session::default().execute("file.import", json!({"paths": [dir]})).unwrap(); // the file cache
    let mut s = Session::default();
    let t = Instant::now();
    let r = s.execute("file.import", json!({"paths": [dir]})).unwrap();
    let at_once = t.elapsed();
    let mut s = Session::default();
    let t = Instant::now();
    let job = s.execute("file.import", json!({"paths": [dir], "background": true})).unwrap()["job"].as_u64().unwrap();
    let started = t.elapsed();
    let (mut longest, mut frames) = (Duration::ZERO, 0);
    while s.imports.result(job).is_none() {
        std::thread::sleep(Duration::from_millis(16));
        let f = Instant::now();
        s.poll_persistence();
        longest = longest.max(f.elapsed());
        frames += 1;
    }
    let v = s.imports.result(job).unwrap();
    assert_eq!(v["items"].as_array().map(Vec::len), r["items"].as_array().map(Vec::len));
    eprintln!(
        "BENCH files={} file.import at once: {at_once:?} | background: start {started:?}, longest frame {longest:?} ({frames} frames), done after {:?}",
        r["items"].as_array().unwrap().len(),
        t.elapsed()
    );
}
