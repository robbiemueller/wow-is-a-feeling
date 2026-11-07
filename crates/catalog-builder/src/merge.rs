//! Combines the sources into one list and settles who owns what. Anyone with a free
//! GitHub account gets looked at automatically, and everything in their repository is
//! theirs to write: its title, folders, TOC ids and download counts prove nothing. So
//! identity is first come, oldest wins, and whatever a person picked by hand beats all
//! of it. See the Catalog section of `docs/DESIGN.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use feeling_core::catalog::{Addon, Catalog, Source};
use feeling_core::deps::BIGGEST_UNASKED_DOWNLOAD;

use crate::curated::{Curated, HandPicked, is_line_id};
use crate::detect::FOREVER_EPOCH;
use crate::gate::is_safe_folder;
use crate::github::RepoInfo;
use crate::model::{Candidate, Held, MAX_FORMERLY};
use crate::state::{LastPublished, Remembered};
use crate::text::{FolderMap, fold_folder, fold_name, normalize, same_folder, tokens};
use crate::tier::popularity;

/// The age of everything that was already listed when ages started being kept. Older
/// than the game itself, so those entries are established from the first build on.
pub const BASELINE_CREATED: i64 = FOREVER_EPOCH;
/// How long an entry must keep its name and main folder before the app may pick it by itself.
const TRUST_AFTER_SECS: i64 = 14 * 24 * 60 * 60;
/// How long an entry gone from the catalog keeps its age and its claims, counted from the last
/// catalog it was published in. Long enough for a held or left-out entry to come back.
const AWAY_CLAIM_SECS: i64 = 30 * 24 * 60 * 60;
/// Entries an owner can have without being hand-picked. The most any real owner has today
/// is 21 listed out of 38 known. This is what makes a flood of repositories cost something.
pub const MAX_ENTRIES_PER_OWNER: usize = 40;

/// Why a repository isn't worth looking at, or `None` if it is.
pub fn repo_rejection(info: &RepoInfo, hand_picked: bool) -> Option<&'static str> {
    if info.disabled {
        return Some("disabled");
    }
    if info.empty {
        return Some("empty");
    }
    if info.archived && !hand_picked {
        return Some("archived");
    }
    if hand_picked {
        return None;
    }
    if info.template {
        return Some("template");
    }
    if info.mirror {
        return Some("mirror");
    }
    let repo_name = info.name.split('/').nth(1).unwrap_or(&info.name);
    let words = tokens(repo_name);
    if words.iter().any(|w| {
        matches!(
            w.as_str(),
            "mirror" | "template" | "boilerplate" | "skeleton" | "scaffold"
        )
    }) {
        return Some("template or mirror by name");
    }
    None
}

/// Placeholder titles that mean nobody finished setting up the addon.
pub fn is_placeholder_name(name: &str) -> bool {
    matches!(
        normalize(name).as_str(),
        "" | "myaddon"
            | "addon"
            | "addonname"
            | "newaddon"
            | "youraddon"
            | "addontemplate"
            | "template"
            | "test"
            | "testaddon"
            | "example"
            | "exampleaddon"
            | "sample"
            | "sampleaddon"
            | "helloworld"
            | "untitled"
            | "foo"
            | "bar"
    )
}

/// Forks exist for many reasons, but one that installs the same folders as the repo it
/// came from adds nothing but confusion, and its downloads go to the wrong place. The
/// same goes for a lesser-known sibling. Any other repo with the same folders proves
/// nothing, because anyone can make one, and something picked by hand always stays.
pub fn drop_redundant_forks(
    candidates: Vec<Candidate>,
    picks: &HandPicked,
) -> (Vec<Candidate>, Vec<String>) {
    let sets: BTreeMap<String, BTreeSet<String>> = candidates
        .iter()
        .map(|c| (c.addon.id.to_ascii_lowercase(), c.folder_set()))
        .collect();
    let forks: Vec<(String, String, BTreeSet<String>, u32)> = candidates
        .iter()
        .filter_map(|c| {
            let parent = c.fork_of.as_ref().filter(|parent| !parent.is_empty())?;
            Some((
                c.addon.id.clone(),
                parent.to_ascii_lowercase(),
                c.folder_set(),
                c.addon.stars,
            ))
        })
        .collect();

    let mut kept = Vec::with_capacity(candidates.len());
    let mut dropped = Vec::new();
    for candidate in candidates {
        let redundant = !picks.contains(&candidate.addon.id)
            && candidate.fork_of.as_ref().is_some_and(|parent| {
                let set = candidate.folder_set();
                let parent = parent.to_ascii_lowercase();
                let same_as_parent = sets
                    .get(&format!("gh:{parent}"))
                    .is_some_and(|parent_set| *parent_set == set);
                let same_as_better_sibling =
                    forks.iter().any(|(id, other_parent, other, stars)| {
                        *id != candidate.addon.id
                            && *other_parent == parent
                            && *other == set
                            && *stars > candidate.addon.stars
                    });
                same_as_parent || same_as_better_sibling
            });
        if redundant {
            dropped.push(candidate.addon.id);
        } else {
            kept.push(candidate);
        }
    }
    (kept, dropped)
}

/// Fold WoWInterface entries into the GitHub repositories they're approved for in
/// `catalog/curated.toml`. GitHub wins because its builds are versioned and carry digests,
/// and the WoWI id is kept so an addon installed from there can still be matched. Nothing
/// else merges: a repository can claim any id and copy any folder, so a TOC id or
/// matching folders never remove a listing or move its counts.
pub fn merge_wowi(
    mut github: Vec<Candidate>,
    mut wowi: Vec<Candidate>,
    approved: &BTreeMap<String, String>,
) -> (Vec<Candidate>, usize) {
    // In id order, so two listings approved for one repository settle the same way every run.
    wowi.sort_by_key(|entry| listing_number(entry.addon.ids.wowi.as_deref()));
    let positions: HashMap<String, usize> = github
        .iter()
        .enumerate()
        .map(|(at, g)| (g.addon.id.to_ascii_lowercase(), at))
        .collect();
    let mut merged_count = 0;
    let mut standalone = Vec::new();
    for entry in wowi {
        let id = entry.addon.ids.wowi.clone().unwrap_or_default();
        let repo = approved.get(&id);
        let target = repo.and_then(|repo| {
            positions
                .get(&format!("gh:{}", repo.to_ascii_lowercase()))
                .copied()
        });
        match (target, repo) {
            (Some(at), Some(repo)) => {
                let target = &mut github[at];
                // The id the repository's own TOC gives stays when that pair is approved too.
                let own_id_is_approved = target
                    .addon
                    .ids
                    .wowi
                    .as_ref()
                    .and_then(|own| approved.get(own))
                    .is_some_and(|other| other.eq_ignore_ascii_case(repo));
                if !own_id_is_approved {
                    target.addon.ids.wowi = Some(id);
                }
                target.addon.downloads += entry.addon.downloads;
                target.favorites += entry.favorites;
                if target.addon.summary.is_empty() {
                    target.addon.summary = entry.addon.summary;
                }
                if target.wowi_category.is_none() {
                    target.wowi_category = entry.wowi_category;
                }
                merged_count += 1;
            }
            _ => standalone.push(entry),
        }
    }
    github.append(&mut standalone);
    (github, merged_count)
}

fn listing_number(id: Option<&str>) -> u64 {
    id.and_then(|id| id.parse().ok()).unwrap_or(0)
}

/// Whether an entry still is what the last catalog listed: same name to a reader's eye,
/// same main folder.
fn same_identity(old_name: &str, old_folders: &[String], candidate: &Candidate) -> bool {
    let main = |folders: &[String]| folders.first().map(|folder| fold_folder(folder));
    fold_name(old_name) == fold_name(&candidate.addon.name)
        && main(old_folders) == main(&candidate.addon.folders)
}

/// Whether the scan cache remembers this entry being published in the last 30 days. One noted
/// before the time was kept counts as recent.
fn is_recent(last: &LastPublished, now: i64) -> bool {
    last.at == 0 || now - last.at <= AWAY_CLAIM_SECS
}

/// Set when each entry first appeared, from the last published catalog, under any id the entry
/// goes by. An entry that catalog left out keeps the age the scan cache remembers from the last
/// catalog it was in, for 30 days after that, so a build it was held or left out of doesn't make
/// it new. An entry neither knows is new. So is one that changed its name or main folder, unless
/// someone picked it by hand, so a repository can't get established as one thing and then turn
/// into another.
pub fn stamp_ages(
    candidates: &mut [Candidate],
    previous: Option<&Catalog>,
    remembered: &Remembered,
    picks: &HandPicked,
    now: i64,
) {
    let listed: HashMap<String, (&str, &[String], i64)> = previous
        .map(|catalog| catalog.addons.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|addon| {
            (
                addon.id.to_ascii_lowercase(),
                (addon.name.as_str(), addon.folders.as_slice(), addon.created),
            )
        })
        .collect();
    for candidate in candidates {
        let still = |name: &str, folders: &[String]| {
            picks.contains(&candidate.addon.id) || same_identity(name, folders, candidate)
        };
        let known = candidate
            .known_as()
            .find_map(|id| listed.get(&id.to_ascii_lowercase()));
        let created = match known {
            Some((name, folders, created)) => still(name, folders).then_some(*created),
            None => candidate
                .known_as()
                .find_map(|id| remembered.get(id))
                .filter(|last| is_recent(last, now) && !last.name.is_empty())
                .filter(|last| still(&last.name, std::slice::from_ref(&last.main)))
                .map(|last| last.created),
        };
        candidate.addon.created = match created {
            Some(0) => BASELINE_CREATED,
            Some(created) => created,
            None => now,
        };
    }
}

/// Names and folders only something hand-picked may claim: well-known addons that have no
/// entry yet, whose names a stranger could otherwise take for free.
#[derive(Debug, Default)]
pub struct Reserved {
    names: HashMap<String, String>,
    folders: HashMap<String, String>,
}

impl Reserved {
    pub fn new(curated: &Curated) -> Self {
        let mut reserved = Self::default();
        for gap in &curated.gaps {
            reserved
                .names
                .insert(fold_name(&gap.name), gap.name.clone());
        }
        let folders = curated
            .gaps
            .iter()
            .flat_map(|gap| gap.folders.iter())
            .chain(curated.reserved.iter());
        for folder in folders {
            // A title that reads like the folder passes for the addon just as well.
            reserved
                .names
                .entry(fold_name(folder))
                .or_insert_with(|| folder.clone());
            reserved.folders.insert(fold_folder(folder), folder.clone());
        }
        reserved
    }

    fn problem(&self, candidate: &Candidate) -> Option<String> {
        if let Some(name) = self.names.get(&fold_name(&candidate.addon.name)) {
            return Some(format!(
                "its name is reserved for the real {name}, which needs picking by hand"
            ));
        }
        let folder = candidate
            .addon
            .folders
            .iter()
            .find_map(|folder| self.folders.get(&fold_folder(folder)));
        folder.map(|folder| {
            format!(
                "its folder {folder} is reserved for the real addon, which needs picking by hand"
            )
        })
    }
}

/// What the collision pass goes by.
pub struct ClaimRules<'a> {
    pub picks: &'a HandPicked,
    pub reserved: &'a Reserved,
    pub previous: Option<&'a Catalog>,
    /// What the scan cache remembers of entries the last catalog left out.
    pub remembered: &'a Remembered,
    /// For `[pins]` and the names `[overrides]` gives.
    pub curated: &'a Curated,
    /// The build's time, which says how long an entry has been away.
    pub now: i64,
    /// Ids, in lower case, that the build dropped on purpose before the pass: forks with the same
    /// folders as their parent or a better-known sibling. Those claim nothing, like an entry a
    /// maintainer excluded.
    pub left_out: &'a HashSet<String>,
}

fn is_baseline(candidate: &Candidate) -> bool {
    candidate.addon.created == BASELINE_CREATED
}

fn is_repo(candidate: &Candidate) -> bool {
    candidate.addon.id.starts_with("gh:")
}

fn is_listing(candidate: &Candidate) -> bool {
    matches!(candidate.addon.source, Source::Wowi { .. })
}

/// The owner part of a GitHub id, in lower case.
fn owner_of(candidate: &Candidate) -> Option<String> {
    owner_of_id(&candidate.addon.id)
}

fn owner_of_id(id: &str) -> Option<String> {
    let repo = id.strip_prefix("gh:")?;
    Some(repo.split('/').next()?.to_ascii_lowercase())
}

/// Who has claimed what so far. Candidates go through it best first.
#[derive(Default)]
struct Ledger<'c> {
    kept: Vec<&'c Candidate>,
    names: HashMap<String, Vec<usize>>,
    folders: HashMap<String, Vec<usize>>,
    per_owner: HashMap<String, usize>,
}

impl<'c> Ledger<'c> {
    /// Claims what `candidate` has, and `also_named` too, a name that would pass for it.
    fn keep(&mut self, candidate: &'c Candidate, picks: &HandPicked, also_named: Option<&str>) {
        let at = self.kept.len();
        let mut names: Vec<String> = std::iter::once(candidate.addon.name.as_str())
            .chain(also_named)
            .map(fold_name)
            .filter(|name| !name.is_empty())
            .collect();
        names.dedup();
        for name in names {
            self.names.entry(name).or_default().push(at);
        }
        for folder in &candidate.addon.folders {
            self.folders
                .entry(fold_folder(folder))
                .or_default()
                .push(at);
        }
        if let Some(owner) = owner_of(candidate)
            && !picks.contains(&candidate.addon.id)
        {
            *self.per_owner.entry(owner).or_default() += 1;
        }
        self.kept.push(candidate);
    }

    /// Why this candidate can't have what it claims, given everything ahead of it.
    fn conflict(&self, candidate: &Candidate, picks: &HandPicked) -> Option<String> {
        let picked = picks.contains(&candidate.addon.id);
        // Entries that were already side by side stay that way, and so do two a person chose.
        let tied = |other: &Candidate| {
            (is_baseline(other) && is_baseline(candidate))
                || (picked && picks.contains(&other.addon.id))
        };
        let main = |c: &Candidate| c.addon.folders.first().map(|folder| fold_folder(folder));

        let name = fold_name(&candidate.addon.name);
        for other in self.at(self.names.get(&name)) {
            if !tied(other) {
                return Some(format!("its name is already taken by {}", other.addon.id));
            }
            // Only repositories compete as exact copies, as they always have.
            if is_repo(other) && is_repo(candidate) && main(other) == main(candidate) {
                return Some(format!(
                    "it's a copy of {}, with the same name and main folder",
                    other.addon.id
                ));
            }
        }
        let mut folders = candidate.addon.folders.iter();
        if let Some(main_folder) = folders.next() {
            let claimed = self.at(self.folders.get(&fold_folder(main_folder)));
            if let Some(other) = claimed.into_iter().find(|other| !tied(other)) {
                return Some(format!(
                    "its main folder {main_folder} is already taken by {}",
                    other.addon.id
                ));
            }
        }
        for folder in folders {
            let claimed = self.at(self.folders.get(&fold_folder(folder)));
            let ahead = claimed.into_iter().find(|other| {
                !tied(other)
                    && ((picks.contains(&other.addon.id) && !picked)
                        || other.addon.created < candidate.addon.created)
            });
            if let Some(other) = ahead {
                return Some(format!(
                    "its folder {folder} is already taken by {}",
                    other.addon.id
                ));
            }
        }
        if !picked
            && let Some(owner) = owner_of(candidate)
            && self.per_owner.get(&owner).copied().unwrap_or(0) >= MAX_ENTRIES_PER_OWNER
        {
            return Some(format!(
                "{owner} already has {MAX_ENTRIES_PER_OWNER} addons listed"
            ));
        }
        None
    }

