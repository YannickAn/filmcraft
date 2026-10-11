//! `file.import` of a folder (a folder dropped on the Project panel, File ▸ Import Folder…, a
//! folder path from the CLI, control channel or MCP), as in Premiere: every media file in it and in
//! its sub-folders is imported, the folder becomes a bin of its name and each sub-folder a bin
//! inside it. Folders without media make no bin. Other files (documents, captions, projects…) are
//! skipped and reported, never errors; hidden and system files are left alone.
//!
//! The tree on disk is untrusted: a folder linked from inside itself is entered once (links are
//! resolved through [`Services::canonical_path`]), and the depth, the number of folders, of entries
//! looked at and of files imported are capped. A walk that hits a cap imports what it found so far
//! and says why it stopped.
//!
//! An import has two parts: [`scan`] looks through the folders and opens every media file (the
//! slow part: one container index read per file), and [`finish`] adds the bins and items to the
//! project in one edit and one undo step. With `file.import {background: true}` (the UI's drops and
//! File ▸ Import Folder…) the scan is a job on another thread, with progress and Cancel in the
//! status bar, the Progress panel and `jobs.list`, and [`poll`] brings its result in on the
//! session's thread when it is done; `jobs.list` then shows what `file.import` returns under
//! `import`. Cancel imports nothing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use filmcraft_export::Progress;
use filmcraft_media::SharedSource;
use filmcraft_media::sequence::Numbered;
use filmcraft_project::{Bin, BinEntry, BinId, ItemKind, Label, MediaIdentity, Project};
use filmcraft_time::FrameRate;
use serde_json::{Value, json};

use crate::commands::{Imported, finish_import, import_one, media_item};
use crate::media_pool::MediaPool;
use crate::panels::Level;
use crate::{EngineError, Event, Result, Services, Session};

/// Folders nested deeper than this below the imported one are skipped.
pub const MAX_DEPTH: usize = 32;
/// Media files one import takes from folders.
pub const MAX_FILES: usize = 10_000;
/// Folders one import enters.
pub const MAX_FOLDERS: usize = 2_000;
/// Directory entries one import looks at.
pub const MAX_ENTRIES: usize = 200_000;
/// Skipped files listed in the result (`skippedCount` counts them all).
const LISTED_SKIPS: usize = 200;
/// Skipped files the Events panel names.
const NAMED_SKIPS: usize = 10;
/// Why a cancelled import job brought nothing in.
pub const CANCELLED: &str = "Cancelled: nothing was imported";
/// Why an import job for a project that was closed brought nothing in.
const PROJECT_CLOSED: &str = "Cancelled: the project was closed before the import was done; nothing was imported";
/// Finished background imports whose results `jobs.list` shows.
const KEPT_RESULTS: usize = 32;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One folder of the walk: its media files and its sub-folders that hold media.
#[derive(Debug, Default)]
pub(crate) struct Folder {
    pub name: String,
    pub files: Vec<String>,
    pub folders: Vec<Folder>,
}

impl Folder {
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.folders.is_empty()
    }

    /// The files [`scan`] opens in it and below it.
    fn to_open(&self, sequences: bool) -> usize {
        self.folders.iter().fold(plan_files(&self.files, sequences).len(), |n, f| n.saturating_add(f.to_open(sequences)))
    }
}

/// The folders of one `file.import`: what was entered, skipped and made, and the caps.
#[derive(Default)]
pub(crate) struct Walk {
    seen: HashSet<String>,
    /// Folders entered.
    pub folders: usize,
    entries: usize,
    files: usize,
    /// Why the walk stopped early (the first cap it hit).
    pub stopped: Option<String>,
    /// (path, reason), the first [`LISTED_SKIPS`].
    pub skipped: Vec<(String, String)>,
    pub skipped_count: usize,
    /// Bins made, inner ones first.
    pub bins: Vec<u64>,
    /// The job's progress (a background import): what the walk has found so far, and Cancel.
    progress: Option<Arc<Progress>>,
    cancelled: bool,
}

impl Walk {
    fn skip(&mut self, path: String, reason: impl Into<String>) {
        self.skipped_count = self.skipped_count.saturating_add(1);
        if self.skipped.len() < LISTED_SKIPS {
            self.skipped.push((path, reason.into()));
        }
    }

