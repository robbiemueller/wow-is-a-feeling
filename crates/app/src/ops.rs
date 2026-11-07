//! The queue of things that change the disk: at most three at once, never two on the same
//! folder, and one casting bar that tells the truth about all of it. Pure, so it's tested
//! without threads.

use feeling_core::catalog::{Channel, Release};
use feeling_core::install::Stage;
use feeling_core::selfupdate::UpdateInfo;
use feeling_core::state::{ManagedAddon, Origin};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAX_RUNNING: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Install,
    Update,
    Reinstall,
    RollBack,
    Remove,
    Backup,
    Restore,
    AppUpdate,
}

impl Verb {
    /// The word on the casting bar.
    pub fn gerund(self) -> &'static str {
        match self {
            Verb::Install => "Installing",
            Verb::Update => "Updating",
            Verb::Reinstall => "Reinstalling",
            Verb::RollBack => "Rolling back",
            Verb::Remove => "Removing",
            Verb::Backup => "Backing up",
            Verb::Restore => "Restoring",
            Verb::AppUpdate => "Updating",
        }
    }
}

#[derive(Debug, Clone)]
pub enum OpKind {
    /// Download a catalog release and swap it in.
    Download {
        release: Release,
        channel: Channel,
        origin: Origin,
        old_folders: Vec<String>,
    },
    /// Swap in a zip we kept earlier.
    Rollback {
        zip: PathBuf,
        version: String,
        channel: Channel,
        origin: Origin,
        old_folders: Vec<String>,
    },
    /// A zip the player dropped on the window.
    LocalZip { path: PathBuf },
    Remove { folders: Vec<String> },
    BackupWtf,
    RestoreWtf { zip: PathBuf },
    SelfUpdate { update: Box<UpdateInfo> },
}

#[derive(Debug, Clone)]
pub struct Op {
    pub id: u64,
    pub verb: Verb,
    pub kind: OpKind,
    /// Empty for anything that isn't about one addon.
    pub addon_id: String,
    /// The name on the casting bar.
    pub name: String,
    /// Lowercased names of every folder this may touch.
    pub folders: HashSet<String>,
    /// Holds this one back until that one is finished, so dependencies land first.
    pub after: Option<u64>,
    /// Runs alone: nothing starts beside it, and nothing passes it in the queue.
    pub exclusive: bool,
}

impl Op {
    fn title(&self) -> String {
        match self.verb {
            Verb::Backup => "Backing up your settings".to_owned(),
            Verb::Restore => "Restoring your settings".to_owned(),
            Verb::AppUpdate => format!("Updating to {}", self.name),
            verb => format!("{} {}", verb.gerund(), self.name),
        }
    }

    fn overlaps(&self, other: &Op) -> bool {
        !self.folders.is_disjoint(&other.folders)
    }
}

/// Lowercased folder names, for comparing against the running ops.
pub fn folder_set<'a>(names: impl IntoIterator<Item = &'a String>) -> HashSet<String> {
    names.into_iter().map(|name| name.to_lowercase()).collect()
}

struct Running {
    op: Op,
    fraction: f32,
    cancel: Arc<AtomicBool>,
}

/// What the casting bar should say.
#[derive(Debug, Clone, PartialEq)]
pub struct Casting {
    pub text: String,
    pub fraction: f32,
}

#[derive(Default)]
pub struct Queue {
    next_id: u64,
    waiting: VecDeque<Op>,
    running: Vec<Running>,
    /// Ops counted on the bar since it was last empty, and how many of those are done.
    batch_total: usize,
    batch_done: usize,
}