    fn at(&self, places: Option<&Vec<usize>>) -> Vec<&'c Candidate> {
        places
            .into_iter()
            .flatten()
            .map(|at| self.kept[*at])
            .collect()
    }
}

/// Which addon a GitHub entry is: the main folder a new build has to ship to count, so a
/// release of another addon from the same repository can never take the entry's place.
/// See "One product per entry" in `docs/DESIGN.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// Compared ignoring case.
    pub main: String,
    /// A person wrote it in `[pins]`, rather than it coming from a last result.
    pub written: bool,
}

/// Where each GitHub entry's pin comes from: the `main` of `[pins]` in `curated.toml`, or else
/// the main folder the last catalog listed it with, or for an entry that catalog left out, the
/// one the scan cache remembers it was last published with. With no last catalog to read, the
/// scan cache's last result says. An entry none of them knows has nothing to stay, so it takes
/// its newest build.
#[derive(Debug, Default)]
pub struct Pins<'a> {
    /// `[pins]` that name a main folder, by lowercased id.
    written: HashMap<String, String>,
    /// The last catalog's GitHub entries' main folders, by lowercased id, or `None` without a
    /// last catalog to read.
    listed: Option<HashMap<String, &'a str>>,
    /// Every entry's main folder when it was last published, by lowercased id.
    remembered: HashMap<String, String>,
}

impl<'a> Pins<'a> {
    pub fn new(previous: Option<&'a Catalog>, curated: &Curated) -> Self {
        let written = curated
            .pins
            .iter()
            .filter_map(|(id, pin)| Some((id.to_ascii_lowercase(), pin.main.clone()?)))
            .collect();
        let listed = previous.map(|catalog| {
            catalog
                .addons
                .iter()
                .filter(|addon| addon.id.starts_with("gh:"))
                .filter_map(|addon| {
                    let main = addon.folders.first()?;
                    Some((addon.id.to_ascii_lowercase(), main.as_str()))
                })
                .collect()
        });
        Self {
            written,
            listed,
            remembered: HashMap::new(),
        }
    }

    /// The same, knowing what the scan cache remembers each entry was last published as, so an
    /// entry one catalog left out is still pinned in the next build.
    pub fn remembering(mut self, remembered: &Remembered) -> Self {
        self.remembered = remembered
            .mains()
            .map(|(id, main)| (id.to_string(), main.to_string()))
            .collect();
        self
    }

    /// The pin of the entry that goes by any of `ids`: its own, and after GitHub renames a
    /// repository, the one the last catalog and `curated.toml` still use, which the scan cache's
    /// last result keeps. `cached` is that last result.
    pub fn get(&self, ids: &[&str], cached: Option<&Candidate>) -> Option<Pin> {
        let ids: Vec<String> = ids.iter().map(|id| id.to_ascii_lowercase()).collect();
        if let Some(main) = ids.iter().find_map(|id| self.written.get(id)) {
            return Some(Pin {
                main: main.clone(),
                written: true,
            });
        }
        let listed = self
            .listed
            .as_ref()
            .and_then(|listed| ids.iter().find_map(|id| listed.get(id)))
            .map(|main| (*main).to_string());
        let main = listed
            .or_else(|| ids.iter().find_map(|id| self.remembered.get(id)).cloned())
            .or_else(|| {
                let unread = self.listed.is_none();
                let cached = cached.and_then(|entry| entry.addon.folders.first());
                cached.filter(|_| unread).cloned()
            });
        main.map(|main| Pin {
            main,
            written: false,
        })
    }
}

/// What decides the former main folders the catalog vouches for: what each entry was when it
/// was last published, and `[pins]`. See "What it used to be" in `docs/DESIGN.md`.
struct Vouches<'a> {
    /// The last catalog's entries, by lowercased id.
    listed: HashMap<String, &'a Addon>,
    /// What the scan cache remembers of entries the last catalog left out.
    remembered: &'a Remembered,
    curated: &'a Curated,
}

impl<'a> Vouches<'a> {
    fn new(
        previous: Option<&'a Catalog>,
        remembered: &'a Remembered,
        curated: &'a Curated,
    ) -> Self {
        let listed = previous
            .map_or(&[][..], |catalog| catalog.addons.as_slice())
            .iter()
            .map(|addon| (addon.id.to_ascii_lowercase(), addon))
            .collect();
        Self {
            listed,
            remembered,
            curated,
        }
    }

    /// The main folder an entry was last published with, and the former ones vouched for then:
    /// from the last catalog, or for an entry it left out, from the scan cache's memory. After
    /// GitHub renames a repository, what it was published as under its old id is its own.
    fn last(&self, candidate: &Candidate) -> Option<(&'a str, &'a [String])> {
        candidate.known_as().find_map(|id| self.last_of(id))
    }

    fn last_of(&self, id: &str) -> Option<(&'a str, &'a [String])> {
        match self.listed.get(&id.to_ascii_lowercase()) {
            Some(addon) => {
                let main = addon.folders.first().map_or("", String::as_str);
                Some((main, addon.formerly.as_slice()))
            }
            None => {
                let last = self.remembered.get(id)?;
                Some((last.main.as_str(), last.formerly.as_slice()))
            }
        }
    }

    /// Whether the last catalog lists the entry, or the scan cache remembers it being published
    /// in the last 30 days, under any id it goes by.
    fn published_lately(&self, candidate: &Candidate, now: i64) -> bool {
        candidate.known_as().any(|id| {
            self.listed.contains_key(&id.to_ascii_lowercase())
                || self
                    .remembered
                    .get(id)
                    .is_some_and(|last| is_recent(last, now))
        })
    }

    /// The former main folders vouched for this entry in this build, before dropping any that an
    /// entry with a better claim ships, and the folders `[pins]` says it was that don't fit.
    fn of(&self, candidate: &Candidate) -> (Vec<String>, Vec<String>) {
        former_mains(
            self.last(candidate),
            self.was_of(candidate),
            &candidate.addon,
        )
    }

    /// The folders `[pins]` says this entry was, under its own id or one it had before GitHub
    /// renamed it.
    fn was_of(&self, candidate: &Candidate) -> &'a [String] {
        let pin = candidate.known_as().find_map(|id| self.curated.pin_of(id));
        pin.map_or(&[][..], |pin| &pin.was[..])
    }

    fn was(&self, id: &str) -> &'a [String] {
        self.curated.pin_of(id).map_or(&[][..], |pin| &pin.was[..])
    }

    /// Entries this build doesn't have, whose former main folders are still their claims: each
    /// one the last catalog listed, and each one the scan cache remembers being published in the
    /// last 30 days, but none a maintainer excluded. Each comes with the folders it would be
    /// vouched for if it came back as it was, and its age. `accounted` holds, in lower case, the
    /// ids the build has: every candidate's, the ones they had before GitHub renamed them, and
    /// the ones it left out on purpose.
    fn away(&self, accounted: &HashSet<String>, now: i64) -> Vec<Away> {
        let mut away = Vec::new();
        for (id, addon) in &self.listed {
            if accounted.contains(id) || self.curated.excludes_id(id) {
                continue;
            }
            let main = addon.folders.first().map_or("", String::as_str);
            away.push(Away {
                id: addon.id.clone(),
                created: addon.created,
                folders: vouched_list(main, None, self.was(id), &addon.formerly).0,
            });
        }
        // In id order, so a folder two of them are vouched for names the same one every run.
        away.sort_by(|a, b| a.id.cmp(&b.id));
        for (key, last) in self.remembered.entries() {
            if accounted.contains(key)
                || self.listed.contains_key(key)
                || !is_recent(last, now)
                || self.curated.excludes_id(key)
            {
                continue;
            }
            away.push(Away {
                id: last.id_or(key).to_string(),
                created: last.created,
                folders: vouched_list(&last.main, None, self.was(key), &last.formerly).0,
            });
        }
        away
    }

    /// A summary line for a GitHub entry whose main folder moved since it was last published,
    /// with nothing vouching for the old one: a pinned build keeps its main folder first, so
    /// this is an entry with no pin, such as a `[[split]]` repository's own. Only a person can
    /// tell whether the old folder was this addon or another of the repository's.
    fn moved(&self, candidate: &Candidate) -> Option<String> {
        let addon = &candidate.addon;
        let pinned = candidate
            .known_as()
            .any(|id| self.curated.main_pin_of(id).is_some());
        if holds_one_addon(addon) || pinned {
            return None;
        }
        let main = addon.folders.first()?;
        let (old, _) = self.last(candidate)?;
        let unchanged = old.is_empty() || old.eq_ignore_ascii_case(main);
        if unchanged || addon.formerly.iter().any(|f| f.eq_ignore_ascii_case(old)) {
            return None;
        }
        let id = &addon.id;
        Some(format!(
            "{id} lists {main} first now, not {old}. If they're the same addon, write was = [\"{old}\"] under [pins.\"{id}\"] in curated.toml"
        ))
    }
}

/// A source that only ever holds one addon, so a new main folder there is a rename: a
/// WoWInterface listing, a direct entry, or a release line, whose tag prefix decides its addon.
/// A GitHub repository's own entry can be any of the addons the repository publishes.
fn holds_one_addon(addon: &Addon) -> bool {
    !matches!(addon.source, Source::Github { .. }) || is_line_id(&addon.id)
}

/// One entry's former main folders, the most recent first: for a source that holds one addon,
/// its last main folder when this build has another one first, then what it was vouched for when
/// it was last published, then the folders `[pins]` says it was. A GitHub repository can hold
/// several addons, so a move there is never vouched for by itself. The current main folder is
/// never a former one. Also returns the folders `[pins]` names that don't fit.
fn former_mains(
    last: Option<(&str, &[String])>,
    was: &[String],
    addon: &Addon,
) -> (Vec<String>, Vec<String>) {
    let Some(main) = addon.folders.first() else {
        return (Vec::new(), Vec::new());
    };
    let (last_main, carried) = last.unwrap_or(("", &[]));
    let moved_from =
        (holds_one_addon(addon) && !last_main.is_empty() && !last_main.eq_ignore_ascii_case(main))
            .then_some(last_main);
    vouched_list(main, moved_from, was, carried)
}

/// The former main folders of an entry whose main folder is `main`, the most recent first: a
/// folder it just moved away from, then the ones `carried` over, then the ones `[pins]` says it
/// `was` that aren't carried yet. Only the most recent [`MAX_FORMERLY`] stay, so what's already
/// vouched for keeps its place from build to build, and a folder written down can't push out the
/// one an entry was just renamed from. Returns those, and the written folders that don't fit.
fn vouched_list(
    main: &str,
    moved_from: Option<&str>,
    was: &[String],
    carried: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut seen = HashSet::new();
    let folders = moved_from
        .into_iter()
        .map(|folder| (folder, false))
        .chain(carried.iter().map(|folder| (folder.as_str(), false)))
        .chain(was.iter().map(|folder| (folder.as_str(), true)))
        .filter(|(folder, _)| is_safe_folder(folder) && !folder.eq_ignore_ascii_case(main))
        .filter(|(folder, _)| seen.insert(folder.to_ascii_lowercase()));
    let mut kept = Vec::new();
    let mut cut = Vec::new();
    for (folder, written) in folders {
        if kept.len() < MAX_FORMERLY {
            kept.push(folder.to_string());
        } else if written {
            cut.push(folder.to_string());
        }
    }
    (kept, cut)
}

/// An entry this build doesn't have, with the former main folders it's still vouched for.
struct Away {
    id: String,
    /// Its age in the catalog it was last published in.
    created: i64,
    folders: Vec<String>,
}

/// Where an entry stands in a claim to a folder, the way the collision pass ranks entries:
/// hand-picked first, then oldest. Two entries listed before ages were kept are level, and so
/// are two a person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rank {
    picked: bool,
    created: i64,
}

impl Rank {
    fn new(id: &str, created: i64, picks: &HandPicked) -> Self {
        Self {
            picked: picks.contains(id),
            // A catalog from before ages were kept says 0, which a build reads as the baseline.
            created: if created == 0 {
                BASELINE_CREATED
            } else {
                created
            },
        }
    }

    fn of(candidate: &Candidate, picks: &HandPicked) -> Self {
        Self::new(&candidate.addon.id, candidate.addon.created, picks)
    }

    fn level_with(self, other: Self) -> bool {
        (self.created == BASELINE_CREATED && other.created == BASELINE_CREATED)
            || (self.picked && other.picked)
    }

    fn ahead_of(self, other: Self) -> bool {
        !self.level_with(other)
            && match (self.picked, other.picked) {
                (true, false) => true,
                (false, true) => false,
                _ => self.created < other.created,
            }
    }
}

/// A former main folder vouched for an entry, which makes it that entry's claim.
#[derive(Debug, Clone)]
struct Claim {
    id: String,
    rank: Rank,
}

/// Whether an entry shipping a folder that's vouched for another has the better claim to it, so
/// it keeps the folder and the vouch goes. It must rank ahead of the entry vouched for, or level
/// with it, and unless it's hand-picked, the last catalog must already have listed it with that
/// folder: a folder an entry has only now added is a claim from today, however old the entry is,
/// as in the collision pass. While the entry vouched for had the folder as its main one, anything
/// that ranked behind it and shipped the folder was held, so one listed with it since got it
/// while that entry was away from a build, or after its rename.
fn keeps_vouched_folder(
    shipper: &Candidate,
    rank: Rank,
    folder: &str,
    vouched: Rank,
    last: &LastCatalog<'_>,
) -> bool {
    (rank.picked || last.listed_with(shipper, folder))
        && (rank.ahead_of(vouched) || rank.level_with(vouched))
}

/// What stamping `formerly` found, one summary line each.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stamped {
    /// Former main folders the catalog vouches for from this build on.
    pub vouched: Vec<String>,
    /// Ones it stops vouching for, because an entry with a better claim ships them.
    pub unvouched: Vec<String>,
    /// GitHub main folders that moved with nothing vouching for the old one.
    pub moved: Vec<String>,
    /// Folders `[pins]` says an entry was that don't fit in its `formerly`.
    pub cut: Vec<String>,
}

/// Fills in each entry's `formerly`, from what it was last published as and `[pins]`, and
/// returns the summary's lines about it. See "What it used to be" in `docs/DESIGN.md`.
pub fn stamp_formerly(
    candidates: &mut [Candidate],
    previous: Option<&Catalog>,
    remembered: &Remembered,
    curated: &Curated,
    picks: &HandPicked,
) -> Stamped {
    let vouches = Vouches::new(previous, remembered, curated);
    let mut cut = Vec::new();
    for candidate in candidates.iter_mut() {
        let (formerly, left_over) = vouches.of(candidate);
        candidate.addon.formerly = formerly;
        for folder in left_over {
            cut.push(format!(
                "{} was {folder}, as [pins] says, but it isn't vouched for: formerly keeps the {MAX_FORMERLY} most recent",
                candidate.addon.id
            ));
        }
    }
    // Only after every entry has its folders.
    let mut stamped = Stamped {
        unvouched: drop_formerly_others_ship(candidates, picks, &LastCatalog::new(previous)),
        cut,
        ..Stamped::default()
    };
    for candidate in candidates.iter() {
        let addon = &candidate.addon;
        let carried = vouches.last(candidate).map_or(&[][..], |(_, was)| was);
        for folder in &addon.formerly {
            if !carried.iter().any(|old| old.eq_ignore_ascii_case(folder)) {
                stamped.vouched.push(format!("{} was {folder}", addon.id));
            }
        }
        stamped.moved.extend(vouches.moved(candidate));
    }
    stamped
}