    fn stop(&mut self, why: String) {
        self.stopped.get_or_insert(why);
    }

    /// Add the bins made and the files skipped to `file.import`'s result `out`, and tell the
    /// Events panel what was left out.
    pub(crate) fn report(&self, s: &mut Session, out: &mut Value) {
        out["bins"] = json!(self.bins);
        out["skipped"] = json!(self.skipped.iter().map(|(path, reason)| json!({"path": path, "reason": reason})).collect::<Vec<_>>());
        out["skippedCount"] = json!(self.skipped_count);
        if let Some(why) = &self.stopped {
            out["warnings"] = json!([why]);
            s.log.push(Level::Warning, "file.import", why.clone());
        }
        if self.skipped_count > 0 {
            let named: Vec<String> = self.skipped.iter().take(NAMED_SKIPS).map(|(p, why)| format!("{} ({why})", file_name(p))).collect();
            let more = match self.skipped_count.saturating_sub(named.len()) {
                0 => String::new(),
                n => format!(", and {n} more"),
            };
            s.log.push(Level::Warning, "file.import", format!("Not imported from folders: {}{more}", named.join(", ")));
        }
    }
}

/// The bin name for a folder path: its last component (`E:` for a drive root).
pub(crate) fn folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => path.to_string(),
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Hidden files and folders (a leading dot, as "._" resource forks on camera cards) and the
/// system's own (thumbnail caches, a drive's recycle bin) are passed over without a word.
fn is_hidden(name: &str) -> bool {
    const SYSTEM: [&str; 5] = ["thumbs.db", "ehthumbs.db", "desktop.ini", "$recycle.bin", "system volume information"];
    name.is_empty() || name.starts_with('.') || name.contains('/') || name == "Icon\r" || SYSTEM.contains(&name.to_lowercase().as_str())
}

/// Why a file found in a folder is not imported; `None`: it is media.
fn skip_reason(name: &str) -> Option<&'static str> {
    if filmcraft_media::is_importable(std::path::Path::new(name)) {
        return None;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    Some(match crate::media_browser::kind_of(name) {
        Some("caption") => "captions: import the file on its own to add a caption track",
        Some("project") => "project or interchange file: import it on its own",
        _ if matches!(ext.as_str(), "aaf" | "omf") => "project or interchange file: import it on its own",
        _ => "unsupported file type",
    })
}

/// List the folder `dir` (shown as `name`) and, `depth` levels down, its sub-folders. `Ok(None)`:
/// the folder is not entered (already entered through a link, or a cap was hit); an error: it
/// can't be listed.
pub(crate) fn walk(services: &dyn Services, dir: &str, name: String, depth: usize, w: &mut Walk) -> std::io::Result<Option<Folder>> {
    if w.stopped.is_some() {
        return Ok(None);
    }
    if let Some(p) = &w.progress {
        if p.cancel.load(Ordering::Relaxed) {
            w.cancelled = true;
            w.stop(CANCELLED.into());
            return Ok(None);
        }
        *lock(&p.status) = format!("Looking through folders: {} media files in {} folders", w.files, w.folders);
    }
    let key = services.canonical_path(dir).unwrap_or_else(|| dir.to_string());
    if !w.seen.insert(key) {
        w.skip(dir.to_string(), "folder already imported (a link to it, or given twice)");
        return Ok(None);
    }
    if w.folders >= MAX_FOLDERS {
        w.stop(format!("Folder import stopped at {MAX_FOLDERS} folders: the rest was not imported"));
        return Ok(None);
    }
    w.folders += 1;
    let mut entries = services.list_entries(dir).unwrap_or_else(|| Err(std::io::Error::other("this host cannot list folders")))?;
    entries.sort_by_cached_key(|e| (e.name.to_lowercase(), e.name.clone()));
    let mut folder = Folder { name, ..Default::default() };
    for e in entries {
        if w.stopped.is_some() {
            break;
        }
        if is_hidden(&e.name) {
            continue;
        }
        w.entries += 1;
        if w.entries > MAX_ENTRIES {
            w.stop(format!("Folder import stopped after {MAX_ENTRIES} files and folders: the rest was not imported"));
            break;
        }
        let path = crate::media_browser::join(dir, &e.name);
        if e.is_dir {
            if depth >= MAX_DEPTH {
                w.skip(path, format!("folder nested more than {MAX_DEPTH} levels deep"));
                continue;
            }
            match walk(services, &path, e.name, depth + 1, w) {
                Ok(Some(f)) if !f.is_empty() => folder.folders.push(f),
                Ok(_) => {}
                Err(err) => w.skip(path, format!("the folder can't be read: {err}")),
            }
        } else if let Some(why) = skip_reason(&e.name) {
            w.skip(path, why);
        } else if w.files >= MAX_FILES {
            w.stop(format!("Folder import stopped at {MAX_FILES} media files: the rest was not imported"));
        } else {
            w.files += 1;
            folder.files.push(path);
        }
    }
    Ok(Some(folder))
}

