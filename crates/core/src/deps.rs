//! Required-dependency resolution and removal warnings. Dependencies are folder
//! names, as TOCs declare them, and every comparison ignores case. `OptionalDeps`
//! are load-order hints only, so they're never followed here.

use std::collections::{HashMap, HashSet};

use crate::catalog::{Addon, Catalog, Channel};
use crate::scan::LocalFolder;
use crate::state::State;

/// Addons that ship inside the game client. TOCs can require them, but they never
/// live in `Interface/AddOns` and nobody can install them.
const GAME_ADDON_PREFIX: &str = "blizzard_";

/// The biggest pack, in bytes, that installs as another addon's dependency without the player
/// seeing its size first. A pack is an addon a reviewed entry lists under its `packs`, such as
/// a voice pack: those run to gigabytes, and any addon's TOC can require one of their folders.
/// A pack up to this size comes along whether or not it's reviewed, because the reviewed entry
/// that lists it vouches for it. Other dependencies aren't held to this, because the addon that
/// requires one won't load without it.
pub const BIGGEST_UNASKED_DOWNLOAD: u64 = 100_000_000;

/// Which catalog addons provide each folder.
#[derive(Debug, Clone, Default)]
pub struct FolderIndex {
    by_folder: HashMap<String, Vec<usize>>,
}

impl FolderIndex {
    pub fn build(catalog: &Catalog) -> Self {
        let mut by_folder: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, addon) in catalog.addons.iter().enumerate() {
            for folder in &addon.folders {
                let providers = by_folder.entry(folder.to_lowercase()).or_default();
                // A catalog can list one folder twice in different case.
                if providers.last() != Some(&i) {
                    providers.push(i);
                }
            }
        }
        Self { by_folder }
    }

    /// Indices into `catalog.addons` of every addon providing `folder`, in any case.
    pub fn providers(&self, folder: &str) -> &[usize] {
        self.by_folder
            .get(&folder.to_lowercase())
            .map_or(&[], Vec::as_slice)
    }
}

/// What installing an addon takes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Addon ids in install order: dependencies first, the requested addon last.
    /// Empty when the requested id isn't in the catalog.
    pub to_install: Vec<String>,
    /// Required folders that nothing installed provides and no trusted catalog entry does
    /// either. The player has to look for these themselves.
    pub missing: Vec<String>,
    /// Required folders whose pick is trusted but was never reviewed by a person, and isn't a
    /// pack of a known size up to [`BIGGEST_UNASKED_DOWNLOAD`], so it isn't installed by itself.
    /// The player can open it from the Requires line and decide.
    pub unreviewed: Vec<Unreviewed>,
    /// Required folders whose pick is a pack with a download over [`BIGGEST_UNASKED_DOWNLOAD`],
    /// reviewed or not, so it isn't installed by itself either. The player hears its size and
    /// decides.
    pub too_big: Vec<TooBig>,
}

/// A required addon left for the player to install, because nobody reviewed its entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreviewed {
    /// The addon whose TOC requires it, which is the page whose Requires line links to it.
    pub needed_by: String,
    /// The folder, spelled the way that TOC spells it.
    pub folder: String,
    /// The entry the app picked for the folder, the same one the Requires line shows.
    pub provider: String,
}

/// A required pack left for the player to install, because its download is big.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooBig {
    /// The addon whose TOC requires it, which is the page whose Requires line links to it.
    pub needed_by: String,
    /// The folder, spelled the way that TOC spells it.
    pub folder: String,
    /// The entry the app picked for the folder, the same one the Requires line shows.
    pub provider: String,
    /// Its download, in bytes, from the channel it would install from.
    pub size: u64,
}