/// A former main folder vouches that an installed copy with it is this addon, and the collision
/// pass held everything else that ships one unless it has the better claim (see
/// [`keeps_vouched_folder`]). What still ships one has it, so the folder is that entry's: the
/// vouch goes, and the line says which entry ships it. Folders compare as [`same_folder`] does.
fn drop_formerly_others_ship(
    candidates: &mut [Candidate],
    picks: &HandPicked,
    last: &LastCatalog<'_>,
) -> Vec<String> {
    // Each folder with the entry that ships it and where it is in that entry's list.
    let mut shipped_by: FolderMap<(usize, usize)> = FolderMap::default();
    for (at, candidate) in candidates.iter().enumerate() {
        for (place, folder) in candidate.addon.folders.iter().enumerate() {
            shipped_by.add(folder, (at, place));
        }
    }
    let ranks: Vec<Rank> = candidates.iter().map(|c| Rank::of(c, picks)).collect();
    // Decided before anything changes, so the order of the list can't matter.
    let mut goes: Vec<(usize, String, usize)> = Vec::new();
    for (at, candidate) in candidates.iter().enumerate() {
        let id = &candidate.addon.id;
        for folder in &candidate.addon.formerly {
            // Judged by the shipper's own folder, as the collision pass judged it.
            let better = shipped_by.alike(folder).copied().find(|&(owner, place)| {
                let shipper = &candidates[owner];
                let shipped = &shipper.addon.folders[place];
                !shipper.addon.id.eq_ignore_ascii_case(id)
                    && keeps_vouched_folder(shipper, ranks[owner], shipped, ranks[at], last)
            });
            if let Some((owner, _)) = better {
                goes.push((at, folder.clone(), owner));
            }
        }
    }
    let mut dropped = Vec::with_capacity(goes.len());
    for (at, folder, owner) in goes {
        dropped.push(format!(
            "{} no longer vouches for {folder}, which {} ships",
            candidates[at].addon.id, candidates[owner].addon.id
        ));
        candidates[at].addon.formerly.retain(|kept| *kept != folder);
    }
    dropped
}

/// Which folders the last catalog listed, and for whom.
#[derive(Default)]
struct LastCatalog<'a> {
    folders_of: HashMap<String, HashSet<String>>,
    owners_of: HashMap<String, Vec<&'a str>>,
    /// Each entry's folders as it was listed, by lowercased id.
    listed: HashMap<String, &'a [String]>,
    /// Folders its addons require, folded.
    required: HashSet<String>,
}

impl<'a> LastCatalog<'a> {
    fn new(previous: Option<&'a Catalog>) -> Self {
        let mut last = Self::default();
        for addon in previous.map_or(&[][..], |catalog| catalog.addons.as_slice()) {
            let folders: HashSet<String> = addon.folders.iter().map(|f| fold_folder(f)).collect();
            for folder in &folders {
                last.owners_of
                    .entry(folder.clone())
                    .or_default()
                    .push(&addon.id);
            }
            let id = addon.id.to_ascii_lowercase();
            last.listed.insert(id.clone(), addon.folders.as_slice());
            last.folders_of.insert(id, folders);
            last.required
                .extend(addon.deps.iter().map(|dep| fold_folder(dep)));
        }
        last
    }

    /// The folders, folded, the last catalog listed this entry with, under any id it goes by.
    fn folders_of_entry(&self, candidate: &Candidate) -> Option<&HashSet<String>> {
        candidate
            .known_as()
            .find_map(|id| self.folders_of.get(&id.to_ascii_lowercase()))
    }

    /// Whether the last catalog listed this entry with this folder.
    fn had_folder(&self, candidate: &Candidate, folded: &str) -> bool {
        self.folders_of_entry(candidate)
            .is_some_and(|folders| folders.contains(folded))
    }

    /// Whether the last catalog listed this entry with a folder that passes for this one, as
    /// [`same_folder`] has it.
    fn listed_with(&self, candidate: &Candidate, folder: &str) -> bool {
        candidate
            .known_as()
            .find_map(|id| self.listed.get(&id.to_ascii_lowercase()))
            .is_some_and(|folders| folders.iter().any(|f| same_folder(f, folder)))
    }

    /// Folders the last catalog's addons require that nothing still provides: no entry
    /// listed with the folder then still ships it now. Worked out once, before the pass, so
    /// neither the order of the pass nor a newcomer's own requirements can change it.
    fn open_dependencies(&self, candidates: &[Candidate]) -> HashSet<String> {
        let mut open = self.required.clone();
        for candidate in candidates {
            for folder in &candidate.addon.folders {
                let folded = fold_folder(folder);
                if self.had_folder(candidate, &folded) {
                    open.remove(&folded);
                }
            }
        }
        open
    }

    /// For an entry that was already listed: a folder it has only now added, which another
    /// listed entry already had. That claim dates from today, however old the entry is, or
    /// a long-listed repository could push out anything younger by shipping its folder. An
    /// entry the last catalog left out goes by the folders the scan cache remembers it being
    /// published with, for the 30 days it keeps its age. `present` gives, for every id an entry
    /// of this build goes by, in lower case, the id it has now.
    fn added_a_taken_folder(
        &self,
        candidate: &Candidate,
        present: &HashMap<String, String>,
        remembered: &Remembered,
        now: i64,
    ) -> Option<String> {
        let from_memory: HashSet<String>;
        let had = match self.folders_of_entry(candidate) {
            Some(had) => had,
            None => {
                let last = candidate
                    .known_as()
                    .find_map(|id| remembered.get(id))
                    .filter(|last| is_recent(last, now) && !last.folders.is_empty())?;
                from_memory = last.folders.iter().map(|f| fold_folder(f)).collect();
                &from_memory
            }
        };
        let its_own = |id: &str| candidate.known_as().any(|own| own.eq_ignore_ascii_case(id));
        for folder in &candidate.addon.folders {
            let folded = fold_folder(folder);
            if had.contains(&folded) {
                continue;
            }
            let owner = self.owners_of.get(&folded).and_then(|owners| {
                owners
                    .iter()
                    .filter(|owner| !its_own(owner))
                    .find_map(|owner| present.get(&owner.to_ascii_lowercase()))
            });
            if let Some(owner) = owner {
                return Some(format!(
                    "it just added the folder {folder}, which {owner} already lists"
                ));
            }
        }
        None
    }
}

/// A folder that listed addons require and nothing provides is the easiest one to squat:
/// once whatever ships it was trusted, the app would install it as a dependency. So only
/// something already listed with that folder, or something hand-picked, may fill it. Being
/// old doesn't count, and neither does being a WoWInterface listing: either can start
/// shipping any folder it likes in its next upload.
fn squats_a_dependency(candidate: &Candidate, open: &HashSet<String>) -> Option<String> {
    candidate.addon.folders.iter().find_map(|folder| {
        open.contains(&fold_folder(folder)).then(|| {
            format!(
                "it would be the first to provide {folder}, which addons already listed require"
            )
        })
    })
}

/// The former main folders vouched for each entry, as its claims: every entry in this build, and
/// every one away from it that still has them (see [`Vouches::away`]), so an entry that's held or
/// left out for a while doesn't lose them. An entry in `held`, which a first pass held, claims
/// what it moved away from and carried only as an entry away from the build does: while the last
/// catalog lists it, or for 30 days after it was last published. What `[pins]` says it was always
/// counts. Worked out before the pass, so its order can't decide it.
fn vouched_claims(
    candidates: &[Candidate],
    vouches: &Vouches<'_>,
    picks: &HandPicked,
    accounted: &HashSet<String>,
    held: &HashSet<String>,
    now: i64,
) -> FolderMap<Claim> {
    let mut claims = FolderMap::default();
    for candidate in candidates {
        let claim = Claim {
            id: candidate.addon.id.clone(),
            rank: Rank::of(candidate, picks),
        };
        let lapsed = held.contains(&candidate.addon.id.to_ascii_lowercase())
            && !vouches.published_lately(candidate, now);
        let folders = if lapsed {
            let main = candidate.addon.folders.first().map_or("", String::as_str);
            vouched_list(main, None, vouches.was_of(candidate), &[]).0
        } else {
            vouches.of(candidate).0
        };
        for folder in folders {
            claims.add(&folder, claim.clone());
        }
    }
    for away in vouches.away(accounted, now) {
        let claim = Claim {
            rank: Rank::new(&away.id, away.created, picks),
            id: away.id,
        };
        for folder in &away.folders {
            claims.add(folder, claim.clone());
        }
    }
    claims
}

/// A former main folder says an installed copy with it is that entry's addon, so it's that
/// entry's claim. Anything else shipping it is held unless it has the better claim (see
/// [`keeps_vouched_folder`]), so it can't take the copies the vouch is for, or strip the vouch.
fn ships_a_vouched_folder(
    candidate: &Candidate,
    claims: &FolderMap<Claim>,
    picks: &HandPicked,
    last: &LastCatalog<'_>,
) -> Option<String> {
    let id = &candidate.addon.id;
    let rank = Rank::of(candidate, picks);
    candidate.addon.folders.iter().find_map(|folder| {
        let owner = claims.alike(folder).find(|claim| {
            !claim.id.eq_ignore_ascii_case(id)
                && !keeps_vouched_folder(candidate, rank, folder, claim.rank, last)
        })?;
        Some(format!(
            "its folder {folder} was {}'s main folder",
            owner.id
        ))
    })
}

/// Hand-given names that end in a parenthetical, such as "Spoken Audio: Quests (English)", with
/// that part cut off, by the lowercased id of the hand-picked entry they're for. A stranger's
/// "Spoken Audio: Quests" would pass for the pack, so the pack claims that name too.
fn names_without_parentheses(curated: &Curated, picks: &HandPicked) -> HashMap<String, String> {
    curated
        .overrides
        .iter()
        .filter(|(id, _)| picks.contains(id))
        .filter_map(|(id, edit)| {
            let bare = without_parenthetical(edit.name.as_deref()?)?;
            Some((id.to_ascii_lowercase(), bare.to_string()))
        })
        .collect()
}

/// A name without the parenthetical it ends in, or `None` when it doesn't end in one.
fn without_parenthetical(name: &str) -> Option<&str> {
    let open = name.trim_end().strip_suffix(')')?.rfind(" (")?;
    let bare = name[..open].trim_end();
    (!bare.is_empty()).then_some(bare)
}

/// An established entry this build doesn't list: one missing from it, or one held in it. Its
/// name and main folder, folded, are still its claims at its rank.
#[derive(Debug, Clone)]
struct Absent {
    id: String,
    /// The GitHub owner, in lower case, for a repository or one of its release lines.
    owner: Option<String>,
    name: String,
    main: String,
    rank: Rank,
}

impl Absent {
    fn new(id: &str, name: &str, main: &str, created: i64, picks: &HandPicked) -> Self {
        Self {
            id: id.to_string(),
            owner: owner_of_id(id),
            name: fold_name(name),
            main: fold_folder(main),
            rank: Rank::new(id, created, picks),
        }
    }
}

/// Entries missing from this build whose name and main folder are still their claims: each one
/// the last catalog listed, and each one the scan cache remembers being published in the last 30
/// days, but none a maintainer excluded or the build has accounted for (see [`Vouches::away`]).
fn absent_entries(rules: &ClaimRules<'_>, accounted: &HashSet<String>) -> Vec<Absent> {
    let mut absent = Vec::new();
    let mut listed = HashSet::new();
    for addon in rules
        .previous
        .map_or(&[][..], |catalog| catalog.addons.as_slice())
    {
        let id = addon.id.to_ascii_lowercase();
        let away = !accounted.contains(&id) && !rules.curated.excludes_id(&id);
        listed.insert(id);
        if let Some(main) = addon.folders.first().filter(|_| away) {
            absent.push(Absent::new(
                &addon.id,
                &addon.name,
                main,
                addon.created,
                rules.picks,
            ));
        }
    }
    // In id order, so when two of them had the same name, the reason names the same one every run.
    absent.sort_by(|a, b| a.id.cmp(&b.id));
    for (key, last) in rules.remembered.entries() {
        if accounted.contains(key)
            || listed.contains(key)
            || !is_recent(last, rules.now)
            || last.name.is_empty()
            || rules.curated.excludes_id(key)
        {
            continue;
        }
        let id = last.id_or(key);
        absent.push(Absent::new(
            id,
            &last.name,
            &last.main,
            last.created,
            rules.picks,
        ));
    }
    absent
}

/// An entry that isn't listed in this build keeps its name and main folder, so a stranger can't
/// take them while it's held or away for a build and then shut it out as the older entry. One
/// that ranks behind it with that name or main folder is held, unless it's from the same GitHub
/// owner, whose own move it is. Only called for an entry nobody picked by hand.
fn takes_an_absent_entrys_place(
    candidate: &Candidate,
    absent: &[Absent],
    picks: &HandPicked,
) -> Option<String> {
    let rank = Rank::of(candidate, picks);
    let name = fold_name(&candidate.addon.name);
    let main = candidate.addon.folders.first();
    let owner = owner_of(candidate);
    absent.iter().find_map(|entry| {
        let itself = candidate
            .known_as()
            .any(|id| id.eq_ignore_ascii_case(&entry.id));
        let same_owner = owner.is_some() && owner == entry.owner;
        if itself || same_owner || !entry.rank.ahead_of(rank) {
            return None;
        }
        if !name.is_empty() && name == entry.name {
            return Some(format!(
                "its name is {}'s, which isn't listed in this build",
                entry.id
            ));
        }
        let main = main.filter(|main| fold_folder(main) == entry.main)?;
        Some(format!(
            "its main folder {main} is {}'s, which isn't listed in this build",
            entry.id
        ))
    })
}

/// What one run of the collision pass goes by.
struct Pass<'r> {
    rules: &'r ClaimRules<'r>,
    last: LastCatalog<'r>,
    open: HashSet<String>,
    bare_names: HashMap<String, String>,
    present: HashMap<String, String>,
    absent: Vec<Absent>,
}