/// Which of a folder's media files are imported, by index, and whether as an image sequence. With
/// Settings ▸ Media ▸ Import image sequences on, a run of numbered stills (same name around the
/// number) is one image-sequence clip, from its lowest number, as a single numbered still is when
/// imported on its own.
fn plan_files(files: &[String], sequences: bool) -> Vec<(usize, bool)> {
    let mut first_of: HashMap<usize, bool> = HashMap::new();
    if sequences {
        let mut runs: HashMap<(String, String), Vec<(u64, usize)>> = HashMap::new();
        for (i, f) in files.iter().enumerate() {
            if let Some(n) = Numbered::parse(f) {
                runs.entry((n.prefix, n.suffix)).or_default().push((n.number, i));
            }
        }
        for run in runs.values().filter(|r| r.len() > 1) {
            let Some(&(_, first)) = run.iter().min() else { continue };
            let Some(n) = files.get(first).and_then(|f| Numbered::parse(f)) else { continue };
            // the frames the sequence will play (same padding as its first one)
            let frames: Vec<usize> = run.iter().filter(|(_, i)| files.get(*i).is_some_and(|f| n.number_of(file_name(f)).is_some())).map(|r| r.1).collect();
            if frames.len() > 1 {
                for i in frames {
                    first_of.insert(i, i == first);
                }
            }
        }
    }
    (0..files.len())
        .filter_map(|i| match first_of.get(&i) {
            Some(true) => Some((i, true)),
            Some(false) => None, // a later frame of a sequence
            None => Some((i, false)),
        })
        .collect()
}

/// What a folder import takes from the session, to look through folders and open files on
/// another thread.
pub(crate) struct Request {
    paths: Vec<String>,
    services: Arc<dyn Services>,
    pool: Arc<MediaPool>,
    /// Settings ▸ Media ▸ Import image sequences.
    sequences: bool,
    /// Settings ▸ Media ▸ Indeterminate Media Timebase, the rate image sequences play at.
    rate: FrameRate,
}

impl Request {
    pub(crate) fn new(s: &Session, paths: Vec<String>) -> Self {
        Self {
            paths,
            services: s.services.clone(),
            pool: s.media.clone(),
            sequences: s.prefs.media.import_image_sequences,
            rate: crate::settings::timebase_rate(&s.prefs.media.indeterminate_timebase),
        }
    }
}

/// A media file of a folder, opened, or why it can't be.
struct Opened {
    path: String,
    media: std::result::Result<Media, String>,
}

struct Media {
    src: SharedSource,
    identity: MediaIdentity,
    /// An image sequence: its frame count and the missing frame numbers (from the first).
    sequence: Option<(usize, Vec<usize>)>,
}

/// A folder with its media files opened.
struct Scanned {
    name: String,
    files: Vec<Opened>,
    folders: Vec<Scanned>,
}

/// One path of a folder import.
enum Root<T> {
    /// A folder: what is in it (`None`: not entered, see [`walk`]) or why it can't be listed, and
    /// how many of its files were skipped.
    Folder { path: String, tree: std::result::Result<Option<T>, String>, skipped: usize },
    /// A file given with the folders: imported as `file.import` imports files.
    File(String),
}