/// Work out what installing `addon_id` needs, walking required dependencies
/// transitively. Folders in `installed_folders` count as satisfied, along with
/// anything they would have needed. The requested addon is always included, so the
/// same call serves a first install, an update and a repair. `channel` says which channel
/// each addon would install from, which decides how big its download is.
///
/// A dependency joins the plan only when its pick is reviewed as well as trusted: any addon
/// can ship a library's folder name long before anything needs it, and a trusted entry only
/// had to be old enough. The rest are left out and listed in `unreviewed`. A pick that's a
/// pack, which a reviewed entry lists under its `packs`, never joins when its download is over
/// [`BIGGEST_UNASKED_DOWNLOAD`], reviewed or not, because a voice pack can run to gigabytes:
/// those are listed in `too_big`, with their size. That's decided before `reviewed`, since the
/// catalog publishes big packs unreviewed. A pack whose download is known and up to that size
/// joins even when it's published unreviewed, as one is when only its other channel's build is
/// big: the reviewed entry's hand-written group vouches for it. Any other pick joins whatever its
/// size, because the addon that requires it won't load without it.
pub fn resolve<I, S>(
    addon_id: &str,
    catalog: &Catalog,
    index: &FolderIndex,
    installed_folders: I,
    channel: impl Fn(&str) -> Channel,
) -> Resolution
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut resolution = Resolution::default();
    let Some(root) = catalog.addons.iter().position(|a| a.id == addon_id) else {
        return resolution;
    };
    let installed = lowercased(installed_folders);
    // Folders the plan already provides. An addon only joins the plan by adding a
    // new one, so this is what stops cycles, and it stops a folder an addon ships
    // itself from pulling in another provider.
    let mut planned: HashSet<String> = HashSet::new();
    let mut seen_missing: HashSet<String> = HashSet::new();
    // Each addon's requirement left for the player is named once, whichever list it's in.
    let mut seen_left: HashSet<(usize, String)> = HashSet::new();

    plan_folders(&catalog.addons[root], &mut planned);
    // An explicit stack, because a chain in a bad catalog could be thousands deep.
    let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
    while let Some(top) = stack.last_mut() {
        let current = top.0;
        let addon = &catalog.addons[current];
        let Some(dep) = addon.deps.get(top.1) else {
            resolution.to_install.push(addon.id.clone());
            stack.pop();
            continue;
        };
        top.1 += 1;

        let folder = dep.to_lowercase();
        if folder.starts_with(GAME_ADDON_PREFIX)
            || installed.contains(&folder)
            || planned.contains(&folder)
        {
            continue;
        }
        let Some(provider) = best_provider(&folder, catalog, index) else {
            if seen_missing.insert(folder) {
                resolution.missing.push(dep.clone());
            }
            continue;
        };
        let pick = &catalog.addons[provider];
        let size = download_size(pick, channel(&pick.id));
        let pack = is_pack(&pick.id, catalog);
        let too_big = pack && size > BIGGEST_UNASKED_DOWNLOAD;
        // A pack's group on a reviewed entry vouches for it, when its size is known.
        let vouched_pack = pack && size > 0;
        if !too_big && (pick.reviewed || vouched_pack) {
            plan_folders(pick, &mut planned);
            stack.push((provider, 0));
        } else if seen_left.insert((current, folder)) {
            let needed_by = addon.id.clone();
            let provider = pick.id.clone();
            if too_big {
                resolution.too_big.push(TooBig {
                    needed_by,
                    folder: dep.clone(),
                    provider,
                    size,
                });
            } else {
                resolution.unreviewed.push(Unreviewed {
                    needed_by,
                    folder: dep.clone(),
                    provider,
                });
            }
        }
    }
    resolution
}

fn plan_folders(addon: &Addon, planned: &mut HashSet<String>) {
    planned.extend(addon.folders.iter().map(|f| f.to_lowercase()));
}

/// What installing this addon from `channel` downloads, in bytes. 0 when unknown, or when
/// there's no build, which an install then says for itself.
fn download_size(addon: &Addon, channel: Channel) -> u64 {
    addon.release(channel).map_or(0, |release| release.size)
}

/// Is this addon a pack, such as a voice pack: one that a reviewed entry lists under its
/// `packs`? Only a reviewed entry's groups count, because a person wrote those down.
fn is_pack(id: &str, catalog: &Catalog) -> bool {
    catalog.addons.iter().any(|addon| {
        addon.reviewed
            && addon
                .packs
                .iter()
                .any(|group| group.ids.iter().any(|pack| pack == id))
    })
}

/// The addon the app would pick for this folder: the trusted one whose main folder it is,
/// then the most downloaded trusted one. Ties keep catalog order. An untrusted entry is never
/// picked, even when nothing else provides the folder, because its folder list is only a
/// claim and its downloads can be inflated. [`resolve`] installs the pick by itself only when
/// it's reviewed too, or a pack small enough, and never when it's a big pack. The Requires line
/// links to it either way.
pub fn best_provider(folder: &str, catalog: &Catalog, index: &FolderIndex) -> Option<usize> {
    let folder = folder.to_lowercase();
    index
        .providers(&folder)
        .iter()
        .copied()
        .filter(|&i| catalog.addons[i].trusted)
        .min_by_key(|&i| {
            let addon = &catalog.addons[i];
            let is_main = addon
                .folders
                .first()
                .is_some_and(|main| main.to_lowercase() == folder);
            (!is_main, std::cmp::Reverse(addon.downloads), i)
        })
}