impl Pass<'_> {
    /// Why each candidate, in order, is held, or `None` for one that's listed.
    fn run(&self, ordered: &[Candidate], claims: &FolderMap<Claim>) -> Vec<Option<String>> {
        let rules = self.rules;
        let picks = rules.picks;
        let mut absent = self.absent.clone();
        let mut ledger = Ledger::default();
        let mut reasons = Vec::with_capacity(ordered.len());
        for candidate in ordered {
            let picked = picks.contains(&candidate.addon.id);
            let problem = (!picked)
                .then(|| {
                    rules
                        .reserved
                        .problem(candidate)
                        .or_else(|| ships_a_vouched_folder(candidate, claims, picks, &self.last))
                        .or_else(|| {
                            self.last.added_a_taken_folder(
                                candidate,
                                &self.present,
                                rules.remembered,
                                rules.now,
                            )
                        })
                        .or_else(|| takes_an_absent_entrys_place(candidate, &absent, picks))
                })
                .flatten()
                .or_else(|| ledger.conflict(candidate, picks))
                .or_else(|| {
                    (!picked)
                        .then(|| squats_a_dependency(candidate, &self.open))
                        .flatten()
                });
            match &problem {
                // An established entry held this build keeps its name and main folder.
                Some(_) if candidate.addon.created != rules.now => {
                    if let Some(main) = candidate.addon.folders.first() {
                        let addon = &candidate.addon;
                        absent.push(Absent::new(
                            &addon.id,
                            &addon.name,
                            main,
                            addon.created,
                            picks,
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    let also = self
                        .bare_names
                        .get(&candidate.addon.id.to_ascii_lowercase());
                    ledger.keep(candidate, picks, also.map(String::as_str));
                }
            }
            reasons.push(problem);
        }
        reasons
    }
}

/// Settle who gets what. Candidates go in order, hand-picked first and then oldest, each
/// claiming its name and folders. One whose name or main folder is already taken is held
/// out of the catalog, and so is one with a side folder taken by something strictly ahead
/// of it, or a reserved name or folder, or a folder another entry is vouched for, or the name
/// or main folder of an older entry that isn't listed in this build, or too many siblings from
/// one owner. What was already listed together stays that way, so this only ever affects
/// newcomers and changes.
pub fn hold_collisions(
    candidates: Vec<Candidate>,
    rules: &ClaimRules<'_>,
) -> (Vec<Candidate>, Vec<Held>) {
    let picks = rules.picks;
    let mut ordered = candidates;
    ordered.sort_by(|a, b| {
        // Among two entries new in the same build, a moderated listing goes before a repository.
        let key = |c: &Candidate| {
            (
                !picks.contains(&c.addon.id),
                c.addon.created,
                !is_baseline(c) && !is_listing(c),
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| popularity(b, 0.0).total_cmp(&popularity(a, 0.0)))
            .then_with(|| a.addon.id.cmp(&b.addon.id))
    });

    // Every id an entry of this build goes by, after GitHub renamed it too, with the id it has now.
    let mut present = HashMap::new();
    for candidate in &ordered {
        for id in candidate.known_as() {
            present
                .entry(id.to_ascii_lowercase())
                .or_insert_with(|| candidate.addon.id.clone());
        }
    }
    let accounted: HashSet<String> = present
        .keys()
        .cloned()
        .chain(rules.left_out.iter().map(|id| id.to_ascii_lowercase()))
        .collect();
    let last = LastCatalog::new(rules.previous);
    let pass = Pass {
        rules,
        open: last.open_dependencies(&ordered),
        last,
        bare_names: names_without_parentheses(rules.curated, picks),
        absent: absent_entries(rules, &accounted),
        present,
    };
    let vouches = Vouches::new(rules.previous, rules.remembered, rules.curated);
    // A first pass says which entries are held, and the pass that counts gives those the claims
    // of an entry away from the build.
    let claims = vouched_claims(
        &ordered,
        &vouches,
        picks,
        &accounted,
        &HashSet::new(),
        rules.now,
    );
    let held_first: HashSet<String> = ordered
        .iter()
        .zip(pass.run(&ordered, &claims))
        .filter(|(_, problem)| problem.is_some())
        .map(|(candidate, _)| candidate.addon.id.to_ascii_lowercase())
        .collect();
    let claims = vouched_claims(
        &ordered,
        &vouches,
        picks,
        &accounted,
        &held_first,
        rules.now,
    );
    let reasons = pass.run(&ordered, &claims);

    let mut kept = Vec::with_capacity(ordered.len());
    let mut held = Vec::new();
    for (candidate, problem) in ordered.into_iter().zip(reasons) {
        match problem {
            Some(reason) => held.push(Held::new(&candidate.addon, reason)),
            None => kept.push(candidate),
        }
    }
    held.sort_by(|a, b| a.id.cmp(&b.id));
    (kept, held)
}

/// Decide what the app may pick by itself: anything hand-picked, a WoWInterface listing, or
/// anything that has kept its name for two weeks. Everything else is a stranger's new repo
/// and only gets installed when a player asks for it, and its ids on other sites are only
/// claims, so they go. Among what's left, an id belongs to whoever got there first.
/// Only what a person chose is reviewed, which is what it takes to be installed as another
/// addon's dependency, unless a reviewed entry lists it as a small pack: any addon can ship a
/// library's folder long before anything needs it. [`unreview_big_packs`] then takes it back
/// from packs too big for that, or of unknown size.
/// `candidates` must be in the collision pass's order.
pub fn settle_trust(
    candidates: &mut [Candidate],
    picks: &HandPicked,
    approved: &BTreeMap<String, String>,
    now: i64,
) {
    let present: HashSet<String> = candidates
        .iter()
        .map(|c| c.addon.id.to_ascii_lowercase())
        .collect();
    let mut wowi_owner: HashMap<String, String> = HashMap::new();
    for candidate in candidates.iter() {
        if let Source::Wowi { id } = &candidate.addon.source {
            wowi_owner.insert(id.to_string(), candidate.addon.id.to_ascii_lowercase());
        }
    }
    for (id, repo) in approved {
        let repo = format!("gh:{}", repo.to_ascii_lowercase());
        if present.contains(&repo) {
            wowi_owner.insert(id.clone(), repo);
        }
    }
    let mut curse_owner: HashMap<String, String> = HashMap::new();
    let mut wago_owner: HashMap<String, String> = HashMap::new();

    for candidate in candidates.iter_mut() {
        let addon = &mut candidate.addon;
        addon.reviewed = picks.contains(&addon.id);
        addon.trusted = addon.reviewed
            || matches!(addon.source, Source::Wowi { .. })
            || now - addon.created >= TRUST_AFTER_SECS;
        if !addon.trusted {
            addon.ids = Default::default();
            continue;
        }
        let me = addon.id.to_ascii_lowercase();
        let keep = |owners: &mut HashMap<String, String>, id: &mut Option<String>| {
            let first = id
                .as_ref()
                .is_some_and(|id| *owners.entry(id.clone()).or_insert_with(|| me.clone()) == me);
            if !first {
                *id = None;
            }
        };
        keep(&mut wowi_owner, &mut addon.ids.wowi);
        keep(&mut curse_owner, &mut addon.ids.curse);
        keep(&mut wago_owner, &mut addon.ids.wago);
    }
}

/// The packs [`unreview_big_packs`] published unreviewed, one summary line each.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unreviewed {
    /// Ones with a stable or beta download over 100 MB.
    pub big: Vec<String>,
    /// Ones with a stable or beta download whose size the catalog doesn't give.
    pub unknown_size: Vec<String>,
}

/// Release lines, and the packs `[[packs]]` names, whose download is over 100 MB go out trusted
/// but not reviewed, and so do ones with a build whose size isn't known, which could be any size.
/// Apps up to 0.1.2 install any reviewed entry another addon requires by itself, whatever its
/// size, and a voice pack can be 1.5 GB. Later apps never install a pack that big without asking,
/// and offer packs by their groups, which only needs them trusted. Returns a line for each, after
/// [`settle_trust`].
pub fn unreview_big_packs(candidates: &mut [Candidate], curated: &Curated) -> Unreviewed {
    let packs: HashSet<String> = curated
        .packs
        .iter()
        .flat_map(|group| group.ids.iter())
        .map(|id| id.to_ascii_lowercase())
        .collect();
    let mut unreviewed = Unreviewed::default();
    for candidate in candidates {
        let addon = &mut candidate.addon;
        let is_pack = is_line_id(&addon.id) || packs.contains(&addon.id.to_ascii_lowercase());
        if !is_pack || !addon.reviewed {
            continue;
        }
        let builds = [("stable", &addon.stable), ("beta", &addon.beta)];
        let size = builds
            .iter()
            .filter_map(|(_, build)| build.as_ref())
            .map(|release| release.size)
            .max()
            .unwrap_or(0);
        let unknown = builds
            .iter()
            .find(|(_, build)| build.as_ref().is_some_and(|release| release.size == 0))
            .map(|(channel, _)| *channel);
        if size > BIGGEST_UNASKED_DOWNLOAD {
            unreviewed
                .big
                .push(format!("{}, a {size}-byte download", addon.id));
        } else if let Some(channel) = unknown {
            unreviewed.unknown_size.push(format!(
                "{}, whose {channel} download's size isn't known",
                addon.id
            ));
        } else {
            continue;
        }
        addon.reviewed = false;
    }
    unreviewed
}

/// A summary line for each entry GitHub renamed whose old id was hand-picked. It isn't hand-picked
/// under its new name until a person writes that down, as with `[[split]]`.
pub fn renamed_picks(candidates: &[Candidate], picks: &HandPicked) -> Vec<String> {
    candidates
        .iter()
        .filter(|candidate| !picks.contains(&candidate.addon.id))
        .filter_map(|candidate| {
            let old = candidate.renamed_from.iter().find(|old| picks.contains(old))?;
            Some(format!(
                "{old} is {} on GitHub now. It isn't hand-picked under that name until seeds.toml or curated.toml uses it",
                candidate.addon.id
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use feeling_core::catalog::{Addon, ExternalIds};

    use super::*;
    use crate::curated::{GapSpec, Seeds, SplitSpec};

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 24 * 60 * 60;

    fn gh(repo: &str, folders: &[&str]) -> Candidate {
        let mut c = Candidate::default();
        c.addon.id = format!("gh:{repo}");
        c.addon.name = repo.split('/').nth(1).unwrap().into();
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.source = Source::Github { repo: repo.into() };
        c
    }

    fn named(mut candidate: Candidate, name: &str) -> Candidate {
        candidate.addon.name = name.into();
        candidate
    }

    fn wowi(id: u64, name: &str, folders: &[&str]) -> Candidate {
        let mut c = Candidate::default();
        c.addon.id = format!("wowi:{id}");
        c.addon.name = name.into();
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.source = Source::Wowi { id };
        c.addon.ids = ExternalIds {
            wowi: Some(id.to_string()),
            ..ExternalIds::default()
        };
        c
    }

    fn approved(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(id, repo)| ((*id).to_string(), (*repo).to_string()))
            .collect()
    }

    /// `seeds` is what's hand-picked: `owner/repo`, or a number for a WoWInterface listing.
    fn picks(seeds: &[&str]) -> HandPicked {
        let seeds = Seeds {
            repos: seeds
                .iter()
                .filter(|s| s.contains('/'))
                .map(|s| (*s).to_string())
                .collect(),
            wowi: seeds.iter().filter_map(|s| s.parse().ok()).collect(),
        };
        HandPicked::new(&seeds, &Curated::default())
    }

    fn ids(candidates: &[Candidate]) -> Vec<&str> {
        let mut ids: Vec<&str> = candidates.iter().map(|c| c.addon.id.as_str()).collect();
        ids.sort_unstable();
        ids
    }

    fn held_ids(held: &[Held]) -> Vec<&str> {
        held.iter().map(|h| h.id.as_str()).collect()
    }

    /// The catalog as it was published some builds ago: `created` 0 is one from before ages.
    fn published(entries: &[(&Candidate, i64)]) -> Catalog {
        Catalog {
            addons: entries
                .iter()
                .map(|(c, created)| Addon {
                    created: *created,
                    ..c.addon.clone()
                })
                .collect(),
            ..Catalog::default()
        }
    }

    /// Ages, then the collision pass, the same way a build does it.
    fn pass(
        mut list: Vec<Candidate>,
        chosen: &HandPicked,
        previous: Option<&Catalog>,
    ) -> (Vec<Candidate>, Vec<Held>) {
        pass_with(&mut list, chosen, previous, &Reserved::default())
    }

    fn pass_with(
        list: &mut Vec<Candidate>,
        chosen: &HandPicked,
        previous: Option<&Catalog>,
        reserved: &Reserved,
    ) -> (Vec<Candidate>, Vec<Held>) {
        stamp_ages(list, previous, &Remembered::default(), chosen, NOW);
        let rules = ClaimRules {
            picks: chosen,
            reserved,
            previous,
            remembered: &Remembered::default(),
            curated: &Curated::default(),
            now: NOW,
            left_out: &HashSet::new(),
        };
        hold_collisions(std::mem::take(list), &rules)
    }

    #[test]
    fn a_newcomer_cannot_be_the_first_to_fill_a_dependency() {
        let mut needs = gh("someone/NeedsLib", &["NeedsLib"]);
        needs.addon.deps = vec!["LibMissing".into()];
        let before = published(&[(&needs, 0)]);
        let squatter = gh("stranger/LibMissing", &["LibMissing"]);
        let (kept, held) = pass(vec![needs.clone(), squatter], &picks(&[]), Some(&before));
        assert_eq!(held_ids(&held), ["gh:stranger/LibMissing"]);
        assert_eq!(ids(&kept), ["gh:someone/NeedsLib"]);

        // A hand-picked repo or listing still can.
        let chosen = gh("trusted/LibMissing", &["LibMissing"]);
        let (_, held) = pass(
            vec![needs.clone(), chosen],
            &picks(&["trusted/LibMissing"]),
            Some(&before),
        );
        assert!(held.is_empty(), "{:?}", held_ids(&held));
        let seeded = wowi(42, "LibMissing", &["LibMissing"]);
        let (_, held) = pass(vec![needs.clone(), seeded], &picks(&["42"]), Some(&before));
        assert!(held.is_empty(), "{:?}", held_ids(&held));

        // Something already listed with that folder keeps it, however young.
        let young = gh("someone/LibMissing", &["LibMissing"]);
        let before = published(&[(&needs, 0), (&young, NOW - 3 * DAY)]);
        let (kept, held) = pass(vec![needs, young], &picks(&[]), Some(&before));
        assert!(held.is_empty(), "{:?}", held_ids(&held));
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_listing_cannot_be_the_first_to_fill_a_dependency_either() {
        let mut needs = gh("someone/NeedsLib", &["NeedsLib"]);
        needs.addon.deps = vec!["LibMissing".into()];
        let before = published(&[(&needs, 0)]);
        let listing = wowi(42, "Harmless Bags", &["HarmlessBags", "LibMissing"]);
        let (kept, held) = pass(vec![needs, listing], &picks(&[]), Some(&before));
        assert_eq!(held_ids(&held), ["wowi:42"]);
        assert!(held[0].reason.contains("LibMissing"), "{}", held[0].reason);
        assert_eq!(ids(&kept), ["gh:someone/NeedsLib"]);
    }

    #[test]
    fn an_established_entry_cannot_start_providing_a_dependency() {
        let mut needs = gh("someone/NeedsLib", &["NeedsLib"]);
        needs.addon.deps = vec!["LibMissing".into()];
        let helper = named(gh("zebra/Helpers", &["Helpers"]), "Helpers");
        let grown = named(gh("zebra/Helpers", &["Helpers", "LibMissing"]), "Helpers");
        // From before ages were kept, and older than the addon that needs the folder, which
        // also puts it first in the pass.
        for (helper_age, needs_age) in [(0, 0), (NOW - 120 * DAY, NOW - 30 * DAY)] {
            let before = published(&[(&needs, needs_age), (&helper, helper_age)]);
            let (kept, held) = pass(
                vec![grown.clone(), needs.clone()],
                &picks(&[]),
                Some(&before),
            );
            assert_eq!(held_ids(&held), ["gh:zebra/Helpers"], "{helper_age}");
            assert!(
                held[0].reason.contains("first to provide"),
                "{}",
                held[0].reason
            );
            assert_eq!(ids(&kept), ["gh:someone/NeedsLib"]);
        }
    }

    #[test]
    fn a_provider_that_left_doesnt_open_its_folder_to_a_newcomer() {
        let mut needs = gh("someone/NeedsLib", &["NeedsLib"]);
        needs.addon.deps = vec!["LibThing".into()];
        let gone = gh("author/LibThing", &["LibThing"]);
        let before = published(&[(&needs, 0), (&gone, 0)]);
        let squatter = gh("stranger/LibThing", &["LibThing"]);
        let (_, held) = pass(vec![needs, squatter], &picks(&[]), Some(&before));
        assert_eq!(held_ids(&held), ["gh:stranger/LibThing"]);
    }

    #[test]
    fn a_newcomer_requiring_a_folder_doesnt_hold_whatever_ships_it_next() {
        let lib = named(gh("author/Thing", &["Thing"]), "Thing");
        let before = published(&[(&lib, 0)]);
        let grown = named(gh("author/Thing", &["Thing", "Thing_Lib"]), "Thing");
        let mut griefer = gh("zebra/Wants", &["Wants"]);
        griefer.addon.deps = vec!["Thing_Lib".into()];
        let (kept, held) = pass(vec![griefer, grown], &picks(&[]), Some(&before));
        assert!(held.is_empty(), "{:?}", held_ids(&held));
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn an_approved_pair_merges_and_the_counts_add_up() {
        let mut github = gh("funkydude/BugSack", &["BugSack", "!BugGrabber"]);
        github.addon.ids.wowi = Some("5995".into());
        github.addon.downloads = 100;
        let mut listing = wowi(5995, "BugSack (WoWI)", &["BugSack"]);
        listing.addon.downloads = 900;
        listing.favorites = 3;
        let pairs = approved(&[("5995", "FUNKYDUDE/bugsack")]);
        let (out, merged) = merge_wowi(vec![github], vec![listing], &pairs);
        assert_eq!(merged, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].addon.id, "gh:funkydude/BugSack", "GitHub wins");
        assert_eq!(
            out[0].addon.downloads, 1000,
            "counts from both sources add up"
        );
        assert_eq!(out[0].favorites, 3);
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("5995"));
    }

    #[test]
    fn a_pair_gives_the_repository_the_id_even_when_its_toc_doesnt() {
        let github = gh("Nevcairiel/Mapster", &["Mapster"]);
        let listing = wowi(1234, "Something Else Entirely", &["Mapster"]);
        let pairs = approved(&[("1234", "Nevcairiel/Mapster")]);
        let (out, merged) = merge_wowi(vec![github], vec![listing], &pairs);
        assert_eq!(merged, 1);
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("1234"));
    }

    #[test]
    fn claiming_a_listings_id_merges_nothing_and_moves_no_counts() {
        let mut stranger = gh("zebra/CoolBags", &["CoolBags"]);
        stranger.addon.ids.wowi = Some("7032".into());
        stranger.addon.downloads = 1;
        let mut listing = wowi(7032, "TomTom", &["TomTom"]);
        listing.addon.downloads = 3_550_000;
        let (out, merged) = merge_wowi(vec![stranger], vec![listing], &BTreeMap::new());
        assert_eq!(merged, 0);
        assert_eq!(ids(&out), ["gh:zebra/CoolBags", "wowi:7032"]);
        assert_eq!(out[0].addon.downloads, 1, "the stranger gets none of it");
        assert_eq!(out[1].addon.downloads, 3_550_000);
    }

    #[test]
    fn matching_folders_merge_nothing_either() {
        let stranger = gh("zebra/TomTom", &["TomTom"]);
        let listing = wowi(7032, "TomTom", &["TomTom"]);
        let (out, merged) = merge_wowi(vec![stranger], vec![listing], &BTreeMap::new());
        assert_eq!(merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_pair_only_applies_to_the_repository_it_names() {
        let other = gh("zebra/TomTom", &["TomTom"]);
        let listing = wowi(7032, "TomTom", &["TomTom"]);
        let pairs = approved(&[("7032", "cladhaire/TomTom")]);
        let (out, merged) = merge_wowi(vec![other], vec![listing], &pairs);
        assert_eq!(merged, 0, "its repository isn't here, so the listing stays");
        assert_eq!(ids(&out), ["gh:zebra/TomTom", "wowi:7032"]);
    }

    #[test]
    fn two_listings_of_one_addon_settle_on_the_same_id_either_way() {
        let build = |listings: Vec<Candidate>| {
            let mut repo = gh("AssisrMatheus/TodoChecklister", &["TodoChecklister"]);
            repo.addon.ids.wowi = Some("25348".into());
            let pairs = approved(&[
                ("25348", "AssisrMatheus/TodoChecklister"),
                ("25349", "AssisrMatheus/TodoChecklister"),
            ]);
            merge_wowi(vec![repo], listings, &pairs)
        };
        let one = wowi(25348, "TodoChecklister", &["TodoChecklister"]);
        let two = wowi(25349, "TodoChecklister", &["TodoChecklister"]);
        let (forward, merged) = build(vec![one.clone(), two.clone()]);
        let (backward, _) = build(vec![two, one]);
        assert_eq!(merged, 2);
        assert_eq!(forward[0].addon.ids.wowi.as_deref(), Some("25348"));
        assert_eq!(forward, backward);
    }

    #[test]
    fn forks_with_the_same_folders_as_the_parent_go_away() {
        let parent = gh("Up/Stream", &["Foo"]);
        let mut copy = gh("me/Stream", &["Foo"]);
        copy.fork_of = Some("Up/Stream".into());
        let mut own_path = gh("you/Stream", &["FooReborn"]);
        own_path.fork_of = Some("Up/Stream".into());
        let (kept, dropped) =
            drop_redundant_forks(vec![parent, copy, own_path], &HandPicked::default());
        assert_eq!(dropped, ["gh:me/Stream"]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_fork_of_something_missing_can_stay_when_it_is_the_only_one() {
        let mut revived = gh("new/Keeper", &["Foo"]);
        revived.fork_of = Some("Gone/Abandoned".into());
        let (kept, dropped) = drop_redundant_forks(vec![revived], &HandPicked::default());
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn among_forks_of_a_missing_parent_the_best_known_survives() {
        let mut a = gh("a/Fork", &["Foo"]);
        a.fork_of = Some("Gone/Foo".into());
        a.addon.stars = 40;
        let mut b = gh("b/Fork", &["Foo"]);
        b.fork_of = Some("Gone/Foo".into());
        b.addon.stars = 2;
        let (kept, dropped) = drop_redundant_forks(vec![a, b], &HandPicked::default());
        assert_eq!(dropped, ["gh:b/Fork"]);
        assert_eq!(kept[0].addon.id, "gh:a/Fork");
    }

    #[test]
    fn a_fork_of_a_listed_fork_goes_away_too() {
        let mut middle = gh("middle/Stream", &["Foo"]);
        middle.fork_of = Some("Gone/Stream".into());
        let mut last = gh("last/Stream", &["Foo"]);
        last.fork_of = Some("middle/Stream".into());
        let (_, dropped) = drop_redundant_forks(vec![middle, last], &HandPicked::default());
        assert_eq!(dropped, ["gh:last/Stream"]);
    }

    #[test]
    fn a_new_repository_with_the_same_folders_doesnt_evict_a_fork() {
        for chosen in [picks(&[]), picks(&["Legacy-of-Sylvanaar/prat-3-0"])] {
            let mut real = gh("Legacy-of-Sylvanaar/prat-3-0", &["Prat-3.0"]);
            real.fork_of = Some("sylvanaar/prat-3-0".into());
            real.addon.stars = 38;
            let stranger = gh("free-account/Prat", &["Prat-3.0"]);
            let (kept, dropped) = drop_redundant_forks(vec![real, stranger], &chosen);
            assert!(dropped.is_empty(), "{dropped:?}");
            assert_eq!(kept.len(), 2, "the collision pass settles the rest");
        }
    }

    #[test]
    fn a_hand_picked_fork_is_never_dropped_even_beside_its_parent() {
        let parent = gh("sylvanaar/wim", &["WIM"]);
        let mut fork = gh("Legacy-of-Sylvanaar/wim", &["WIM"]);
        fork.fork_of = Some("sylvanaar/wim".into());
        let chosen = picks(&["Legacy-of-Sylvanaar/wim"]);
        let (kept, dropped) = drop_redundant_forks(vec![parent, fork], &chosen);
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn the_first_build_with_ages_makes_everything_listed_the_baseline() {
        let old = gh("a/Old", &["Old"]);
        let chosen = gh("a/Chosen", &["Chosen"]);
        let previous = published(&[(&old, 0), (&chosen, 0)]);
        let fresh = gh("a/Fresh", &["Fresh"]);
        let mut list = vec![old.clone(), chosen.clone(), fresh];
        stamp_ages(
            &mut list,
            Some(&previous),
            &Remembered::default(),
            &picks(&["a/Chosen"]),
            NOW,
        );
        assert_eq!(list[0].addon.created, BASELINE_CREATED);
        assert_eq!(list[1].addon.created, BASELINE_CREATED);
        assert_eq!(list[2].addon.created, NOW, "it wasn't listed");
    }

    #[test]
    fn an_age_stays_from_build_to_build() {
        let addon = gh("a/Thing", &["Thing"]);
        let previous = published(&[(&addon, NOW - 3 * DAY)]);
        let mut list = vec![addon];
        stamp_ages(
            &mut list,
            Some(&previous),
            &Remembered::default(),
            &picks(&[]),
            NOW,
        );
        assert_eq!(list[0].addon.created, NOW - 3 * DAY);
    }

    #[test]
    fn with_nothing_published_yet_everything_is_new() {
        let mut list = vec![gh("a/Thing", &["Thing"])];
        stamp_ages(&mut list, None, &Remembered::default(), &picks(&[]), NOW);
        assert_eq!(list[0].addon.created, NOW);
    }

    #[test]
    fn changing_a_name_or_main_folder_counts_as_new_unless_hand_picked() {
        let listed = named(gh("a/Thing", &["Thing", "Thing_Options"]), "Thing");
        let previous = published(&[(&listed, NOW - 90 * DAY)]);
        let age_after = |candidate: Candidate, chosen: &[&str]| {
            let mut list = vec![candidate];
            stamp_ages(
                &mut list,
                Some(&previous),
                &Remembered::default(),
                &picks(chosen),
                NOW,
            );
            list[0].addon.created
        };
        let retitled = named(listed.clone(), "Questie");
        assert_eq!(age_after(retitled.clone(), &[]), NOW);
        assert_eq!(age_after(retitled, &["a/Thing"]), NOW - 90 * DAY);

        let moved = gh("a/Thing", &["Questie", "Thing"]);
        assert_eq!(age_after(named(moved, "Thing"), &[]), NOW);

        // None of these is a different name to a reader, or a different folder.
        for same in ["THING", "Thing!", "Th\u{0456}ng"] {
            assert_eq!(
                age_after(named(listed.clone(), same), &[]),
                NOW - 90 * DAY,
                "{same}"
            );
        }
        let more_folders = gh("a/Thing", &["thing", "Thing_Options", "Thing_Extra"]);
        assert_eq!(
            age_after(named(more_folders, "Thing"), &[]),
            NOW - 90 * DAY,
            "a side folder isn't the addon's identity"
        );
    }

    #[test]
    fn an_entry_a_catalog_left_out_keeps_the_age_the_scan_cache_remembers_for_a_month() {
        let thing = named(gh("a/Thing", &["Thing", "Thing_Options"]), "Thing");
        let remembered_at = |days_ago: i64| {
            let mut noted = published(&[(&thing, NOW - 90 * DAY)]);
            noted.generated = NOW - days_ago * DAY;
            let mut remembered = Remembered::default();
            remembered.note(&noted);
            remembered
        };
        let without = Catalog::default();
        let age_after = |candidate: Candidate, remembered: &Remembered, chosen: &[&str]| {
            let mut list = vec![candidate];
            stamp_ages(&mut list, Some(&without), remembered, &picks(chosen), NOW);
            list[0].addon.created
        };
        let lately = remembered_at(10);
        assert_eq!(age_after(thing.clone(), &lately, &[]), NOW - 90 * DAY);
        // A side folder isn't its identity, as with the last catalog.
        let grown = named(gh("a/Thing", &["Thing", "Thing_Extra"]), "Thing");
        assert_eq!(age_after(grown, &lately, &[]), NOW - 90 * DAY);
        // Renamed, it's new, unless a person picked it.
        let retitled = named(thing.clone(), "Questie");
        assert_eq!(age_after(retitled.clone(), &lately, &[]), NOW);
        assert_eq!(age_after(retitled, &lately, &["a/Thing"]), NOW - 90 * DAY);
        let moved = named(gh("a/Thing", &["Questie", "Thing"]), "Thing");
        assert_eq!(age_after(moved, &lately, &[]), NOW);
        // Away over a month, it starts over.
        assert_eq!(age_after(thing.clone(), &remembered_at(31), &[]), NOW);
        // So does one noted before names were kept.
        let mut unnamed = Remembered::default();
        unnamed.note(&published(&[(&named(thing.clone(), ""), NOW - 90 * DAY)]));
        assert_eq!(age_after(thing.clone(), &unnamed, &[]), NOW);

        // A last catalog that lists it decides, even when the memory would say otherwise.
        let listed = published(&[(&named(thing.clone(), "Other"), NOW - 5 * DAY)]);
        let mut list = vec![thing.clone()];
        stamp_ages(&mut list, Some(&listed), &lately, &picks(&[]), NOW);
        assert_eq!(list[0].addon.created, NOW);

        // After GitHub renames the repository, its old id's listing or memory is its own.
        let mut renamed = named(gh("a/Renamed", &["Thing", "Thing_Options"]), "Thing");
        renamed.renamed_from = vec!["gh:a/Thing".into()];
        let old_listing = published(&[(&thing, NOW - 40 * DAY)]);
        let mut list = vec![renamed.clone()];
        stamp_ages(
            &mut list,
            Some(&old_listing),
            &Remembered::default(),
            &picks(&[]),
            NOW,
        );
        assert_eq!(list[0].addon.created, NOW - 40 * DAY);
        assert_eq!(age_after(renamed, &lately, &[]), NOW - 90 * DAY);
    }

    #[test]
    fn a_new_repo_cant_take_a_listings_name_with_or_without_claiming_its_id() {
        let tomtom = wowi(7032, "TomTom", &["TomTom"]);
        let previous = published(&[(&tomtom, 0)]);
        for chosen in [picks(&[]), picks(&["7032"])] {
            let mut with_id = gh("zebra/TomTom", &["TomTom"]);
            with_id.addon.ids.wowi = Some("7032".into());
            with_id.addon.downloads = 4_000_000;
            for stranger in [with_id, gh("zebra/TomTom", &["TomTom"])] {
                let (kept, held) = pass(vec![stranger, tomtom.clone()], &chosen, Some(&previous));
                assert_eq!(ids(&kept), ["wowi:7032"]);
                assert_eq!(held_ids(&held), ["gh:zebra/TomTom"]);
                assert!(held[0].reason.contains("wowi:7032"), "{}", held[0].reason);
            }
        }
    }

    #[test]
    fn download_counts_dont_decide_who_owns_a_name() {
        let mut real = named(gh("Peavers/PeaversCommons", &["PeaversCommons"]), "Commons");
        real.addon.downloads = 10;
        let previous = published(&[(&real, NOW - 30 * DAY)]);
        let mut copy = named(gh("zebra/Commons", &["PeaversCommons"]), "Commons");
        copy.addon.downloads = 9_000_000;
        copy.addon.stars = 5000;
        let (kept, held) = pass(vec![copy, real], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Peavers/PeaversCommons"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Commons"]);
    }

    #[test]
    fn look_alike_letters_dont_get_a_name_past_the_pass() {
        let questie = gh("Questie/Questie", &["Questie"]);
        let previous = published(&[(&questie, 0)]);
        let fake = named(gh("zebra/Totally", &["QuestieX"]), "Qu\u{0435}stie");
        let (kept, held) = pass(vec![fake, questie], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Questie/Questie"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Totally"]);
    }

    #[test]
    fn a_folder_that_only_looks_the_same_counts_as_taken() {
        let questie = gh("Questie/Questie", &["Questie"]);
        let previous = published(&[(&questie, 0)]);
        let fake = named(gh("zebra/Totally", &["Qu\u{0435}stie"]), "Something Else");
        let (_, held) = pass(vec![fake, questie], &picks(&[]), Some(&previous));
        assert_eq!(held_ids(&held), ["gh:zebra/Totally"]);
    }

    #[test]
    fn a_main_folder_taken_by_anyone_older_holds_a_newcomer_under_another_name() {
        let masque = named(gh("SFX-WoW/Masque", &["Masque"]), "Masque");
        let dominos = named(gh("t/Dominos", &["Dominos", "Masque_Dominos"]), "Dominos");
        let previous = published(&[(&masque, 0), (&dominos, 0)]);
        for (repo, main) in [("zebra/One", "Masque"), ("zebra/Two", "Masque_Dominos")] {
            let new = named(gh(repo, &[main]), "A New Name");
            let (_, held) = pass(
                vec![new, masque.clone(), dominos.clone()],
                &picks(&[]),
                Some(&previous),
            );
            assert_eq!(held_ids(&held), [repo.replace("zebra", "gh:zebra")]);
            assert!(held[0].reason.contains("main folder"), "{}", held[0].reason);
        }
    }

    #[test]
    fn a_side_folder_taken_by_something_older_holds_a_newcomer() {
        let questie = gh("Questie/Questie", &["Questie"]);
        let previous = published(&[(&questie, NOW - 90 * DAY)]);
        let pins = gh("zebra/CoolPins", &["CoolPins", "Questie"]);
        let (kept, held) = pass(vec![pins, questie], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Questie/Questie"]);
        assert_eq!(held_ids(&held), ["gh:zebra/CoolPins"]);
        assert!(held[0].reason.contains("Questie"), "{}", held[0].reason);
    }

    #[test]
    fn a_side_folder_shared_with_something_just_as_new_is_allowed() {
        let one = gh("a/One", &["One", "SharedLib"]);
        let two = gh("b/Two", &["Two", "SharedLib"]);
        let (kept, held) = pass(vec![one, two], &picks(&[]), None);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn what_was_already_listed_together_stays_together() {
        // Each of these pairs is in the real catalog today.
        let pairs = [
            (
                named(gh("delaysoftware/fightclub-forever", &["FightClub"]), "One"),
                named(gh("lordjumper/FightClub", &["FightClub"]), "Two"),
            ),
            (
                named(gh("SFX-WoW/Masque_Dominos", &["Masque_Dominos"]), "Skin"),
                named(gh("tullamods/Dominos", &["Dominos", "Masque_Dominos"]), "D"),
            ),
            (
                named(gh("LenweSaralonde/Musician", &["Musician"]), "Musician"),
                wowi(24915, "Musician", &["Musician", "MusicianList"]),
            ),
            (
                named(gh("stpain/Forever", &["Forever"]), "Forever"),
                named(gh("xIG/ForeverPlusPlus", &["ForeverPlusPlus"]), "Forever++"),
            ),
        ];
        for (a, b) in pairs {
            let previous = published(&[(&a, 0), (&b, 0)]);
            let chosen = [
                picks(&[]),
                picks(&["SFX-WoW/Masque_Dominos", "xIG/ForeverPlusPlus"]),
            ];
            for chosen in chosen {
                let (kept, held) = pass(vec![a.clone(), b.clone()], &chosen, Some(&previous));
                assert!(held.is_empty(), "{held:?}");
                assert_eq!(kept.len(), 2);
            }
        }
    }

    #[test]
    fn a_new_seed_beats_even_an_older_stranger_for_its_name() {
        let stranger = gh("zebra/WeakAuras", &["WeakAuras"]);
        let previous = published(&[(&stranger, 0)]);
        let real = named(gh("WeakAuras/WeakAuras2", &["WeakAuras"]), "WeakAuras");
        let chosen = picks(&["WeakAuras/WeakAuras2"]);
        let (kept, held) = pass(vec![stranger, real], &chosen, Some(&previous));
        assert_eq!(ids(&kept), ["gh:WeakAuras/WeakAuras2"]);
        assert_eq!(held_ids(&held), ["gh:zebra/WeakAuras"]);
    }

    #[test]
    fn two_hand_picked_repos_settle_like_they_always_have() {
        let mut big = named(gh("real/Details", &["Details"]), "Details!");
        big.addon.downloads = 5000;
        let small = named(gh("other/Details", &["Details"]), "Details!");
        let chosen = picks(&["real/Details", "other/Details"]);
        let (kept, held) = pass(vec![small, big.clone()], &chosen, None);
        assert_eq!(ids(&kept), ["gh:real/Details"], "an exact copy goes");
        assert!(held[0].reason.contains("copy"), "{}", held[0].reason);

        let different = named(gh("other/Details", &["Details"]), "Another Project");
        let (kept, held) = pass(vec![different, big], &chosen, None);
        assert!(held.is_empty(), "both were chosen: {held:?}");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_hand_given_name_counts_when_the_pass_runs_after_overrides() {
        // `settle` applies names first. This pins what that buys.
        let dbm = named(gh("Deadly/DBM", &["DBM-Core"]), "Deadly Boss Mods");
        let previous = published(&[(&dbm, 0)]);
        let fake = named(gh("zebra/Boss", &["BossThing"]), "Deadly Boss Mods");
        let (_, held) = pass(vec![fake, dbm], &picks(&["Deadly/DBM"]), Some(&previous));
        assert_eq!(held_ids(&held), ["gh:zebra/Boss"]);
    }

    #[test]
    fn a_retitled_sleeper_is_new_and_loses_to_what_was_there_first() {
        let real = gh("Peavers/PeaversCommons", &["PeaversCommons"]);
        let sleeper = gh("zebra/Helpers", &["Helpers"]);
        // The sleeper has been listed longer than the real addon.
        let previous = published(&[(&sleeper, NOW - 90 * DAY), (&real, NOW - 30 * DAY)]);
        let retitled = named(gh("zebra/Helpers", &["PeaversCommons"]), "PeaversCommons");
        let (kept, held) = pass(vec![retitled, real], &picks(&[]), Some(&previous));
        assert_eq!(ids(&kept), ["gh:Peavers/PeaversCommons"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
    }

    #[test]
    fn a_sleeper_cant_evict_a_younger_addon_by_adding_its_folder() {
        let real = gh("Peavers/PeaversCommons", &["PeaversCommons"]);
        let sleeper = named(gh("zebra/Helpers", &["Helpers"]), "Helpers");
        for created in [0, NOW - 90 * DAY] {
            let previous = published(&[(&sleeper, created), (&real, NOW - 30 * DAY)]);
            let grown = named(
                gh("zebra/Helpers", &["Helpers", "PeaversCommons"]),
                "Helpers",
            );
            let (kept, held) = pass(vec![grown, real.clone()], &picks(&[]), Some(&previous));
            assert_eq!(
                ids(&kept),
                ["gh:Peavers/PeaversCommons"],
                "the older claim on the folder wins"
            );
            assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
            assert!(held[0].reason.contains("just added"), "{}", held[0].reason);
        }
    }

    #[test]
    fn a_baseline_repo_cant_add_a_popular_side_folder_either() {
        let tomtom = wowi(7032, "TomTom", &["TomTom"]);
        let stranger = named(gh("zebra/Helpers", &["Helpers"]), "Helpers");
        let previous = published(&[(&tomtom, 0), (&stranger, 0)]);
        let grown = named(gh("zebra/Helpers", &["Helpers", "TomTom"]), "Helpers");
        let (kept, held) = pass(vec![grown, tomtom], &picks(&["7032"]), Some(&previous));
        assert_eq!(ids(&kept), ["wowi:7032"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
    }

    #[test]
    fn an_addon_adding_a_folder_nobody_else_had_is_fine() {
        let listed = gh("a/Thing", &["Thing"]);
        let previous = published(&[(&listed, NOW - 90 * DAY)]);
        let grown = named(gh("a/Thing", &["Thing", "Thing_Options"]), "Thing");
        let (kept, held) = pass(vec![grown], &picks(&[]), Some(&previous));
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept[0].addon.created, NOW - 90 * DAY);
    }

    #[test]
    fn a_held_entry_claims_nothing() {
        let tomtom = wowi(7032, "TomTom", &["TomTom"]);
        let previous = published(&[(&tomtom, 0)]);
        let first = named(gh("a/Copy", &["Elsewhere", "TomTom"]), "Elsewhere");
        let second = named(gh("b/Other", &["Elsewhere"]), "Other");
        let (kept, held) = pass(
            vec![first, second, tomtom],
            &picks(&["7032"]),
            Some(&previous),
        );
        assert_eq!(held_ids(&held), ["gh:a/Copy"]);
        assert_eq!(ids(&kept), ["gh:b/Other", "wowi:7032"]);
    }

    #[test]
    fn in_one_build_a_moderated_listing_goes_before_a_repository_and_the_outcome_is_stable() {
        let mut repo = named(gh("zebra/Foo", &["Foo"]), "Foo");
        repo.addon.downloads = 9_000_000;
        let listing = wowi(77, "Foo", &["Foo"]);
        let (kept, held) = pass(vec![repo.clone(), listing.clone()], &picks(&[]), None);
        assert_eq!(ids(&kept), ["wowi:77"]);
        assert_eq!(held_ids(&held), ["gh:zebra/Foo"]);

        let a = named(gh("a/One", &["Foo"]), "Foo");
        let b = named(gh("b/Two", &["Foo"]), "Foo");
        let (first, _) = pass(vec![a.clone(), b.clone()], &picks(&[]), None);
        let (second, _) = pass(vec![b, a], &picks(&[]), None);
        assert_eq!(ids(&first), ids(&second));
    }

    #[test]
    fn reserved_names_and_folders_are_only_for_something_hand_picked() {
        let curated = Curated {
            gaps: vec![
                GapSpec {
                    name: "Leatrix Plus".into(),
                    folders: vec!["Leatrix_Plus".into()],
                    ..GapSpec::default()
                },
                GapSpec {
                    name: "ElvUI".into(),
                    folders: vec!["ElvUI".into(), "ElvUI_Options".into()],
                    ..GapSpec::default()
                },
                GapSpec {
                    name: "BugGrabber".into(),
                    folders: vec!["!BugGrabber".into()],
                    ..GapSpec::default()
                },
            ],
            reserved: vec!["WeakAuras".into(), "pfQuest".into()],
            ..Curated::default()
        };
        let reserved = Reserved::new(&curated);
        let mut list = vec![
            named(gh("z/One", &["One"]), "Leatrix Plus"),
            named(gh("z/Two", &["Leatrix_Plus"]), "Something"),
            named(gh("z/Three", &["Three", "Leatrix_Plus"]), "Something 3"),
            named(gh("z/Four", &["Four"]), "ELV-UI"),
            named(gh("z/Five", &["ElvUI"]), "Something 5"),
            named(gh("z/Six", &["Six", "ElvUI_Options"]), "Something 6"),
            named(gh("z/Seven", &["Seven", "!BugGrabber"]), "Something 7"),
            named(gh("z/Eight", &["BugGrabber"]), "Bug Grabber"),
            named(gh("z/Nine", &["Nine", "WeakAuras"]), "Something 9"),
            named(gh("z/Ten", &["Ten"]), "Weak Auras"),
            named(gh("z/Eleven", &["pfQuest"]), "Something 11"),
            named(gh("z/Twelve", &["Twelve"]), "Something 12"),
            wowi(5, "ElvUI", &["ElvUI"]),
        ];
        let (kept, held) = pass_with(&mut list, &picks(&[]), None, &reserved);
        assert_eq!(
            ids(&kept),
            ["gh:z/Twelve"],
            "the only one not reserved: {held:?}"
        );
        assert_eq!(held.len(), 12);
        assert!(
            held.iter().all(|h| h.reason.contains("reserved")),
            "{held:?}"
        );

        let mut real = vec![
            named(gh("tukui-org/ElvUI", &["ElvUI", "ElvUI_Options"]), "ElvUI"),
            named(gh("WeakAuras/WeakAuras2", &["WeakAuras"]), "WeakAuras"),
        ];
        let chosen = picks(&["tukui-org/ElvUI", "WeakAuras/WeakAuras2"]);
        let (kept, held) = pass_with(&mut real, &chosen, None, &reserved);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn an_owner_gets_a_cap_oldest_first_but_hand_picked_repos_dont_count() {
        let repo = |at: usize| {
            named(
                gh(&format!("flood/R{at}"), &[&format!("F{at}")]),
                &format!("N{at}"),
            )
        };
        let flood: Vec<Candidate> = (0..MAX_ENTRIES_PER_OWNER + 5).map(repo).collect();
        // The oldest ones have the highest numbers, so order in the list can't decide this.
        let listed: Vec<(&Candidate, i64)> = flood
            .iter()
            .rev()
            .take(MAX_ENTRIES_PER_OWNER)
            .enumerate()
            .map(|(at, c)| (c, NOW - 100 * DAY + i64::try_from(at).unwrap()))
            .collect();
        let previous = published(&listed);
        let (kept, held) = pass(flood.clone(), &picks(&[]), Some(&previous));
        assert_eq!(kept.len(), MAX_ENTRIES_PER_OWNER);
        assert_eq!(held.len(), 5);
        assert!(held.iter().all(|h| h.reason.contains("flood")), "{held:?}");
        assert!(
            held.iter().any(|h| h.id == "gh:flood/R0"),
            "the new ones go"
        );

        let seed_names: Vec<String> = (0..MAX_ENTRIES_PER_OWNER + 5)
            .map(|at| format!("flood/R{at}"))
            .collect();
        let seeds: Vec<&str> = seed_names.iter().map(String::as_str).collect();
        let (kept, held) = pass(flood, &picks(&seeds), None);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), MAX_ENTRIES_PER_OWNER + 5);

        let others = vec![gh("other/One", &["One"]), gh("other/Two", &["Two"])];
        let (kept, _) = pass(others, &picks(&[]), None);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn release_lines_never_count_toward_their_owners_cap() {
        let at_cap: Vec<Candidate> = (0..MAX_ENTRIES_PER_OWNER)
            .map(|at| {
                named(
                    gh(&format!("flood/R{at}"), &[&format!("F{at}")]),
                    &format!("N{at}"),
                )
            })
            .collect();
        let listed: Vec<(&Candidate, i64)> = at_cap.iter().map(|c| (c, NOW - 100 * DAY)).collect();
        let previous = published(&listed);
        let curated = Curated {
            split: vec![SplitSpec {
                repo: "flood/Packs".into(),
                main: "main".into(),
                lines: vec!["one".into(), "two".into()],
            }],
            ..Curated::default()
        };
        let chosen = HandPicked::new(&Seeds::default(), &curated);
        let mut list = at_cap;
        for line in ["one", "two"] {
            let mut pack = named(
                gh("flood/Packs", &[&format!("Pack{line}")]),
                &format!("Pack {line}"),
            );
            pack.addon.id = SplitSpec::line_id("flood/Packs", line);
            list.push(pack);
        }
        let (kept, held) = pass(list, &chosen, Some(&previous));
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(kept.len(), MAX_ENTRIES_PER_OWNER + 2);
    }

    fn listed_as(id: &str, folders: &[&str]) -> Addon {
        Addon {
            id: id.into(),
            folders: folders.iter().map(|f| f.to_string()).collect(),
            ..Addon::default()
        }
    }

    fn pinned_by_hand(id: &str, main: &str, was: &[&str]) -> Curated {
        Curated {
            pins: BTreeMap::from([(
                id.to_string(),
                crate::curated::PinSpec {
                    main: (!main.is_empty()).then(|| main.into()),
                    was: was.iter().map(|f| f.to_string()).collect(),
                },
            )]),
            ..Curated::default()
        }
    }

    #[test]
    fn pins_come_from_curated_toml_then_the_last_catalog_then_the_scan_cache() {
        let previous = Catalog {
            addons: vec![
                listed_as("gh:A/Addon", &["Addon", "Addon_Options"]),
                listed_as("gh:b/data", &["DataPack"]),
                listed_as("wowi:7", &["Listing"]),
                listed_as("gh:c/empty", &[]),
            ],
            ..Catalog::default()
        };
        let curated = pinned_by_hand("GH:B/Data", "TheAddon", &[]);
        let pins = Pins::new(Some(&previous), &curated);
        let cached = gh("x/Cached", &["Cached"]);
        let main = |ids: &[&str]| pins.get(ids, Some(&cached)).map(|pin| pin.main);

        let listed = pins.get(&["gh:a/addon"], Some(&cached)).unwrap();
        assert_eq!(
            listed.main, "Addon",
            "the main folder, compared ignoring case"
        );
        assert!(!listed.written);
        let written = pins.get(&["gh:b/data"], None).unwrap();
        assert_eq!(written.main, "TheAddon", "[pins] beats the last catalog");
        assert!(written.written);
        assert_eq!(
            main(&["gh:a/Renamed", "gh:A/Addon"]).as_deref(),
            Some("Addon"),
            "under the name it was asked for, after GitHub renamed it"
        );
        for free in ["wowi:7", "gh:c/empty", "gh:new/one"] {
            assert_eq!(
                main(&[free]),
                None,
                "{free}: a readable catalog doesn't list it"
            );
        }

        // With no last catalog to read, the scan cache says which addon it is.
        let unread = Pins::new(None, &Curated::default());
        assert_eq!(
            unread.get(&["gh:new/one"], Some(&cached)).unwrap().main,
            "Cached"
        );
        assert_eq!(unread.get(&["gh:new/one"], None), None);
        let unread = Pins::new(None, &curated);
        assert_eq!(
            unread.get(&["gh:b/data"], Some(&cached)).unwrap().main,
            "TheAddon"
        );
        // A pin with only was names no main folder, so the last catalog's stands.
        let was_only = pinned_by_hand("gh:a/addon", "", &["AddonOld"]);
        let pins = Pins::new(Some(&previous), &was_only);
        let pin = pins.get(&["gh:a/addon"], None).unwrap();
        assert_eq!((pin.main.as_str(), pin.written), ("Addon", false));
    }

    #[test]
    fn an_entry_a_catalog_left_out_is_still_pinned_to_what_it_was_last_published_as() {
        let mut remembered = Remembered::default();
        remembered.note(&Catalog {
            addons: vec![
                listed_as("gh:a/Addon", &["Addon"]),
                listed_as("gh:b/Other", &["Other"]),
            ],
            ..Catalog::default()
        });
        // The next catalog lists the other one under a new main folder, and leaves this out.
        let previous = Catalog {
            addons: vec![listed_as("gh:b/Other", &["OtherCore"])],
            ..Catalog::default()
        };
        let cached = gh("x/Cached", &["Cached"]);
        for last in [Some(&previous), None] {
            let pins = Pins::new(last, &Curated::default()).remembering(&remembered);
            let main = |id: &str| pins.get(&[id], Some(&cached)).map(|pin| pin.main);
            assert_eq!(main("GH:A/ADDON").as_deref(), Some("Addon"), "{last:?}");
            assert_eq!(
                main("gh:new/one").as_deref(),
                last.map_or(Some("Cached"), |_| None)
            );
        }
        let pins = Pins::new(Some(&previous), &Curated::default()).remembering(&remembered);
        assert_eq!(
            pins.get(&["gh:b/other"], None).unwrap().main,
            "OtherCore",
            "the last catalog says first"
        );
        let written = pinned_by_hand("gh:a/Addon", "AddonCore", &[]);
        let pins = Pins::new(Some(&previous), &written).remembering(&remembered);
        assert_eq!(pins.get(&["gh:a/addon"], None).unwrap().main, "AddonCore");
    }

    fn formerly_of(last: Option<&Addon>, was: &[&str], now: &Addon) -> Vec<String> {
        let was: Vec<String> = was.iter().map(|f| f.to_string()).collect();
        let last = last.map(|last| {
            let main = last.folders.first().map_or("", String::as_str);
            (main, last.formerly.as_slice())
        });
        former_mains(last, &was, now).0
    }

    #[test]
    fn a_github_build_never_vouches_for_a_main_folder_by_itself() {
        // A repository can hold several addons, so a move there is only a rename when a
        // person says so: the next build could be its other addon.
        let last = listed_as("gh:a/Addon", &["HogHeals", "HogHeals_Frames"]);
        let moved = gh("a/Addon", &["HogTronUI", "HogHeals", "HogHeals_Frames"]).addon;
        assert!(formerly_of(Some(&last), &[], &moved).is_empty());
        assert!(
            formerly_of(Some(&last), &[], &gh("a/Addon", &["HogTronUI"]).addon).is_empty(),
            "the old folder is gone, which another product of the repository would do too"
        );
        assert_eq!(
            formerly_of(
                Some(&last),
                &["HogHeals"],
                &gh("a/Addon", &["HogTronUI"]).addon
            ),
            ["HogHeals"],
            "unless a person wrote it in was"
        );
        assert!(
            formerly_of(None, &[], &moved).is_empty(),
            "nothing to move from"
        );
    }

    #[test]
    fn a_listing_direct_entry_or_release_line_vouches_for_any_main_folder_it_moves_away_from() {
        let last = listed_as("wowi:424242", &["ShortKings"]);
        let renamed = wowi(424_242, "Short Royalty", &["ShortRoyalty"]);
        assert_eq!(
            formerly_of(Some(&last), &[], &renamed.addon),
            ["ShortKings"]
        );
        let mut direct = renamed.addon.clone();
        direct.source = Source::Direct {
            url: "https://foreverdb.net".into(),
        };
        assert_eq!(formerly_of(Some(&last), &[], &direct), ["ShortKings"]);
        let same = wowi(424_242, "Short Kings", &["shortkings", "Extra"]).addon;
        assert!(
            formerly_of(Some(&last), &[], &same).is_empty(),
            "case isn't a move"
        );

        // A release line's tag prefix fixes its addon, the way Spoken's packs have renamed
        // their folders before: a new main folder there is a rename, shipped or not.
        const LINE: &str = "gh:rusty-key/spoken-wow/bosses-audio";
        let last = listed_as(LINE, &["SpokenBossesAudio"]);
        let mut split = gh(
            "rusty-key/spoken-wow",
            &["SpokenBossesAudioAlliance", "SpokenBossesAudioHorde"],
        );
        split.addon.id = LINE.into();
        assert_eq!(
            formerly_of(Some(&last), &[], &split.addon),
            ["SpokenBossesAudio"]
        );
        let mut moved = gh(
            "rusty-key/spoken-wow",
            &["SpokenBossesAudioHorde", "SpokenBossesAudio"],
        );
        moved.addon.id = LINE.into();
        assert_eq!(
            formerly_of(Some(&last), &[], &moved.addon),
            ["SpokenBossesAudio"]
        );
    }

    #[test]
    fn a_split_repositorys_own_entry_never_vouches_for_what_a_catalog_listed_in_its_place() {
        // Before [[split]], the builder could list a pack under Spoken's own id, so the last
        // catalog's main folder there may be a pack's. Vouching for it would let an update
        // turn that 1.3 GB pack into Spoken and delete it.
        let curated = Curated {
            split: vec![crate::curated::SplitSpec {
                repo: "rusty-key/spoken-wow".into(),
                main: "spoken".into(),
                lines: vec!["quests-audio".into()],
            }],
            ..Curated::default()
        };
        let previous = Catalog {
            addons: vec![listed_as(
                "gh:rusty-key/spoken-wow",
                &["SpokenQuestsAudio_esES"],
            )],
            ..Catalog::default()
        };
        let mut candidates = vec![gh("rusty-key/spoken-wow", &["Spoken", "Spoken_Quests"])];
        let stamped = stamp_formerly(
            &mut candidates,
            Some(&previous),
            &Remembered::default(),
            &curated,
            &HandPicked::default(),
        );
        assert!(candidates[0].addon.formerly.is_empty());
        assert_eq!(
            stamped.moved,
            [
                "gh:rusty-key/spoken-wow lists Spoken first now, not SpokenQuestsAudio_esES. If they're the same addon, write was = [\"SpokenQuestsAudio_esES\"] under [pins.\"gh:rusty-key/spoken-wow\"] in curated.toml"
            ],
            "named, for a person to decide"
        );
    }

    #[test]
    fn formerly_carries_over_newest_first_without_doubles_or_the_main_folder() {
        let mut last = listed_as("wowi:1", &["Five"]);
        last.formerly = ["Four", "Three", "Two", "One"].map(String::from).to_vec();
        let now = wowi(1, "Six", &["Six"]).addon;
        assert_eq!(
            formerly_of(Some(&last), &["THREE"], &now),
            ["Five", "Four", "Three", "Two", "One"],
            "what's already there stays where it is"
        );

        // Back to a name it had before: that one is the main folder again.
        let mut back = now.clone();
        back.folders = vec!["three".into()];
        assert_eq!(
            formerly_of(Some(&last), &[], &back),
            ["Five", "Four", "Two", "One"]
        );

        // Only the most recent eight, and nothing unsafe a hand-edited catalog could carry.
        last.formerly = (1..=10).map(|n| format!("Old{n}")).collect();
        last.formerly.insert(3, "../evil".into());
        let kept = formerly_of(Some(&last), &[], &now);
        assert_eq!(kept.len(), MAX_FORMERLY);
        assert_eq!(kept[0], "Five");
        assert_eq!(kept[MAX_FORMERLY - 1], "Old7");
        assert!(!kept.iter().any(|f| f.contains('/')));
    }

    #[test]
    fn a_folder_written_in_was_never_pushes_out_one_already_vouched_for() {
        // The folder just moved away from first, then what's carried, then what's written.
        let was: Vec<String> = (1..=8).map(|n| format!("Written{n}")).collect();
        let (kept, cut) = vouched_list("Now", Some("Before"), &was, &["Carried".into()]);
        assert_eq!(
            kept,
            [
                "Before", "Carried", "Written1", "Written2", "Written3", "Written4", "Written5",
                "Written6"
            ]
        );
        assert_eq!(
            cut,
            ["Written7", "Written8"],
            "named, so a person can see it"
        );

        // Written again once carried, it keeps its place and nothing rotates.
        let (again, cut_again) = vouched_list("Now", None, &was, &kept);
        assert_eq!(again, kept);
        assert_eq!(cut_again, cut);
        // A carried folder that doesn't fit is the oldest, and goes without a line.
        let carried: Vec<String> = (1..=8).map(|n| format!("Carried{n}")).collect();
        let (kept, cut) = vouched_list("Now", Some("Before"), &[], &carried);
        assert_eq!(kept.len(), MAX_FORMERLY);
        assert_eq!(kept[0], "Before");
        assert!(cut.is_empty());
    }

    #[test]
    fn stamping_formerly_names_what_it_newly_vouches_for() {
        let mut last = listed_as("wowi:1", &["Old"]);
        last.formerly = vec!["Older".into()];
        let previous = Catalog {
            addons: vec![last, listed_as("gh:a/Addon", &["Addon"])],
            ..Catalog::default()
        };
        let mut candidates = vec![wowi(1, "New", &["New"]), gh("a/Addon", &["Addon"])];
        let curated = pinned_by_hand("gh:a/addon", "Addon", &["AddonClassic"]);
        let stamped = stamp_formerly(
            &mut candidates,
            Some(&previous),
            &Remembered::default(),
            &curated,
            &HandPicked::default(),
        );
        assert_eq!(candidates[0].addon.formerly, ["Old", "Older"]);
        assert_eq!(candidates[1].addon.formerly, ["AddonClassic"]);
        assert_eq!(
            stamped,
            Stamped {
                vouched: vec![
                    "wowi:1 was Old".into(),
                    "gh:a/Addon was AddonClassic".into()
                ],
                ..Stamped::default()
            }
        );
    }

    #[test]
    fn what_an_entry_was_comes_back_after_a_catalog_that_left_it_out() {
        let mut remembered = Remembered::default();
        let mut gone = listed_as("wowi:1", &["New"]);
        gone.formerly = vec!["Old".into()];
        remembered.note(&Catalog {
            addons: vec![gone],
            ..Catalog::default()
        });
        let mut candidates = vec![wowi(1, "Newer", &["Newer"])];
        let stamped = stamp_formerly(
            &mut candidates,
            Some(&Catalog::default()),
            &remembered,
            &Curated::default(),
            &HandPicked::default(),
        );
        assert_eq!(candidates[0].addon.formerly, ["New", "Old"]);
        assert_eq!(
            stamped.vouched,
            ["wowi:1 was New"],
            "Old was vouched for already"
        );
    }

    /// Stamps `formerly` for a listing that was Questie and is NewName now, `days` old or from
    /// before ages were kept, beside `others`, and returns its vouches and the lines about dropped
    /// ones. `pins_had_questie` says whether the last catalog listed gh:b/Pins with a Questie
    /// folder.
    fn stamp_beside(
        days: Option<i64>,
        others: Vec<Candidate>,
        chosen: &HandPicked,
        pins_had_questie: bool,
    ) -> (Vec<String>, Vec<String>) {
        let pins_folders: &[&str] = if pins_had_questie {
            &["Pins", "Questie"]
        } else {
            &["Pins"]
        };
        let previous = Catalog {
            addons: vec![
                listed_as("wowi:1", &["Questie"]),
                listed_as("gh:b/Pins", pins_folders),
            ],
            ..Catalog::default()
        };
        let renamed = wowi(1, "New Name", &["NewName"]);
        let mut candidates = vec![days.map_or(renamed.clone(), |days| aged(renamed, days))];
        candidates.extend(others);
        let stamped = stamp_formerly(
            &mut candidates,
            Some(&previous),
            &Remembered::default(),
            &Curated::default(),
            chosen,
        );
        (candidates[0].addon.formerly.clone(), stamped.unvouched)
    }

    #[test]
    fn a_vouch_goes_only_to_an_entry_with_the_better_claim_to_its_folder() {
        let pins = || gh("b/Pins", &["Pins", "Questie"]);
        // Something hand-picked shipping it wins it, whenever it arrived.
        let (formerly, unvouched) = stamp_beside(
            Some(3),
            vec![gh("questie/questie", &["Questie"])],
            &picks(&["questie/questie"]),
            true,
        );
        assert!(formerly.is_empty());
        assert_eq!(
            unvouched,
            ["wowi:1 no longer vouches for Questie, which gh:questie/questie ships"]
        );

        // So does an entry the last catalog listed with it that's older, or level with it as
        // two entries from before ages were kept are.
        let (formerly, _) = stamp_beside(Some(3), vec![aged(pins(), 90)], &picks(&[]), true);
        assert!(formerly.is_empty());
        let (formerly, unvouched) = stamp_beside(None, vec![pins()], &picks(&[]), true);
        assert!(formerly.is_empty());
        assert_eq!(
            unvouched,
            ["wowi:1 no longer vouches for Questie, which gh:b/Pins ships"]
        );

        // Listed with it but younger, it got the folder while the listing was away or after its
        // rename, so the listing keeps the vouch, and the collision pass holds the other.
        let (formerly, unvouched) =
            stamp_beside(Some(90), vec![aged(pins(), 3)], &picks(&[]), true);
        assert_eq!(formerly, ["Questie"]);
        assert!(unvouched.is_empty());
        // And older but only now shipping it, its claim is from today.
        let (formerly, _) = stamp_beside(Some(3), vec![aged(pins(), 90)], &picks(&[]), false);
        assert_eq!(formerly, ["Questie"]);
        let (formerly, _) = stamp_beside(None, vec![pins()], &picks(&[]), false);
        assert_eq!(formerly, ["Questie"]);
    }

    /// The collision pass with what the scan cache remembers, at the time `now`.
    fn pass_at(
        mut list: Vec<Candidate>,
        chosen: &HandPicked,
        previous: &Catalog,
        remembered: &Remembered,
        now: i64,
    ) -> (Vec<Candidate>, Vec<Held>) {
        stamp_ages(&mut list, Some(previous), remembered, chosen, now);
        let rules = ClaimRules {
            picks: chosen,
            reserved: &Reserved::default(),
            previous: Some(previous),
            remembered,
            curated: &Curated::default(),
            now,
            left_out: &HashSet::new(),
        };
        hold_collisions(list, &rules)
    }

    fn vouching(mut candidate: Candidate, formerly: &[&str]) -> Candidate {
        candidate.addon.formerly = formerly.iter().map(|f| f.to_string()).collect();
        candidate
    }

    #[test]
    fn an_entry_away_from_a_build_keeps_its_vouched_folders_for_a_month() {
        let tomtom = vouching(wowi(7032, "TomTom", &["TomTomForever"]), &["TomTom"]);
        let waypoints = || gh("zebra/Waypoints", &["Waypoints", "TomTom"]);
        let reason = "its folder TomTom was wowi:7032's main folder";

        // The last catalog listed the listing, and this build doesn't have it.
        let previous = published(&[(&tomtom, NOW - 10 * DAY)]);
        let nothing = Remembered::default();
        let (_, held) = pass_at(vec![waypoints()], &picks(&[]), &previous, &nothing, NOW);
        assert_eq!(held_ids(&held), ["gh:zebra/Waypoints"]);
        assert_eq!(held[0].reason, reason);

        // Only the scan cache remembers it, from a catalog a few weeks old.
        let mut remembered = Remembered::default();
        let mut noted = published(&[(&tomtom, NOW - 60 * DAY)]);
        noted.generated = NOW - 29 * DAY;
        remembered.note(&noted);
        let without = Catalog::default();
        let (_, held) = pass_at(vec![waypoints()], &picks(&[]), &without, &remembered, NOW);
        assert_eq!(held[0].reason, reason);
        // Noted before the time was kept, it counts as recent.
        let mut undated = Remembered::default();
        noted.generated = 0;
        undated.note(&noted);
        let (_, held) = pass_at(vec![waypoints()], &picks(&[]), &without, &undated, NOW);
        assert_eq!(held[0].reason, reason);
        // Gone over a month, its claims go with it.
        let (kept, held) = pass_at(
            vec![waypoints()],
            &picks(&[]),
            &without,
            &remembered,
            NOW + 2 * DAY,
        );
        assert!(held.is_empty(), "{:?}", held_ids(&held));
        assert_eq!(ids(&kept), ["gh:zebra/Waypoints"]);

        // Back again, it ranks ahead of what got the folder while it was gone.
        let since = published(&[(&waypoints(), NOW - 5 * DAY)]);
        let (_, held) = pass_at(
            vec![waypoints(), tomtom.clone()],
            &picks(&["7032"]),
            &since,
            &remembered,
            NOW,
        );
        assert_eq!(held_ids(&held), ["gh:zebra/Waypoints"]);
    }

    #[test]
    fn an_old_entry_only_now_shipping_a_vouched_folder_is_held_however_old() {
        // The listing renamed itself a while ago and is younger for it. The repository is older,
        // and adds the folder it moved away from.
        let tomtom = vouching(wowi(7032, "TomTom", &["TomTomForever"]), &["TomTom"]);
        let helpers = gh("zebra/Helpers", &["Helpers"]);
        let previous = published(&[(&tomtom, NOW - 20 * DAY), (&helpers, NOW - 400 * DAY)]);
        let grown = gh("zebra/Helpers", &["Helpers", "TomTom"]);
        let (kept, held) = pass_at(
            vec![tomtom.clone(), grown.clone()],
            &picks(&[]),
            &previous,
            &Remembered::default(),
            NOW,
        );
        assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
        assert_eq!(
            held[0].reason,
            "its folder TomTom was wowi:7032's main folder"
        );
        assert_eq!(ids(&kept), ["wowi:7032"]);

        // The same while the listing is away.
        let (_, held) = pass_at(
            vec![grown],
            &picks(&[]),
            &previous,
            &Remembered::default(),
            NOW,
        );
        assert_eq!(held_ids(&held), ["gh:zebra/Helpers"]);
    }

    #[test]
    fn a_folder_that_only_looks_like_a_vouched_one_counts_as_it() {
        let previous = published(&[(
            &vouching(wowi(7032, "TomTom", &["TomTomForever"]), &["TomTom"]),
            0,
        )]);
        let tomtom = wowi(7032, "TomTom", &["TomTomForever"]);
        // A Cyrillic o, which folding sees through. The catalog check only ignores case.
        let look_alike = gh("zebra/Waypoints", &["Waypoints", "TomT\u{043E}m"]);
        let (_, held) = pass(vec![tomtom, look_alike], &picks(&[]), Some(&previous));
        assert_eq!(
            held[0].reason,
            "its folder TomT\u{043E}m was wowi:7032's main folder"
        );

        // A hand-picked one takes it, and the vouch goes, so nothing alike is left beside it.
        let mut list = vec![
            vouching(
                aged(wowi(7032, "TomTom", &["TomTomForever"]), 3),
                &["TomTom"],
            ),
            gh("chosen/Waypoints", &["Waypoints", "TomT\u{043E}m"]),
        ];
        let stamped = stamp_formerly(
            &mut list,
            Some(&previous),
            &Remembered::default(),
            &Curated::default(),
            &picks(&["chosen/Waypoints"]),
        );
        assert!(list[0].addon.formerly.is_empty());
        assert_eq!(
            stamped.unvouched,
            ["wowi:7032 no longer vouches for TomTom, which gh:chosen/Waypoints ships"]
        );
    }

    #[test]
    fn a_folder_another_entry_is_vouched_for_holds_a_newcomer_but_not_what_had_it() {
        let previous = published(&[
            (&wowi(7032, "TomTom", &["TomTom"]), 0),
            (&gh("old/MapPins", &["MapPins", "TomTom"]), 0),
        ]);
        let list = vec![
            wowi(7032, "TomTom", &["TomTomForever"]),
            gh("old/MapPins", &["MapPins", "TomTom"]),
            gh("zebra/Waypoints", &["Waypoints", "tomtom"]),
        ];
        let (kept, held) = pass(list, &picks(&[]), Some(&previous));
        assert_eq!(held_ids(&held), ["gh:zebra/Waypoints"]);
        assert_eq!(
            held[0].reason,
            "its folder tomtom was wowi:7032's main folder"
        );
        assert_eq!(ids(&kept), ["gh:old/MapPins", "wowi:7032"]);
    }

    #[test]
    fn a_name_ending_in_a_parenthetical_claims_the_name_without_it_too() {
        assert_eq!(
            without_parenthetical("Spoken Audio: Quests (English)"),
            Some("Spoken Audio: Quests")
        );
        assert_eq!(without_parenthetical("Prat (3.0) Extras"), None);
        assert_eq!(without_parenthetical("(English)"), None);
        assert_eq!(without_parenthetical("Spoken Audio"), None);
        assert_eq!(without_parenthetical("A (b) (c)"), Some("A (b)"));
    }

    fn trust(
        mut list: Vec<Candidate>,
        chosen: &HandPicked,
        pairs: &[(&str, &str)],
    ) -> Vec<Candidate> {
        settle_trust(&mut list, chosen, &approved(pairs), NOW);
        list
    }

    fn aged(mut candidate: Candidate, days: i64) -> Candidate {
        candidate.addon.created = NOW - days * DAY;
        candidate
    }

    fn with_ids(mut candidate: Candidate, wowi: &str, curse: &str, wago: &str) -> Candidate {
        candidate.addon.ids = ExternalIds {
            wowi: Some(wowi.into()),
            curse: Some(curse.into()),
            wago: Some(wago.into()),
        };
        candidate
    }

    #[test]
    fn only_hand_picked_listed_and_established_entries_are_trusted() {
        let out = trust(
            vec![
                aged(gh("a/Chosen", &["A"]), 0),
                aged(wowi(9, "Listing", &["B"]), 0),
                aged(gh("a/Old", &["C"]), 14),
                aged(gh("a/Older", &["D"]), 400),
                aged(gh("a/Young", &["E"]), 13),
                aged(gh("a/New", &["F"]), 0),
            ],
            &picks(&["a/Chosen"]),
            &[],
        );
        let trusted: Vec<&str> = out
            .iter()
            .filter(|c| c.addon.trusted)
            .map(|c| c.addon.id.as_str())
            .collect();
        assert_eq!(trusted, ["gh:a/Chosen", "wowi:9", "gh:a/Old", "gh:a/Older"]);
    }

    #[test]
    fn only_what_a_person_chose_is_reviewed() {
        let curated = Curated {
            featured: vec!["gh:a/Featured".into()],
            merges: [("77".to_string(), "a/Paired".to_string())].into(),
            ..Curated::default()
        };
        let seeds = Seeds {
            repos: vec!["a/Seeded".into()],
            wowi: vec![9],
        };
        let out = trust(
            vec![
                aged(gh("a/Seeded", &["A"]), 0),
                aged(wowi(9, "Seeded Listing", &["B"]), 0),
                aged(gh("a/Featured", &["C"]), 0),
                aged(gh("a/Paired", &["D"]), 0),
                aged(wowi(77, "Paired Listing", &["E"]), 0),
                aged(wowi(10, "Listing", &["F"]), 0),
                aged(gh("a/Older", &["G"]), 400),
                aged(gh("a/New", &["H"]), 0),
            ],
            &HandPicked::new(&seeds, &curated),
            &[],
        );
        let reviewed: Vec<&str> = out
            .iter()
            .filter(|c| c.addon.reviewed)
            .map(|c| c.addon.id.as_str())
            .collect();
        assert_eq!(
            reviewed,
            [
                "gh:a/Seeded",
                "wowi:9",
                "gh:a/Featured",
                "gh:a/Paired",
                "wowi:77"
            ]
        );
        assert!(
            out.iter().all(|c| c.addon.trusted || !c.addon.reviewed),
            "reviewed is trusted too"
        );
    }

    #[test]
    fn a_pack_with_a_build_of_unknown_size_goes_out_unreviewed_with_its_own_line() {
        const MB: u64 = 1_000_000;
        let line = SplitSpec::line_id("a/Packs", "voices");
        let curated = Curated {
            packs: vec![crate::curated::PackSpec {
                addon: "gh:a/Addon".into(),
                name: "Voices".into(),
                ids: vec!["wowi:1".into(), "wowi:2".into(), "wowi:3".into()],
                met_by: vec!["Voices".into()],
                ..crate::curated::PackSpec::default()
            }],
            ..Curated::default()
        };
        let build = |size: u64| {
            Some(feeling_core::catalog::Release {
                version: "1".into(),
                size,
                ..feeling_core::catalog::Release::default()
            })
        };
        let pack = |id: &str, stable: u64, beta: Option<u64>| {
            let mut c = gh("a/Packs", &["Voices"]);
            c.addon.id = id.into();
            c.addon.stable = build(stable);
            c.addon.beta = beta.and_then(build);
            c.addon.reviewed = true;
            c
        };
        let mut list = vec![
            pack(&line, 0, None),
            pack("wowi:1", 50 * MB, Some(0)),
            pack("wowi:2", 50 * MB, None),
            pack("wowi:3", 0, Some(200 * MB)),
            pack("gh:a/NotAPack", 0, None),
        ];
        let unreviewed = unreview_big_packs(&mut list, &curated);
        assert_eq!(
            unreviewed,
            Unreviewed {
                big: vec!["wowi:3, a 200000000-byte download".into()],
                unknown_size: vec![
                    "gh:a/Packs/voices, whose stable download's size isn't known".into(),
                    "wowi:1, whose beta download's size isn't known".into(),
                ],
            }
        );
        let reviewed: Vec<&str> = list
            .iter()
            .filter(|c| c.addon.reviewed)
            .map(|c| c.addon.id.as_str())
            .collect();
        assert_eq!(
            reviewed,
            ["wowi:2", "gh:a/NotAPack"],
            "a known small pack with no beta at all, and what isn't a pack"
        );
    }

    #[test]
    fn the_ids_of_an_untrusted_entry_are_only_claims_and_go() {
        let out = trust(
            vec![
                with_ids(aged(gh("zebra/New", &["A"]), 1), "7032", "2382", "abc"),
                with_ids(aged(gh("a/Old", &["B"]), 30), "1", "2", "3"),
            ],
            &picks(&[]),
            &[],
        );
        assert!(out[0].addon.ids.is_empty());
        assert_eq!(out[1].addon.ids.wowi.as_deref(), Some("1"));
        assert_eq!(out[1].addon.ids.curse.as_deref(), Some("2"));
        assert_eq!(out[1].addon.ids.wago.as_deref(), Some("3"));
    }

    #[test]
    fn an_id_stays_with_whoever_owns_it() {
        // In the collision pass's order: hand-picked first.
        let out = trust(
            vec![
                with_ids(
                    aged(gh("mooreatv/AuctionDB", &["AuctionDB"]), 90),
                    "25159",
                    "336518",
                    "R4N",
                ),
                with_ids(
                    aged(gh("zebra/Copy", &["AuctionDB"]), 90),
                    "25159",
                    "336518",
                    "R4N",
                ),
                with_ids(aged(gh("zebra/Sleeper", &["S"]), 90), "7032", "2382", "own"),
                aged(wowi(7032, "TomTom", &["TomTom"]), 90),
            ],
            &picks(&["mooreatv/AuctionDB"]),
            &[("25159", "mooreatv/AuctionDB")],
        );
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("25159"));
        assert_eq!(out[0].addon.ids.curse.as_deref(), Some("336518"));
        assert!(out[1].addon.ids.is_empty(), "a copy of the real one's ids");
        assert_eq!(out[2].addon.ids.wowi, None, "the listing owns its own id");
        assert_eq!(out[2].addon.ids.curse.as_deref(), Some("2382"));
        assert_eq!(out[2].addon.ids.wago.as_deref(), Some("own"));
        assert_eq!(out[3].addon.ids.wowi.as_deref(), Some("7032"));
    }

    fn info(name: &str) -> RepoInfo {
        RepoInfo {
            name: name.into(),
            ..RepoInfo::default()
        }
    }

    #[test]
    fn junk_repositories_are_rejected() {
        assert_eq!(repo_rejection(&info("a/Good"), false), None);
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    archived: true,
                    ..info("a/b")
                },
                false
            ),
            Some("archived")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    template: true,
                    ..info("a/b")
                },
                false
            ),
            Some("template")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    empty: true,
                    ..info("a/b")
                },
                false
            ),
            Some("empty")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    mirror: true,
                    ..info("a/b")
                },
                false
            ),
            Some("mirror")
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    disabled: true,
                    ..info("a/b")
                },
                false
            ),
            Some("disabled")
        );
        assert!(repo_rejection(&info("andi-74/foreverdungeonjournal-mirror"), false).is_some());
        assert!(repo_rejection(&info("someone/wow-addon-template"), false).is_some());
        assert_eq!(
            repo_rejection(&info("a/Templates4Life"), false),
            None,
            "whole words only"
        );
    }

    #[test]
    fn hand_picked_repositories_survive_being_archived_but_not_being_empty() {
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    archived: true,
                    ..info("a/b")
                },
                true
            ),
            None
        );
        assert_eq!(
            repo_rejection(&info("a/some-template-looking-name"), true),
            None
        );
        assert_eq!(
            repo_rejection(
                &RepoInfo {
                    empty: true,
                    ..info("a/b")
                },
                true
            ),
            Some("empty")
        );
    }

    #[test]
    fn placeholder_names_are_recognized() {
        for name in [
            "MyAddon",
            "My Addon",
            "Addon Template",
            "Hello World",
            "",
            "@",
        ] {
            assert!(is_placeholder_name(name), "{name}");
        }
        for name in ["Questie", "Testament", "Foobar Deluxe"] {
            assert!(!is_placeholder_name(name), "{name}");
        }
    }
}