/// What [`scan`] found: the folders with their media files opened, and the walk.
pub(crate) struct Scan {
    roots: Vec<Root<Scanned>>,
    walk: Walk,
    /// Files opened (or tried), and how long it took.
    opened: usize,
    seconds: f64,
}

/// Look through the folders among `req.paths` and open their media files. With `progress` (a
/// background job) it says how far it got and stops when cancelled (`None`).
pub(crate) fn scan(req: &Request, progress: Option<&Arc<Progress>>) -> Option<Scan> {
    let t0 = web_time::Instant::now();
    let mut walk = Walk { progress: progress.cloned(), ..Default::default() };
    let mut found = Vec::new();
    for path in &req.paths {
        if !req.services.is_dir(path) {
            found.push(Root::File(path.clone()));
            continue;
        }
        let before = walk.skipped_count;
        let tree = self::walk(&*req.services, path, folder_name(path), 0, &mut walk).map_err(|e| e.to_string());
        found.push(Root::Folder { path: path.clone(), tree, skipped: walk.skipped_count.saturating_sub(before) });
    }
    if walk.cancelled {
        return None;
    }
    let total = found.iter().fold(0usize, |n, r| match r {
        Root::Folder { tree: Ok(Some(f)), .. } => n.saturating_add(f.to_open(req.sequences)),
        _ => n,
    });
    if let Some(p) = progress {
        p.total.store(total as u64, Ordering::Relaxed);
    }
    let mut opener = Opener { req, progress, done: 0, total };
    let mut roots = Vec::with_capacity(found.len());
    for r in found {
        roots.push(match r {
            Root::Folder { path, tree: Ok(Some(f)), skipped } => Root::Folder { path, tree: Ok(Some(opener.folder(f)?)), skipped },
            Root::Folder { path, tree: Ok(None), skipped } => Root::Folder { path, tree: Ok(None), skipped },
            Root::Folder { path, tree: Err(e), skipped } => Root::Folder { path, tree: Err(e), skipped },
            Root::File(path) => Root::File(path),
        });
    }
    if let Some(p) = progress {
        *lock(&p.status) = format!("Adding {total} files to the project…");
    }
    Some(Scan { roots, walk, opened: opener.done, seconds: t0.elapsed().as_secs_f64() })
}

struct Opener<'a> {
    req: &'a Request,
    progress: Option<&'a Arc<Progress>>,
    done: usize,
    total: usize,
}

impl Opener<'_> {
    /// `f` with its media files opened, sub-folders first (the order they are imported in);
    /// `None`: cancelled. Recursion is as deep as the walk went (at most [`MAX_DEPTH`]).
    fn folder(&mut self, f: Folder) -> Option<Scanned> {
        let mut folders = Vec::with_capacity(f.folders.len());
        for sub in f.folders {
            folders.push(self.folder(sub)?);
        }
        let mut files = Vec::new();
        for (i, as_sequence) in plan_files(&f.files, self.req.sequences) {
            let Some(path) = f.files.get(i) else { continue };
            if let Some(p) = self.progress {
                if p.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                *lock(&p.status) = format!("{} ({} of {})", file_name(path), self.done.saturating_add(1), self.total);
            }
            // a file that takes a decoder down is one failed file, not a failed import
            let media = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open(self.req, path, as_sequence)))
                .unwrap_or_else(|_| Err("internal error while opening the file (see the crash log)".to_string()));
            files.push(Opened { path: path.clone(), media });
            self.done = self.done.saturating_add(1);
            if let Some(p) = self.progress {
                p.done.store(self.done as u64, Ordering::Relaxed);
            }
        }
        Some(Scanned { name: f.name, files, folders })
    }
}