/// Names of addons left on disk that require a folder being removed, for a warning
/// before removal. Managed addons go by their record's name, the rest by TOC title
/// or folder name. Sorted, without duplicates.
pub fn dependents(
    folders_being_removed: &[String],
    state: &State,
    local_folders: &[LocalFolder],
) -> Vec<String> {
    let removed = lowercased(folders_being_removed);
    let owners = state.folder_owners();
    let managed = state.current().map(|record| &record.managed);

    let mut names: Vec<String> = Vec::new();
    for folder in local_folders {
        let lower = folder.name.to_lowercase();
        let needs_removed = folder
            .toc
            .deps
            .iter()
            .any(|dep| removed.contains(&dep.to_lowercase()));
        if removed.contains(&lower) || !needs_removed {
            continue;
        }
        let owner_names: Vec<String> = owners
            .get(&lower)
            .into_iter()
            .flatten()
            .filter_map(|id| managed?.get(id))
            .map(|addon| addon.name.clone())
            .collect();
        if owner_names.is_empty() {
            names.push(
                folder
                    .toc
                    .title
                    .clone()
                    .unwrap_or_else(|| folder.name.clone()),
            );
        } else {
            names.extend(owner_names);
        }
    }
    names.sort_by_cached_key(|name| name.to_lowercase());
    names.dedup();
    names
}