impl Queue {
    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn is_idle(&self) -> bool {
        self.waiting.is_empty() && self.running.is_empty()
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// Is this addon queued or being worked on? Its buttons wait until it isn't.
    pub fn busy_with(&self, addon_id: &str) -> bool {
        !addon_id.is_empty()
            && (self.waiting.iter().any(|op| op.addon_id == addon_id)
                || self.running.iter().any(|run| run.op.addon_id == addon_id))
    }

    /// Is a restore or an app update on the go? Those lock everything else out.
    pub fn busy_exclusive(&self) -> bool {
        self.waiting.iter().any(|op| op.exclusive) || self.running.iter().any(|r| r.op.exclusive)
    }

    pub fn push(&mut self, op: Op) {
        if self.is_idle() {
            self.batch_total = 0;
            self.batch_done = 0;
        }
        if !op.exclusive {
            self.batch_total += 1;
        }
        self.waiting.push_back(op);
    }

    /// Every op that may start right now, each with the flag that cancels it.
    pub fn start_ready(&mut self) -> Vec<(Op, Arc<AtomicBool>)> {
        let mut started = Vec::new();
        let mut index = 0;
        while index < self.waiting.len() {
            let op = &self.waiting[index];
            if self.can_start(index) {
                let op = self.waiting.remove(index).expect("index was just checked");
                let cancel = Arc::new(AtomicBool::new(false));
                started.push((op.clone(), cancel.clone()));
                self.running.push(Running {
                    op,
                    fraction: 0.0,
                    cancel,
                });
                continue;
            }
            if op.exclusive {
                break;
            }
            index += 1;
        }
        started
    }

    fn can_start(&self, index: usize) -> bool {
        let op = &self.waiting[index];
        if self.running.len() >= MAX_RUNNING || self.running.iter().any(|r| r.op.exclusive) {
            return false;
        }
        if op.exclusive && !self.running.is_empty() {
            return false;
        }
        if self.running.iter().any(|r| r.op.overlaps(op)) {
            return false;
        }
        // Earlier ops on the same folders keep their place in line.
        if self.waiting.iter().take(index).any(|e| e.overlaps(op)) {
            return false;
        }
        op.after.is_none_or(|after| {
            !self.running.iter().any(|r| r.op.id == after)
                && !self.waiting.iter().any(|w| w.id == after)
        })
    }

    pub fn set_progress(&mut self, id: u64, fraction: f32) {
        if let Some(run) = self.running.iter_mut().find(|r| r.op.id == id) {
            // A later stage can report a lower raw number. The bar only moves forward.
            run.fraction = run.fraction.max(fraction.clamp(0.0, 1.0));
        }
    }

    pub fn finish(&mut self, id: u64) -> Option<Op> {
        let at = self.running.iter().position(|r| r.op.id == id)?;
        let run = self.running.swap_remove(at);
        if !run.op.exclusive {
            self.batch_done += 1;
        }
        Some(run.op)
    }

    /// Stop what's running where that's still possible and forget what hasn't started.
    /// Returns how many ops that touched.
    pub fn cancel_all(&mut self) -> usize {
        for run in &self.running {
            run.cancel.store(true, Ordering::Relaxed);
        }
        let dropped = self.waiting.len();
        let counted = self.waiting.iter().filter(|op| !op.exclusive).count();
        self.batch_total -= counted.min(self.batch_total);
        self.waiting.clear();
        dropped + self.running.len()
    }

    pub fn casting(&self) -> Option<Casting> {
        if self.is_idle() {
            return None;
        }
        if let Some(run) = self.running.iter().find(|r| r.op.exclusive) {
            return Some(Casting {
                text: run.op.title(),
                fraction: run.fraction,
            });
        }
        if self.batch_total <= 1 {
            let (title, fraction) = match (self.running.first(), self.waiting.front()) {
                (Some(run), _) => (run.op.title(), run.fraction),
                (None, Some(op)) => (op.title(), 0.0),
                (None, None) => return None,
            };
            return Some(Casting {
                text: title,
                fraction,
            });
        }
        let in_flight: f32 = self.running.iter().map(|r| r.fraction).sum();
        let verb = self
            .running
            .first()
            .map(|r| &r.op)
            .or(self.waiting.front())
            .map_or("Working on", |op| op.verb.gerund());
        let verb = if self.running.iter().all(|r| r.op.verb.gerund() == verb)
            && self.waiting.iter().all(|w| w.verb.gerund() == verb)
        {
            verb
        } else {
            "Working on"
        };
        Some(Casting {
            text: format!(
                "{verb} {} of {}",
                (self.batch_done + 1).min(self.batch_total),
                self.batch_total
            ),
            fraction: (self.batch_done as f32 + in_flight) / self.batch_total as f32,
        })
    }
}

/// How far along an install is, as a single number. Unpacking is quick next to the
/// download, so the download gets most of the bar.
pub fn stage_fraction(stage: Stage, expected_size: u64) -> f32 {
    const DOWNLOAD: f32 = 0.8;
    match stage {
        Stage::Downloading { done, total } => {
            let total = total.filter(|t| *t > 0).unwrap_or(expected_size);
            let part = if total > 0 {
                done as f32 / total as f32
            } else {
                // No idea how big it is, so creep toward the end and never get there.
                let done = done as f32;
                done / (done + 5_000_000.0)
            };
            part.min(1.0) * DOWNLOAD
        }
        Stage::Verifying => DOWNLOAD,
        Stage::Extracting { done, total } => {
            DOWNLOAD + 0.18 * (done as f32 / total.max(1) as f32).min(1.0)
        }
        Stage::Swapping => 0.98,
        Stage::Done => 1.0,
    }
}

/// Folders this addon owns that no other managed addon claims. These are the ones an
/// update may replace and a removal may delete.
pub fn own_folders(id: &str, managed: &BTreeMap<String, ManagedAddon>) -> Vec<String> {
    let Some(record) = managed.get(id) else {
        return Vec::new();
    };
    record
        .folders
        .iter()
        .filter(|folder| {
            !managed.iter().any(|(other, addon)| {
                other != id && addon.folders.iter().any(|f| f.eq_ignore_ascii_case(folder))
            })
        })
        .cloned()
        .collect()
}

/// The facts of a finished install.
pub struct Installed<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    pub channel: Channel,
    pub origin: Origin,
    pub folders: Vec<String>,
    /// From `install::kept_zips`: the version just installed, then the one before it.
    pub kept: &'a [PathBuf],
    pub keep_previous: bool,
    pub now: i64,
}