/// Open a media file of a folder as `file.import` opens one: through the host's random-access
/// reader when it has one (only the container index is read now), or read whole.
fn open(req: &Request, path: &str, as_sequence: bool) -> std::result::Result<Media, String> {
    let services = &*req.services;
    let media_err = |e: filmcraft_media::MediaError| EngineError::from(e).to_string();
    if as_sequence {
        let frames = crate::media_pool::image_sequence_frames(path, services).map_err(media_err)?;
        let missing = frames.iter().enumerate().filter(|(_, f)| f.is_none()).map(|(i, _)| i).collect();
        let src = crate::media_pool::open_image_sequence(path, services, req.rate, &item_name(path)).map_err(media_err)?;
        let identity = crate::relink::identity_of(services, path).map_err(|e| e.to_string())?;
        return Ok(Media { src, identity, sequence: Some((frames.len(), missing)) });
    }
    if services.reader(path).is_some() {
        let identity = crate::relink::identity_of(services, path).map_err(|e| e.to_string())?;
        let src = req.pool.open_file(path, services).map_err(media_err)?;
        return Ok(Media { src, identity, sequence: None });
    }
    let bytes = services.read_file(path).map_err(|e| e.to_string())?;
    let identity = crate::relink::identity_of_bytes(&bytes);
    let src = req.pool.open_bytes(&item_name(path), bytes.into()).map_err(media_err)?;
    Ok(Media { src, identity, sequence: None })
}

/// The Project panel name of an imported file (as `file.import` names files).
fn item_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// Bring in a [`scan`] as `file.import` of folders does: one undo step named `label` (the label of
/// its only edit when there is one), and nothing changes when nothing could be imported.
pub(crate) fn finish(s: &mut Session, found: Scan, bin: Option<BinId>, label: String) -> Result<Value> {
    s.grouped(|s| {
        let mut got = Imported::default();
        let walk = apply(s, found, bin, &label, &mut got);
        Ok((finish_import(s, got, Some(&walk))?, label))
    })
}

/// A bin to make, with what goes in it.
struct NewBin {
    name: String,
    bins: Vec<NewBin>,
    items: Vec<NewItem>,
}

struct NewItem {
    path: String,
    name: String,
    label: Label,
    kind: ItemKind,
    src: SharedSource,
    sequence: Option<(usize, Vec<usize>)>,
}

/// A media item [`add`] made: its source, and the frame count and missing frames of an image
/// sequence.
type MadeItem = (filmcraft_project::ItemId, String, SharedSource, Option<(usize, Vec<usize>)>);

/// What [`add`] made: bins (inner ones first) and items.
#[derive(Default)]
struct Made {
    bins: Vec<u64>,
    items: Vec<MadeItem>,
}

/// Add what [`scan`] found to the project with one edit (one copy of the project, whatever the
/// number of files): a bin for every folder with media in it, an item for every file that opened.
/// Files given with the folders come in afterwards, as `file.import` imports files. Returns the
/// walk with the bins made.
fn apply(s: &mut Session, found: Scan, bin: Option<BinId>, label: &str, got: &mut Imported) -> Walk {
    let Scan { roots, mut walk, .. } = found;
    let mut new_bins = Vec::new();
    let mut files = Vec::new();
    for root in roots {
        match root {
            Root::File(path) => files.push(path),
            Root::Folder { path, tree: Err(e), .. } => got.errors.push(format!("{path}: {e}")),
            Root::Folder { tree: Ok(None), .. } => {}
            Root::Folder { path, tree: Ok(Some(f)), skipped } if f.files.is_empty() && f.folders.is_empty() => {
                let why = if skipped > 0 { format!(" ({skipped} other files skipped)") } else { String::new() };
                got.errors.push(format!("{path}: no media files to import in this folder{why}"));
            }
            Root::Folder { tree: Ok(Some(f)), .. } => new_bins.extend(new_bin(&s.prefs, f, got)),
        }
    }
    if !new_bins.is_empty() {
        let r = s.edit(label, |p, _| {
            let mut made = Made::default();
            for b in new_bins {
                let tree = add(p, b, &mut made);
                p.insert_bin(tree, bin);
            }
            Ok(made)
        });
        match r {
            Ok(made) => {
                for (id, path, src, sequence) in made.items {
                    s.media.insert_file(id, &path, src);
                    got.ids.push(id.0);
                    if let Some((frames, missing)) = sequence {
                        got.image_sequences.push(json!({"item": id.0, "frames": frames, "missing": missing}));
                    }
                }
                walk.bins = made.bins;
            }
            Err(e) => got.errors.push(e.to_string()),
        }
    }
    for path in files {
        import_one(s, &path, bin, false, false, got);
    }
    walk
}