fn lowercased<I, S>(names: I) -> HashSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .map(|name| name.as_ref().to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{PackGroup, Release};
    use crate::state::ManagedAddon;
    use crate::toc;

    const NOTHING: [&str; 0] = [];

    /// A hand-picked entry, which is trusted and reviewed.
    fn addon(id: &str, folders: &[&str], deps: &[&str]) -> Addon {
        Addon {
            id: id.to_string(),
            name: format!("Name of {id}"),
            folders: folders.iter().map(|f| f.to_string()).collect(),
            deps: deps.iter().map(|d| d.to_string()).collect(),
            trusted: true,
            reviewed: true,
            ..Default::default()
        }
    }

    fn untrusted(id: &str, folders: &[&str], downloads: u64) -> Addon {
        Addon {
            downloads,
            trusted: false,
            reviewed: false,
            ..addon(id, folders, &[])
        }
    }

    /// Trusted for being old enough or moderated elsewhere, but no person here chose it.
    fn unreviewed(id: &str, folders: &[&str], deps: &[&str]) -> Addon {
        Addon {
            reviewed: false,
            ..addon(id, folders, deps)
        }
    }

    fn left_out(needed_by: &str, folder: &str, provider: &str) -> Unreviewed {
        Unreviewed {
            needed_by: needed_by.to_string(),
            folder: folder.to_string(),
            provider: provider.to_string(),
        }
    }

    fn catalog(addons: Vec<Addon>) -> Catalog {
        Catalog {
            addons,
            ..Default::default()
        }
    }

    fn resolve_in(catalog: &Catalog, id: &str, installed: &[&str]) -> Resolution {
        resolve(id, catalog, &FolderIndex::build(catalog), installed, |_| {
            Channel::Stable
        })
    }

    /// A pick with a stable build of this many bytes.
    fn sized(addon: Addon, size: u64) -> Addon {
        Addon {
            stable: Some(Release {
                version: "1.0".to_string(),
                size,
                ..Default::default()
            }),
            ..addon
        }
    }

    fn too_big(needed_by: &str, folder: &str, provider: &str, size: u64) -> TooBig {
        TooBig {
            needed_by: needed_by.to_string(),
            folder: folder.to_string(),
            provider: provider.to_string(),
            size,
        }
    }

    /// A hand-picked addon that lists these ids as its packs, the way Spoken lists its voices.
    fn with_packs(id: &str, folder: &str, packs: &[&str]) -> Addon {
        Addon {
            packs: vec![PackGroup {
                name: "Voices".to_string(),
                needed: true,
                ids: packs.iter().map(|p| p.to_string()).collect(),
                met_by: Vec::new(),
            }],
            ..addon(id, &[folder], &[])
        }
    }

    fn local(name: &str, toc_text: &str) -> LocalFolder {
        LocalFolder {
            name: name.to_string(),
            toc_file: Some(format!("{name}.toc")),
            toc: toc::parse(toc_text),
            ..Default::default()
        }
    }

    fn state_with(addons: &[(&str, &str, &[&str])]) -> State {
        let mut state = State {
            current_install: Some("wow".to_string()),
            ..Default::default()
        };
        let record = state.current_mut().unwrap();
        for (id, name, folders) in addons {
            record.managed.insert(
                id.to_string(),
                ManagedAddon {
                    id: id.to_string(),
                    name: name.to_string(),
                    folders: folders.iter().map(|f| f.to_string()).collect(),
                    ..Default::default()
                },
            );
        }
        state
    }

    fn removed(folders: &[&str]) -> Vec<String> {
        folders.iter().map(|f| f.to_string()).collect()
    }

    // ---- FolderIndex ----

    #[test]
    fn the_index_ignores_case_and_lists_every_provider() {
        let catalog = catalog(vec![
            addon("a", &["Ace3", "LibStub"], &[]),
            addon("b", &["LIBSTUB"], &[]),
        ]);
        let index = FolderIndex::build(&catalog);
        assert_eq!(index.providers("libstub"), [0, 1]);
        assert_eq!(index.providers("ACE3"), [0]);
        assert!(index.providers("nope").is_empty());
    }

    #[test]
    fn a_folder_listed_twice_by_one_addon_counts_once() {
        let catalog = catalog(vec![addon("a", &["Foo", "FOO"], &[])]);
        assert_eq!(FolderIndex::build(&catalog).providers("foo"), [0]);
    }

    // ---- resolve ----

    #[test]
    fn walks_transitive_dependencies_deepest_first() {
        let catalog = catalog(vec![
            addon("top", &["Top"], &["Middle"]),
            addon("middle", &["Middle"], &["Bottom"]),
            addon("bottom", &["Bottom"], &[]),
            addon("unrelated", &["Unrelated"], &[]),
        ]);
        let result = resolve_in(&catalog, "top", &[]);
        assert_eq!(result.to_install, ["bottom", "middle", "top"]);
        assert!(result.missing.is_empty());
    }

    #[test]
    fn an_addon_with_no_dependencies_is_just_itself() {
        let catalog = catalog(vec![addon("solo", &["Solo"], &[])]);
        assert_eq!(resolve_in(&catalog, "solo", &[]).to_install, ["solo"]);
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        let catalog = catalog(vec![addon("solo", &["Solo"], &[])]);
        assert_eq!(
            resolve_in(&catalog, "gh:no/such", &[]),
            Resolution::default()
        );
    }

    #[test]
    fn cycles_terminate_and_list_each_addon_once() {
        let catalog = catalog(vec![
            addon("a", &["A"], &["B"]),
            addon("b", &["B"], &["C"]),
            addon("c", &["C"], &["A", "B"]),
        ]);
        let result = resolve_in(&catalog, "a", &[]);
        assert_eq!(result.to_install, ["c", "b", "a"]);
        assert!(result.missing.is_empty());
    }

    #[test]
    fn a_self_dependency_terminates() {
        let catalog = catalog(vec![addon("a", &["A"], &["A", "a"])]);
        assert_eq!(resolve_in(&catalog, "a", &[]).to_install, ["a"]);
    }

    #[test]
    fn a_very_deep_chain_does_not_overflow_the_stack() {
        let depth = 50_000;
        let addons: Vec<Addon> = (0..depth)
            .map(|i| Addon {
                id: format!("id{i}"),
                folders: vec![format!("F{i}")],
                deps: if i + 1 < depth {
                    vec![format!("F{}", i + 1)]
                } else {
                    vec![]
                },
                trusted: true,
                reviewed: true,
                ..Default::default()
            })
            .collect();
        let catalog = catalog(addons);
        let result = resolve_in(&catalog, "id0", &[]);
        assert_eq!(result.to_install.len(), depth);
        assert_eq!(result.to_install[0], format!("id{}", depth - 1));
        assert_eq!(result.to_install[depth - 1], "id0");
    }

    #[test]
    fn skips_what_is_installed_and_everything_behind_it() {
        let catalog = catalog(vec![
            addon("top", &["Top"], &["Middle", "Other"]),
            addon("middle", &["Middle"], &["Bottom"]),
            addon("bottom", &["Bottom"], &[]),
            addon("other", &["Other"], &[]),
        ]);
        let result = resolve_in(&catalog, "top", &["MIDDLE"]);
        assert_eq!(result.to_install, ["other", "top"]);
    }

    #[test]
    fn the_requested_addon_is_kept_even_when_installed() {
        let catalog = catalog(vec![
            addon("top", &["Top"], &["New"]),
            addon("new", &["New"], &[]),
        ]);
        assert_eq!(
            resolve_in(&catalog, "top", &["Top"]).to_install,
            ["new", "top"]
        );
    }

    #[test]
    fn missing_folders_are_reported_once_with_their_spelling() {
        let catalog = catalog(vec![
            addon("a", &["A"], &["Gone", "B"]),
            addon("b", &["B"], &["GONE", "AlsoGone"]),
        ]);
        let result = resolve_in(&catalog, "a", &[]);
        assert_eq!(result.to_install, ["b", "a"]);
        assert_eq!(result.missing, ["Gone", "AlsoGone"]);
    }

    #[test]
    fn matching_ignores_case_everywhere() {
        let catalog = catalog(vec![
            addon("a", &["A"], &["LIBSTUB", "ace3"]),
            addon("lib", &["LibStub"], &[]),
            addon("ace", &["Ace3"], &[]),
        ]);
        assert_eq!(
            resolve_in(&catalog, "a", &["libstub"]).to_install,
            ["ace", "a"]
        );
    }

    #[test]
    fn an_addons_own_folders_satisfy_its_dependencies() {
        let catalog = catalog(vec![
            addon(
                "bigwigs",
                &["BigWigs", "BigWigs_Core"],
                &["BigWigs_Core", "LibStub"],
            ),
            addon("other", &["BigWigs_Core"], &[]),
            addon("lib", &["LibStub"], &[]),
        ]);
        assert_eq!(
            resolve_in(&catalog, "bigwigs", &[]).to_install,
            ["lib", "bigwigs"]
        );
    }

    #[test]
    fn a_folder_already_in_the_plan_does_not_pull_another_provider() {
        let catalog = catalog(vec![
            addon("a", &["A"], &["Bundle", "Lib"]),
            addon("bundle", &["Bundle", "Lib"], &[]),
            addon("lib", &["Lib"], &[]),
        ]);
        assert_eq!(resolve_in(&catalog, "a", &[]).to_install, ["bundle", "a"]);
    }

    #[test]
    fn the_main_folder_owner_is_preferred_then_the_most_downloaded() {
        let mut embedded = addon("embeds", &["Big", "LibStub"], &[]);
        embedded.downloads = 1_000_000;
        let mut small = addon("small", &["LIBSTUB"], &[]);
        small.downloads = 5;
        let mut smaller = addon("smaller", &["LibStub"], &[]);
        smaller.downloads = 1;
        let root = addon("a", &["A"], &["LibStub"]);
        let catalog1 = catalog(vec![root.clone(), embedded.clone(), smaller.clone(), small]);
        assert_eq!(resolve_in(&catalog1, "a", &[]).to_install, ["small", "a"]);

        // With no main-folder owner at all, downloads decide.
        let mut also_embeds = addon("also", &["Other", "LibStub"], &[]);
        also_embeds.downloads = 2_000_000;
        let catalog2 = catalog(vec![root, embedded, also_embeds]);
        assert_eq!(resolve_in(&catalog2, "a", &[]).to_install, ["also", "a"]);
    }

    #[test]
    fn an_untrusted_entry_is_never_picked_even_when_nothing_else_provides_the_folder() {
        // Nobody else lists ElvUI, so anyone could. It counts as missing, and the player
        // goes looking for it.
        let catalog = catalog(vec![
            addon("plugin", &["ElvUI_WindTools"], &["ElvUI"]),
            untrusted("gh:free-account/ElvUI", &["ElvUI"], 1),
        ]);
        let result = resolve_in(&catalog, "plugin", &[]);
        assert_eq!(result.to_install, ["plugin"]);
        assert_eq!(result.missing, ["ElvUI"]);
    }

    #[test]
    fn downloads_never_lift_an_untrusted_entry_over_a_trusted_one() {
        let root = addon("a", &["A"], &["LibStub"]);
        let stranger = untrusted("stranger", &["LibStub"], 9_000_000);
        let mut embeds = addon("embeds", &["Big", "LibStub"], &[]);
        embeds.downloads = 5;
        let catalog = catalog(vec![root, stranger, embeds]);
        assert_eq!(resolve_in(&catalog, "a", &[]).to_install, ["embeds", "a"]);
        assert_eq!(
            best_provider("LIBSTUB", &catalog, &FolderIndex::build(&catalog)),
            Some(2)
        );
    }

    #[test]
    fn a_pick_nobody_reviewed_is_left_for_the_player_and_says_who_needs_it() {
        // Any repo can ship LibThing long before an addon needs it, then be old enough to trust.
        let catalog = catalog(vec![
            addon("top", &["Top"], &["Middle", "LibThing"]),
            addon("middle", &["Middle"], &["LIBDEEP"]),
            unreviewed("squatter", &["LibThing", "LibDeep"], &["Gone"]),
        ]);
        let result = resolve_in(&catalog, "top", &[]);
        assert_eq!(result.to_install, ["middle", "top"]);
        assert_eq!(
            result.unreviewed,
            [
                left_out("middle", "LIBDEEP", "squatter"),
                left_out("top", "LibThing", "squatter"),
            ],
            "whose page to look on, and nothing it would have needed"
        );
        assert!(result.missing.is_empty());
        // The same pick an install makes, so the Requires line can still link to it.
        assert_eq!(
            best_provider("libthing", &catalog, &FolderIndex::build(&catalog)),
            Some(2)
        );
    }

    #[test]
    fn an_unreviewed_pick_is_named_once_per_folder_and_never_for_what_is_covered() {
        let catalog = catalog(vec![
            addon(
                "top",
                &["Top", "Bundled"],
                &["Lib", "LIB", "Bundled", "Installed"],
            ),
            unreviewed("lib", &["Lib"], &[]),
            unreviewed("bundled", &["Bundled"], &[]),
            unreviewed("installed", &["Installed"], &[]),
        ]);
        let result = resolve_in(&catalog, "top", &["installed"]);
        assert_eq!(result.to_install, ["top"]);
        assert_eq!(result.unreviewed, [left_out("top", "Lib", "lib")]);
    }

    #[test]
    fn a_reviewed_pick_still_installs_by_itself() {
        let mut lib = addon("lib", &["Lib"], &[]);
        lib.downloads = 1;
        let catalog = catalog(vec![addon("top", &["Top"], &["Lib"]), lib]);
        let result = resolve_in(&catalog, "top", &[]);
        assert_eq!(result.to_install, ["lib", "top"]);
        assert!(result.unreviewed.is_empty());
    }

    #[test]
    fn asking_for_an_untrusted_addon_still_installs_it_and_what_it_needs() {
        let mut asked_for = untrusted("stranger", &["Cool"], 1);
        asked_for.deps = vec!["Lib".to_string(), "Gone".to_string()];
        let catalog = catalog(vec![asked_for, addon("lib", &["Lib"], &[])]);
        let result = resolve_in(&catalog, "stranger", &[]);
        assert_eq!(result.to_install, ["lib", "stranger"]);
        assert_eq!(result.missing, ["Gone"]);
    }

    #[test]
    fn game_addons_are_never_missing_or_installed() {
        let catalog = catalog(vec![addon(
            "a",
            &["A"],
            &["Blizzard_InterfaceOptions", "blizzard_AuctionUI"],
        )]);
        let result = resolve_in(&catalog, "a", &[]);
        assert_eq!(result.to_install, ["a"]);
        assert!(result.missing.is_empty());
    }

    #[test]
    fn resolve_accepts_any_string_collection() {
        let catalog = catalog(vec![addon("a", &["A"], &["B"])]);
        let index = FolderIndex::build(&catalog);
        let stable = |_: &str| Channel::Stable;
        let owned = vec!["B".to_string()];
        assert!(
            resolve("a", &catalog, &index, &owned, stable)
                .missing
                .is_empty()
        );
        let owners: HashMap<String, Vec<String>> = HashMap::from([("b".to_string(), vec![])]);
        assert!(
            resolve("a", &catalog, &index, owners.keys(), stable)
                .missing
                .is_empty()
        );
        assert_eq!(
            resolve("a", &catalog, &index, NOTHING, stable).missing,
            ["B"]
        );
    }

    #[test]
    fn a_pack_over_the_size_limit_is_left_for_the_player_with_its_size_reviewed_or_not() {
        // Any addon's TOC can name a voice pack's folders. The catalog publishes the big packs
        // unreviewed, which says nothing about who made them.
        let catalog = catalog(vec![
            addon(
                "chatter",
                &["Chatter"],
                &["QuestsAudio", "Small", "Places", "Bosses"],
            ),
            with_packs("spoken", "Spoken", &["quests", "small", "places", "bosses"]),
            sized(addon("quests", &["QuestsAudio"], &[]), 1_479_966_451),
            sized(addon("small", &["Small"], &[]), BIGGEST_UNASKED_DOWNLOAD),
            sized(unreviewed("places", &["Places"], &[]), 456_216_109),
            sized(
                addon("bosses", &["Bosses"], &[]),
                BIGGEST_UNASKED_DOWNLOAD + 1,
            ),
        ]);
        let result = resolve_in(&catalog, "chatter", &[]);
        assert_eq!(
            result.to_install,
            ["small", "chatter"],
            "up to the limit is fine"
        );
        assert_eq!(
            result.too_big,
            [
                too_big("chatter", "QuestsAudio", "quests", 1_479_966_451),
                too_big("chatter", "Places", "places", 456_216_109),
                too_big("chatter", "Bosses", "bosses", BIGGEST_UNASKED_DOWNLOAD + 1),
            ],
            "reviewed or not, with the size the player is told"
        );
        assert!(result.unreviewed.is_empty() && result.missing.is_empty());
    }

    #[test]
    fn a_big_pick_that_is_no_pack_goes_by_review_alone() {
        // Musician's plugins require it, and WoW won't load them without it.
        let catalog = catalog(vec![
            addon("plugin", &["MusicianList"], &["Musician", "MapData"]),
            sized(addon("musician", &["Musician"], &[]), 101_799_121),
            sized(unreviewed("maps", &["MapData"], &[]), 1_350_000_000),
        ]);
        let result = resolve_in(&catalog, "plugin", &[]);
        assert_eq!(
            result.to_install,
            ["musician", "plugin"],
            "reviewed, whatever its size"
        );
        assert_eq!(
            result.unreviewed,
            [left_out("plugin", "MapData", "maps")],
            "and nobody reviewed this one, whatever its size"
        );
        assert!(result.too_big.is_empty());
    }

    #[test]
    fn only_a_reviewed_entrys_groups_make_a_pack() {
        let mut lister = with_packs("lister", "Lister", &["musician"]);
        lister.reviewed = false;
        let mut catalog = catalog(vec![
            addon("plugin", &["MusicianList"], &["Musician"]),
            sized(addon("musician", &["Musician"], &[]), 101_799_121),
            lister,
        ]);
        // An entry nobody reviewed has no say in what's a pack, so it can't keep a big
        // requirement away.
        assert_eq!(
            resolve_in(&catalog, "plugin", &[]).to_install,
            ["musician", "plugin"]
        );
        catalog.addons[2].reviewed = true;
        let result = resolve_in(&catalog, "plugin", &[]);
        assert_eq!(result.to_install, ["plugin"]);
        assert_eq!(
            result.too_big,
            [too_big("plugin", "Musician", "musician", 101_799_121)]
        );
    }

    #[test]
    fn a_big_pack_brings_nothing_it_would_have_needed_and_is_named_once() {
        let catalog = catalog(vec![
            addon("top", &["Top"], &["Pack", "PACK", "Middle"]),
            addon("middle", &["Middle"], &["Pack"]),
            sized(addon("pack", &["Pack"], &["PackLib"]), 2_000_000_000),
            addon("packlib", &["PackLib"], &[]),
            with_packs("owner", "Owner", &["pack"]),
        ]);
        let result = resolve_in(&catalog, "top", &[]);
        assert_eq!(result.to_install, ["middle", "top"]);
        assert_eq!(
            result.too_big,
            [
                too_big("top", "Pack", "pack", 2_000_000_000),
                too_big("middle", "Pack", "pack", 2_000_000_000),
            ],
            "each page whose Requires line links it"
        );
        // Already here, it's nothing to tell anyone about.
        assert!(resolve_in(&catalog, "top", &["pack"]).too_big.is_empty());
    }

    #[test]
    fn the_size_that_counts_is_the_build_its_channel_would_install() {
        // A small stable build and a big beta, so the catalog publishes the pack unreviewed.
        // The owner's group vouches for it, so it comes along while it's small.
        let mut pack = sized(unreviewed("pack", &["Pack"], &[]), 50_000_000);
        pack.beta = Some(Release {
            version: "2.0-beta".to_string(),
            size: 157_070_901,
            date: 1,
            ..Default::default()
        });
        let catalog = catalog(vec![
            addon("top", &["Top"], &["Pack"]),
            pack,
            with_packs("owner", "Owner", &["pack"]),
        ]);
        let index = FolderIndex::build(&catalog);
        let stable = resolve("top", &catalog, &index, NOTHING, |_| Channel::Stable);
        assert_eq!(stable.to_install, ["pack", "top"]);
        assert!(stable.unreviewed.is_empty() && stable.too_big.is_empty());
        let beta = resolve("top", &catalog, &index, NOTHING, |id: &str| {
            if id == "pack" {
                Channel::Beta
            } else {
                Channel::Stable
            }
        });
        assert_eq!(beta.to_install, ["top"]);
        assert_eq!(beta.too_big, [too_big("top", "Pack", "pack", 157_070_901)]);
        assert!(beta.unreviewed.is_empty());
    }

    #[test]
    fn a_pack_nobody_reviewed_comes_along_only_when_its_download_is_known_and_small() {
        let catalog = catalog(vec![
            addon("top", &["Top"], &["Small", "Unknown", "Library"]),
            sized(unreviewed("small", &["Small"], &[]), 5_000_000),
            unreviewed("unknown", &["Unknown"], &[]),
            sized(unreviewed("library", &["Library"], &[]), 5_000_000),
            with_packs("owner", "Owner", &["small", "unknown"]),
        ]);
        let result = resolve_in(&catalog, "top", &[]);
        assert_eq!(result.to_install, ["small", "top"]);
        assert_eq!(
            result.unreviewed,
            [
                left_out("top", "Unknown", "unknown"),
                left_out("top", "Library", "library"),
            ],
            "a pack of no known size, and anything that's no pack"
        );
        assert!(result.too_big.is_empty());
    }

    #[test]
    fn the_requested_addon_is_never_too_big_to_install() {
        // A big pack, as the catalog publishes one: not reviewed.
        let catalog = catalog(vec![
            sized(unreviewed("pack", &["Pack"], &[]), 2_000_000_000),
            with_packs("owner", "Owner", &["pack"]),
        ]);
        let result = resolve_in(&catalog, "pack", &[]);
        assert_eq!(result.to_install, ["pack"], "the player asked for it");
        assert!(result.too_big.is_empty() && result.unreviewed.is_empty());
    }

    // ---- dependents ----

    #[test]
    fn lists_remaining_addons_that_need_a_removed_folder() {
        let state = state_with(&[("gh:a/wa", "WeakAuras", &["WeakAuras", "WeakAurasOptions"])]);
        let folders = vec![
            local("Ace3", "## Title: Ace3\n"),
            local("WeakAuras", "## Dependencies: Ace3\n"),
            local("WeakAurasOptions", "## RequiredDeps: ACE3, WeakAuras\n"),
            local("MyAddon", "## Title: My Addon\n## Dep: ace3\n"),
            local("NoTitle", "## RequiredDeps: Ace3\n"),
            local("Unrelated", "## OptionalDeps: Ace3\n"),
        ];
        assert_eq!(
            dependents(&removed(&["Ace3"]), &state, &folders),
            ["My Addon", "NoTitle", "WeakAuras"]
        );
    }

    #[test]
    fn folders_also_being_removed_do_not_count() {
        let state = state_with(&[("dbm", "Deadly Boss Mods", &["DBM-Core", "DBM-GUI"])]);
        let folders = vec![
            local("DBM-Core", "## Title: DBM\n"),
            local("DBM-GUI", "## Dependencies: DBM-Core\n"),
        ];
        assert!(dependents(&removed(&["DBM-Core", "DBM-GUI"]), &state, &folders).is_empty());
        assert_eq!(
            dependents(&removed(&["DBM-Core"]), &state, &folders),
            ["Deadly Boss Mods"]
        );
    }

    #[test]
    fn a_shared_folder_names_every_owner() {
        let state = state_with(&[
            ("a", "First", &["Shared"]),
            ("b", "Second", &["Shared", "Other"]),
        ]);
        let folders = vec![local("Shared", "## Dependencies: Lib\n"), local("Lib", "")];
        assert_eq!(
            dependents(&removed(&["lib"]), &state, &folders),
            ["First", "Second"]
        );
    }

    #[test]
    fn nothing_depends_on_nothing() {
        let state = State::default();
        let folders = vec![local("A", "## Dependencies: B\n")];
        assert!(dependents(&[], &state, &folders).is_empty());
        assert!(dependents(&removed(&["B"]), &state, &[]).is_empty());
        assert!(dependents(&removed(&["C"]), &state, &folders).is_empty());
        // Without a chosen install, a folder is simply named by itself.
        assert_eq!(dependents(&removed(&["B"]), &state, &folders), ["A"]);
    }
}
