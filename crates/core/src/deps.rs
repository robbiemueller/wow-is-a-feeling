//! Required-dependency resolution and removal warnings. Dependencies are folder
//! names, as TOCs declare them, and every comparison ignores case. `OptionalDeps`
//! are load-order hints only, so they're never followed here.

use std::collections::{HashMap, HashSet};

use crate::catalog::{Addon, Catalog};
use crate::scan::LocalFolder;
use crate::state::State;

/// Addons that ship inside the game client. TOCs can require them, but they never
/// live in `Interface/AddOns` and nobody can install them.
const GAME_ADDON_PREFIX: &str = "blizzard_";

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
    /// Required folders whose pick is trusted but was never reviewed by a person, so it
    /// isn't installed by itself. The player can open it from the Requires line and decide.
    pub unreviewed: Vec<Unreviewed>,
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

/// Work out what installing `addon_id` needs, walking required dependencies
/// transitively. Folders in `installed_folders` count as satisfied, along with
/// anything they would have needed. The requested addon is always included, so the
/// same call serves a first install, an update and a repair.
///
/// A dependency joins the plan only when its pick is reviewed as well as trusted: any addon
/// can ship a library's folder name long before anything needs it, and a trusted entry only
/// had to be old enough. The rest are left out and listed in `unreviewed`.
pub fn resolve<I, S>(
    addon_id: &str,
    catalog: &Catalog,
    index: &FolderIndex,
    installed_folders: I,
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
    let mut seen_unreviewed: HashSet<(usize, String)> = HashSet::new();

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
        match best_provider(&folder, catalog, index) {
            Some(provider) if catalog.addons[provider].reviewed => {
                plan_folders(&catalog.addons[provider], &mut planned);
                stack.push((provider, 0));
            }
            Some(provider) => {
                if seen_unreviewed.insert((current, folder)) {
                    resolution.unreviewed.push(Unreviewed {
                        needed_by: addon.id.clone(),
                        folder: dep.clone(),
                        provider: catalog.addons[provider].id.clone(),
                    });
                }
            }
            None => {
                if seen_missing.insert(folder) {
                    resolution.missing.push(dep.clone());
                }
            }
        }
    }
    resolution
}

fn plan_folders(addon: &Addon, planned: &mut HashSet<String>) {
    planned.extend(addon.folders.iter().map(|f| f.to_lowercase()));
}

/// The addon the app would pick for this folder: the trusted one whose main folder it is,
/// then the most downloaded trusted one. Ties keep catalog order. An untrusted entry is never
/// picked, even when nothing else provides the folder, because its folder list is only a
/// claim and its downloads can be inflated. [`resolve`] installs the pick by itself only when
/// it's reviewed too, and the Requires line links to it either way.
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
        resolve(id, catalog, &FolderIndex::build(catalog), installed)
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
        let owned = vec!["B".to_string()];
        assert!(resolve("a", &catalog, &index, &owned).missing.is_empty());
        let owners: HashMap<String, Vec<String>> = HashMap::from([("b".to_string(), vec![])]);
        assert!(
            resolve("a", &catalog, &index, owners.keys())
                .missing
                .is_empty()
        );
        assert_eq!(resolve("a", &catalog, &index, NOTHING).missing, ["B"]);
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