/// The bin of a scanned folder, with the items of the files that opened; `None` when nothing in
/// it or below it opened (no empty bins). The others are `got`'s errors.
fn new_bin(prefs: &crate::autosave::Preferences, f: Scanned, got: &mut Imported) -> Option<NewBin> {
    let bins: Vec<NewBin> = f.folders.into_iter().filter_map(|sub| new_bin(prefs, sub, got)).collect();
    let mut items = Vec::new();
    for o in f.files {
        match o.media {
            Ok(m) => {
                let (label, kind) = media_item(prefs, &o.path, &m.src, m.identity);
                items.push(NewItem { name: item_name(&o.path), path: o.path, label, kind, src: m.src, sequence: m.sequence });
            }
            Err(e) => got.errors.push(format!("{}: {e}", o.path)),
        }
    }
    (!bins.is_empty() || !items.is_empty()).then_some(NewBin { name: f.name, bins, items })
}

/// Make the bin `b` (sub-bins first, then items), not yet in the project's tree.
fn add(p: &mut Project, b: NewBin, made: &mut Made) -> Bin {
    let mut bin = Bin { id: BinId(p.alloc_id()), name: b.name, children: Vec::with_capacity(b.bins.len().saturating_add(b.items.len())) };
    for sub in b.bins {
        bin.children.push(BinEntry::Bin(add(p, sub, made)));
    }
    for it in b.items {
        let id = p.new_item(&it.name, it.label, it.kind);
        bin.children.push(BinEntry::Item(id));
        made.items.push((id, it.path, it.src, it.sequence));
    }
    made.bins.push(bin.id.0);
    bin
}

/// Folder imports running in the background, and what the last ones brought in.
#[derive(Default)]
pub struct Imports {
    pending: Vec<Pending>,
    /// (job, what `file.import` returned or `{"error"}`), the latest last.
    done: VecDeque<(u64, Value)>,
}

impl Imports {
    /// What the import job `job` brought in (`file.import`'s result with `job`), or
    /// `{job, error}`; `None` while it runs.
    pub fn result(&self, job: u64) -> Option<&Value> {
        self.done.iter().rev().find(|(j, _)| *j == job).map(|(_, v)| v)
    }

    /// The import jobs still running, the oldest first.
    pub fn running(&self) -> impl Iterator<Item = u64> + '_ {
        self.pending.iter().map(|p| p.job)
    }
}

/// A background folder import: brought in by [`poll`] when its scan is done.
struct Pending {
    job: u64,
    bin: Option<BinId>,
    label: String,
    path: String,
    /// The project it is for ([`crate::History`]'s).
    project: Arc<()>,
    found: Arc<Mutex<Option<Scan>>>,
}

/// `file.import {background: true}` of folders: start a job that looks through them and opens
/// their files; [`poll`] brings them in when it is done. Returns `{job, background, label}`, or
/// with `wait` (and on hosts without threads) runs it now and returns what `file.import` returns.
pub(crate) fn start(s: &mut Session, paths: Vec<String>, bin: Option<BinId>, label: String, wait: bool) -> Result<Value> {
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0).saturating_add(1);
    let job = crate::Job { id, label: label.clone(), progress: Default::default(), result: Default::default() };
    *lock(&job.progress.status) = "Looking through folders…".into();
    let path = paths.first().cloned().unwrap_or_default();
    let req = Request::new(s, paths);
    let found: Arc<Mutex<Option<Scan>>> = Arc::default();
    let (prog, res, out) = (job.progress.clone(), job.result.clone(), found.clone());
    let run = move || match scan(&req, Some(&prog)) {
        // brought in by `poll` (the project is the session's)
        Some(f) => *lock(&out) = Some(f),
        None => {
            *lock(&prog.status) = CANCELLED.into();
            *lock(&res) = Some(Err(CANCELLED.into()));
            prog.finished.store(true, Ordering::Relaxed);
        }
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    let (progress, result) = (job.progress.clone(), job.result.clone());
    s.jobs.push(job);
    s.imports.pending.push(Pending { job: id, bin, label: label.clone(), path, project: s.history.project.clone(), found });
    let inline = wait || cfg!(target_arch = "wasm32");
    if inline {
        run();
    } else if let Err(e) = std::thread::Builder::new().name("filmcraft-folder-import".into()).spawn(run) {
        let e = format!("can't start the import: {e}");
        *lock(&result) = Some(Err(e.clone()));
        progress.finished.store(true, Ordering::Relaxed);
        poll(s);
        return Err(EngineError::Other(e));
    }
    if !inline {
        return Ok(json!({"job": id, "background": true, "label": label}));
    }
    poll(s);
    match s.imports.result(id).cloned() {
        Some(v) => match v.get("error").and_then(Value::as_str) {
            Some(e) => Err(EngineError::Other(e.to_string())),
            None => Ok(v),
        },
        None => Err(EngineError::Other(format!("{label}: the import did not finish"))),
    }
}

