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

use std::collections::{HashMap, HashSet};

use filmcraft_media::sequence::Numbered;
use filmcraft_project::BinId;
use serde_json::{Value, json};

use crate::commands::{Imported, import_one};
use crate::panels::Level;
use crate::{Services, Session};

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
}

/// The folders of one `file.import`: what was entered, skipped and made, and the caps.
#[derive(Debug, Default)]
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

/// Import the folder `path` into a new bin under `parent` (the root when `None`): its media files
/// and its sub-folders, as bins.
pub(crate) fn import(s: &mut Session, path: &str, parent: Option<BinId>, w: &mut Walk, got: &mut Imported) {
    let services = s.services.clone();
    let skipped = w.skipped_count;
    let folder = match walk(&*services, path, folder_name(path), 0, w) {
        Ok(Some(f)) => f,
        Ok(None) => return,
        Err(e) => {
            got.errors.push(format!("{path}: {e}"));
            return;
        }
    };
    if folder.is_empty() {
        let others = w.skipped_count.saturating_sub(skipped);
        let why = if others > 0 { format!(" ({others} other files skipped)") } else { String::new() };
        got.errors.push(format!("{path}: no media files to import in this folder{why}"));
        return;
    }
    place(s, &folder, parent, w, got);
}

/// Make the bin of `folder` under `parent` and import into it, sub-folders first. Recursion is as
/// deep as the walk went (at most [`MAX_DEPTH`]).
fn place(s: &mut Session, folder: &Folder, parent: Option<BinId>, w: &mut Walk, got: &mut Imported) {
    let name = folder.name.clone();
    let bin = match s.edit(&format!("New Bin {name}"), |p, _| Ok(p.add_bin(&name, parent))) {
        Ok(b) => b,
        Err(e) => {
            got.errors.push(format!("{name}: {e}"));
            return;
        }
    };
    for sub in &folder.folders {
        place(s, sub, Some(bin), w, got);
    }
    import_files(s, &folder.files, bin, got);
    // a folder whose every file failed leaves no empty bin behind
    if s.project.root.find_bin(bin).is_some_and(|b| b.children.is_empty()) {
        if let Err(e) = s.edit("Remove Empty Bin", |p, _| Ok(p.root.remove_bin(bin))) {
            got.errors.push(format!("{name}: {e}"));
        }
    } else {
        w.bins.push(bin.0);
    }
}

/// Import a folder's media files into `bin`. With Settings ▸ Media ▸ Import image sequences on, a
/// run of numbered stills (same name around the number) is one image-sequence clip, from its
/// lowest number, as a single numbered still is when imported on its own.
fn import_files(s: &mut Session, files: &[String], bin: BinId, got: &mut Imported) {
    let mut first_of: HashMap<usize, bool> = HashMap::new();
    if s.prefs.media.import_image_sequences {
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
    for (i, path) in files.iter().enumerate() {
        match first_of.get(&i) {
            Some(true) => import_one(s, path, Some(bin), true, false, got),
            Some(false) => {} // a later frame of a sequence
            None => import_one(s, path, Some(bin), false, false, got),
        }
    }
}