/// The record to store after an install, carrying over what should survive it.
pub fn record_after_install(prev: Option<&ManagedAddon>, facts: Installed<'_>) -> ManagedAddon {
    let same = prev.is_some_and(|p| p.version == facts.version);
    let (previous_version, previous_zip) = match prev {
        _ if !facts.keep_previous => (None, None),
        // A repair changes nothing about what came before it.
        Some(prev) if same => (prev.previous_version.clone(), prev.previous_zip.clone()),
        Some(prev) => match kept_zip_for(facts.kept, &prev.version) {
            Some(zip) => (Some(prev.version.clone()), Some(zip)),
            // Adopted or kept with the setting off: nothing to go back to.
            None => (None, None),
        },
        None => (None, None),
    };
    ManagedAddon {
        id: facts.id.to_owned(),
        name: facts.name.to_owned(),
        version: facts.version.to_owned(),
        channel: facts.channel,
        folders: facts.folders,
        installed_at: prev.map_or(facts.now, |p| p.installed_at),
        updated_at: facts.now,
        pinned: prev.is_some_and(|p| p.pinned),
        origin: facts.origin,
        previous_zip,
        previous_version,
    }
}

/// Which kept zip holds this version. Core names them after the version with anything
/// unusual replaced, and this spells it the same way.
fn kept_zip_for(kept: &[PathBuf], version: &str) -> Option<PathBuf> {
    let spelled: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let matches = |zip: &&PathBuf| {
        zip.file_stem()
            .is_some_and(|stem| stem.to_string_lossy().trim_start_matches('_') == spelled)
    };
    kept.iter().skip(1).find(matches).cloned()
}

/// The folder an unmanaged local zip is named after: the one the others hang off, or the
/// shortest name, which is what multi-folder addons do.
pub fn main_folder(folders: &[String]) -> Option<&String> {
    folders.iter().min_by_key(|name| {
        let lower = name.to_lowercase();
        let core = lower.ends_with("-core") || lower.ends_with("_core");
        (
            !core,
            name.len(),
            folders.iter().filter(|o| o.starts_with(name.as_str())).count() == 0,
        )
    })
}