/// Bring in the background imports whose files are open (one undo step each) and drop the
/// cancelled and failed ones; each finished import is an [`Event::Imported`]. An import for a
/// project that was closed (a new or another project opened) is stopped and brings nothing in.
/// Called once per UI frame from [`Session::poll_persistence`], by `jobs.list`, and after
/// synchronous runs.
pub fn poll(s: &mut Session) {
    let mut i = 0;
    while let Some(p) = s.imports.pending.get(i) {
        let id = p.job;
        let found = lock(&p.found).take();
        let job = s.jobs.iter().find(|j| j.id == id).cloned();
        let gone = !Arc::ptr_eq(&p.project, &s.history.project);
        if let Some(j) = job.as_ref().filter(|_| gone) {
            j.progress.cancel.store(true, Ordering::Relaxed);
        }
        let finished = job.as_ref().is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        if found.is_none() && !finished {
            i += 1;
            continue;
        }
        let p = s.imports.pending.remove(i);
        let cancelled = job.as_ref().is_some_and(|j| j.progress.cancel.load(Ordering::Relaxed));
        let r = match found {
            _ if gone => Err(PROJECT_CLOSED.to_string()),
            // cancelled or failed while looking: the job says why
            None => Err(job.as_ref().and_then(|j| lock(&j.result).clone()).and_then(|r| r.err()).unwrap_or_else(|| CANCELLED.to_string())),
            Some(_) if cancelled => Err(CANCELLED.to_string()),
            Some(f) => {
                let (opened, seconds) = (f.opened, f.seconds);
                finish(s, f, p.bin, p.label.clone()).map(|v| (v, opened, seconds)).map_err(|e| e.to_string())
            }
        };
        let value = match r {
            Ok((mut v, opened, seconds)) => {
                if let Some(j) = job.as_ref().filter(|j| !j.progress.finished.load(Ordering::Relaxed)) {
                    let items = v["items"].as_array().map_or(0, Vec::len);
                    let failed = v["errors"].as_array().map_or(0, Vec::len);
                    let skipped = v["skippedCount"].as_u64().unwrap_or(0);
                    *lock(&j.progress.status) = format!("Imported {items} files ({skipped} skipped, {failed} failed) in {seconds:.1}s");
                    let report = filmcraft_export::Report {
                        path: p.path.clone(),
                        frames: opened as u64,
                        seconds,
                        bytes: 0,
                        render_fps: opened as f64 / seconds.max(1e-6),
                        extra_files: Vec::new(),
                    };
                    *lock(&j.result) = Some(Ok(report));
                    j.progress.finished.store(true, Ordering::Relaxed);
                }
                v["job"] = json!(id);
                v
            }
            Err(e) => {
                if let Some(j) = job.as_ref().filter(|j| !j.progress.finished.load(Ordering::Relaxed)) {
                    *lock(&j.progress.status) = e.clone();
                    *lock(&j.result) = Some(Err(e.clone()));
                    j.progress.finished.store(true, Ordering::Relaxed);
                }
                json!({"job": id, "error": e})
            }
        };
        if s.imports.done.len() >= KEPT_RESULTS {
            s.imports.done.pop_front();
        }
        s.imports.done.push_back((id, value.clone()));
        s.events.push(Event::Imported { job: id, result: value });
    }
}