/// Records that an install just made stale, so nobody offers updates for a folder that's
/// now something else. A zip dropped over a catalog addon takes its place, and a catalog
/// addon installed over a dropped zip does the same. Two catalog addons can share a folder.
pub fn stale_records(
    managed: &BTreeMap<String, ManagedAddon>,
    new_id: &str,
    new_origin: Origin,
    new_folders: &[String],
) -> Vec<String> {
    let new_folders = folder_set(new_folders);
    managed
        .iter()
        .filter(|(id, record)| {
            if id.as_str() == new_id {
                return false;
            }
            let either_local = new_origin == Origin::LocalZip || record.origin == Origin::LocalZip;
            let main_replaced = record
                .folders
                .first()
                .is_some_and(|main| new_folders.contains(&main.to_lowercase()));
            either_local && main_replaced
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Is this zip's path one the player could have meant? Used for drops.
pub fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(queue: &mut Queue, name: &str, folders: &[&str]) -> Op {
        let id = queue.next_id();
        Op {
            id,
            verb: Verb::Update,
            kind: OpKind::Remove {
                folders: Vec::new(),
            },
            addon_id: name.to_owned(),
            name: name.to_owned(),
            folders: folders.iter().map(|f| f.to_lowercase()).collect(),
            after: None,
            exclusive: false,
        }
    }

    fn names(started: &[(Op, Arc<AtomicBool>)]) -> Vec<&str> {
        started.iter().map(|(op, _)| op.name.as_str()).collect()
    }

    #[test]
    fn three_run_at_once_and_the_rest_wait() {
        let mut q = Queue::default();
        for name in ["a", "b", "c", "d"] {
            let o = op(&mut q, name, &[name]);
            q.push(o);
        }
        let started = q.start_ready();
        assert_eq!(names(&started), vec!["a", "b", "c"]);
        assert!(q.start_ready().is_empty());
        q.finish(started[0].0.id);
        assert_eq!(names(&q.start_ready()), vec!["d"]);
    }

    #[test]
    fn overlapping_folders_never_run_together_and_keep_their_order() {
        let mut q = Queue::default();
        let a = op(&mut q, "a", &["Shared", "A"]);
        let b = op(&mut q, "b", &["shared"]);
        let c = op(&mut q, "c", &["SHARED", "C"]);
        let d = op(&mut q, "d", &["D"]);
        let a_id = a.id;
        let b_id = b.id;
        q.push(a);
        q.push(b);
        q.push(c);
        q.push(d);
        assert_eq!(names(&q.start_ready()), vec!["a", "d"]);
        q.finish(a_id);
        assert_eq!(names(&q.start_ready()), vec!["b"], "c must not pass b");
        q.finish(b_id);
        assert_eq!(names(&q.start_ready()), vec!["c"]);
    }

    #[test]
    fn dependencies_finish_first() {
        let mut q = Queue::default();
        let dep = op(&mut q, "dep", &["Dep"]);
        let mut main = op(&mut q, "main", &["Main"]);
        main.after = Some(dep.id);
        let dep_id = dep.id;
        q.push(dep);
        q.push(main);
        assert_eq!(names(&q.start_ready()), vec!["dep"]);
        q.finish(dep_id);
        assert_eq!(names(&q.start_ready()), vec!["main"]);
    }

    #[test]
    fn an_exclusive_op_waits_for_quiet_and_holds_the_line() {
        let mut q = Queue::default();
        let a = op(&mut q, "a", &["A"]);
        let mut backup = op(&mut q, "backup", &[]);
        backup.exclusive = true;
        let b = op(&mut q, "b", &["B"]);
        let (a_id, backup_id) = (a.id, backup.id);
        q.push(a);
        q.push(backup);
        q.push(b);
        assert_eq!(names(&q.start_ready()), vec!["a"], "b can't pass the backup");
        q.finish(a_id);
        assert_eq!(names(&q.start_ready()), vec!["backup"]);
        assert!(q.start_ready().is_empty(), "nothing beside it");
        q.finish(backup_id);
        assert_eq!(names(&q.start_ready()), vec!["b"]);
    }

    #[test]
    fn the_bar_counts_a_batch() {
        let mut q = Queue::default();
        assert_eq!(q.casting(), None);
        for name in ["a", "b", "c", "d"] {
            let o = op(&mut q, name, &[name]);
            q.push(o);
        }
        let started = q.start_ready();
        q.set_progress(started[0].0.id, 0.5);
        let casting = q.casting().unwrap();
        assert_eq!(casting.text, "Updating 1 of 4");
        assert!((casting.fraction - 0.125).abs() < 0.001);
        q.finish(started[0].0.id);
        assert_eq!(q.casting().unwrap().text, "Updating 2 of 4");
    }

    #[test]
    fn the_bar_names_a_single_op_and_only_moves_forward() {
        let mut q = Queue::default();
        let o = op(&mut q, "Questie", &["Questie"]);
        let id = o.id;
        q.push(o);
        q.start_ready();
        q.set_progress(id, 0.7);
        q.set_progress(id, 0.2);
        let casting = q.casting().unwrap();
        assert_eq!(casting.text, "Updating Questie");
        assert!((casting.fraction - 0.7).abs() < 0.001);
    }

    #[test]
    fn cancel_flags_what_runs_and_drops_what_waits() {
        let mut q = Queue::default();
        for name in ["a", "b", "c", "d", "e"] {
            let o = op(&mut q, name, &[name]);
            q.push(o);
        }
        let started = q.start_ready();
        assert_eq!(q.cancel_all(), 5);
        assert!(started[0].1.load(Ordering::Relaxed));
        assert!(q.start_ready().is_empty());
        assert_eq!(q.casting().unwrap().text, "Updating 1 of 3");
        for (o, _) in &started {
            q.finish(o.id);
        }
        assert!(q.is_idle());
    }

    #[test]
    fn a_new_batch_starts_counting_afresh() {
        let mut q = Queue::default();
        let o = op(&mut q, "a", &["a"]);
        let id = o.id;
        q.push(o);
        q.start_ready();
        q.finish(id);
        let o = op(&mut q, "b", &["b"]);
        q.push(o);
        assert_eq!(q.casting().unwrap().text, "Updating b");
    }

    #[test]
    fn busy_covers_waiting_and_running() {
        let mut q = Queue::default();
        let a = op(&mut q, "a", &["shared"]);
        let b = op(&mut q, "b", &["shared"]);
        q.push(a);
        q.push(b);
        q.start_ready();
        assert!(q.busy_with("a"));
        assert!(q.busy_with("b"));
        assert!(!q.busy_with("c"));
        assert!(!q.busy_with(""));
    }

    #[test]
    fn stages_map_onto_the_bar_in_order() {
        let downloading = Stage::Downloading {
            done: 50,
            total: Some(100),
        };
        assert!((stage_fraction(downloading, 0) - 0.4).abs() < 0.001);
        let unknown = Stage::Downloading {
            done: 50,
            total: None,
        };
        assert!((stage_fraction(unknown, 100) - 0.4).abs() < 0.001, "catalog size");
        let huge = Stage::Downloading {
            done: 500_000_000,
            total: None,
        };
        assert!(stage_fraction(huge, 0) < 0.8);
        assert!(stage_fraction(Stage::Verifying, 0) <= stage_fraction(Stage::Swapping, 0));
        assert_eq!(stage_fraction(Stage::Done, 0), 1.0);
    }

    fn record(id: &str, version: &str, folders: &[&str]) -> ManagedAddon {
        ManagedAddon {
            id: id.to_owned(),
            name: id.to_owned(),
            version: version.to_owned(),
            folders: folders.iter().map(|f| (*f).to_owned()).collect(),
            ..Default::default()
        }
    }

    fn facts<'a>(version: &'a str, kept: &'a [PathBuf], keep_previous: bool) -> Installed<'a> {
        Installed {
            id: "a",
            name: "A",
            version,
            channel: Channel::Stable,
            origin: Origin::Catalog,
            folders: vec!["A".to_owned()],
            kept,
            keep_previous,
            now: 500,
        }
    }

    #[test]
    fn a_first_install_has_nothing_to_roll_back_to() {
        let kept = [PathBuf::from("zips/1.0.zip")];
        let r = record_after_install(None, facts("1.0", &kept, true));
        assert_eq!(r.previous_version, None);
        assert_eq!(r.installed_at, 500);
    }

    #[test]
    fn an_update_remembers_the_version_it_replaced() {
        let mut prev = record("a", "1.0", &["A"]);
        prev.installed_at = 100;
        prev.pinned = true;
        let kept = [PathBuf::from("zips/2.0.zip"), PathBuf::from("zips/1.0.zip")];
        let r = record_after_install(Some(&prev), facts("2.0", &kept, true));
        assert_eq!(r.previous_version.as_deref(), Some("1.0"));
        assert_eq!(r.previous_zip.as_deref(), Some(Path::new("zips/1.0.zip")));
        assert_eq!(r.installed_at, 100);
        assert_eq!(r.updated_at, 500);
        assert!(r.pinned);
    }

    #[test]
    fn a_rollback_can_be_rolled_back_again() {
        let mut prev = record("a", "2.0", &["A"]);
        prev.previous_version = Some("1.0".to_owned());
        prev.previous_zip = Some(PathBuf::from("zips/1.0.zip"));
        let kept = [PathBuf::from("zips/1.0.zip"), PathBuf::from("zips/2.0.zip")];
        let r = record_after_install(Some(&prev), facts("1.0", &kept, true));
        assert_eq!(r.version, "1.0");
        assert_eq!(r.previous_version.as_deref(), Some("2.0"));
        assert_eq!(r.previous_zip.as_deref(), Some(Path::new("zips/2.0.zip")));
    }

    #[test]
    fn a_reinstall_keeps_what_it_had() {
        let mut prev = record("a", "2.0", &["A"]);
        prev.previous_version = Some("1.0".to_owned());
        prev.previous_zip = Some(PathBuf::from("zips/1.0.zip"));
        let kept = [PathBuf::from("zips/2.0.zip"), PathBuf::from("zips/1.0.zip")];
        let r = record_after_install(Some(&prev), facts("2.0", &kept, true));
        assert_eq!(r.previous_version.as_deref(), Some("1.0"));
    }

    #[test]
    fn an_adopted_addon_has_no_zip_to_go_back_to() {
        let prev = record("a", "1.0", &["A"]);
        let kept = [PathBuf::from("zips/2.0.zip"), PathBuf::from("zips/0.9.zip")];
        let r = record_after_install(Some(&prev), facts("2.0", &kept, true));
        assert_eq!(r.previous_version, None, "that zip isn't this version");
    }

    #[test]
    fn the_setting_off_keeps_nothing() {
        let prev = record("a", "1.0", &["A"]);
        let kept = [PathBuf::from("zips/2.0.zip"), PathBuf::from("zips/1.0.zip")];
        let r = record_after_install(Some(&prev), facts("2.0", &kept, false));
        assert_eq!(r.previous_zip, None);
    }

    #[test]
    fn versions_with_odd_characters_still_find_their_zip() {
        let prev = record("a", "bundle/v12+1", &["A"]);
        let kept = [
            PathBuf::from("zips/2.0.zip"),
            PathBuf::from("zips/bundle_v12+1.zip"),
        ];
        let r = record_after_install(Some(&prev), facts("2.0", &kept, true));
        assert!(r.previous_zip.is_some());
    }

    #[test]
    fn shared_folders_stay_with_their_other_owner() {
        let managed: BTreeMap<String, ManagedAddon> = [
            ("a".to_owned(), record("a", "1", &["A", "Lib"])),
            ("b".to_owned(), record("b", "1", &["B", "lib"])),
        ]
        .into();
        assert_eq!(own_folders("a", &managed), vec!["A".to_owned()]);
        assert!(own_folders("missing", &managed).is_empty());
    }

    #[test]
    fn multi_folder_zips_are_named_after_their_core() {
        let name = |list: &[&str]| {
            let list: Vec<String> = list.iter().map(|s| (*s).to_owned()).collect();
            main_folder(&list).unwrap().clone()
        };
        assert_eq!(name(&["DBM-GUI", "DBM-Core", "DBM-StatusBarTimers"]), "DBM-Core");
        assert_eq!(name(&["BigWigs", "BigWigs_Options"]), "BigWigs");
        assert_eq!(name(&["Solo"]), "Solo");
    }

    #[test]
    fn a_dropped_zip_replaces_the_catalog_record_it_overwrote() {
        let managed: BTreeMap<String, ManagedAddon> = [
            ("gh:q".to_owned(), record("gh:q", "1", &["Questie"])),
            ("gh:lib".to_owned(), record("gh:lib", "1", &["Lib"])),
        ]
        .into();
        assert_eq!(
            stale_records(&managed, "local:Questie", Origin::LocalZip, &["Questie".to_owned()]),
            vec!["gh:q".to_owned()]
        );
        assert!(
            stale_records(&managed, "gh:x", Origin::Catalog, &["Lib".to_owned()]).is_empty(),
            "catalog addons may share folders"
        );
    }

    #[test]
    fn only_zip_files_count_as_zips() {
        assert!(is_zip(Path::new("a/B.ZIP")));
        assert!(!is_zip(Path::new("a/b.txt")));
        assert!(!is_zip(Path::new("a/zip")));
    }
}
