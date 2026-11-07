//! Combines the sources into one list without duplicates, and throws out repos
//! that aren't really addons of their own. Entries only merge on real evidence:
//! a matching `X-WoWI-ID` or an identical folder set, never a matching name, because
//! same-named different projects exist (`ForeverPlus`, `ForeverUI`).

use std::collections::{BTreeMap, BTreeSet};

use crate::github::RepoInfo;
use crate::model::Candidate;
use crate::text::{normalize, tokens};
use crate::tier::popularity;

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

/// Forks exist for many reasons, but one that installs the same folders as a repo
/// we already list adds nothing but confusion, and its downloads go to the wrong place.
pub fn drop_redundant_forks(candidates: Vec<Candidate>) -> (Vec<Candidate>, Vec<String>) {
    let originals: BTreeMap<String, BTreeSet<String>> = candidates
        .iter()
        .filter(|c| c.fork_of.is_none())
        .map(|c| (c.addon.id.to_ascii_lowercase(), c.folder_set()))
        .collect();
    let all_sets: Vec<(String, BTreeSet<String>, u32)> = candidates
        .iter()
        .map(|c| (c.addon.id.clone(), c.folder_set(), c.addon.stars))
        .collect();

    let mut kept = Vec::with_capacity(candidates.len());
    let mut dropped = Vec::new();
    for candidate in candidates {
        let redundant = candidate.fork_of.as_ref().is_some_and(|parent| {
            let set = candidate.folder_set();
            let parent_id = format!("gh:{}", parent.to_ascii_lowercase());
            let same_as_parent = originals
                .get(&parent_id)
                .is_some_and(|parent_set| *parent_set == set);
            // The parent may be missing or be a fork itself. Any better-known repo with the same folders counts.
            let same_as_better = all_sets.iter().any(|(id, other, stars)| {
                *id != candidate.addon.id
                    && *other == set
                    && (originals.contains_key(&id.to_ascii_lowercase())
                        || *stars > candidate.addon.stars)
            });
            same_as_parent || same_as_better
        });
        if redundant {
            dropped.push(candidate.addon.id);
        } else {
            kept.push(candidate);
        }
    }
    (kept, dropped)
}

/// Copies that aren't GitHub forks: same display name, same main folder, in another
/// repository. Installing one would overwrite the other, and a player can't tell
/// them apart. The hand-picked or best-known one stays. Different projects that
/// happen to share a folder keep different names, so they aren't touched. Only
/// repositories compete here, because WoWInterface entries merge on stricter rules.
pub fn drop_copies(
    candidates: Vec<Candidate>,
    hand_picked: &BTreeSet<String>,
) -> (Vec<Candidate>, Vec<String>) {
    let identity = |c: &Candidate| {
        let main = c
            .addon
            .folders
            .first()
            .map_or_else(String::new, |f| f.to_ascii_lowercase());
        (main, normalize(&c.addon.name))
    };
    let is_picked = |c: &Candidate| {
        let repo = c.addon.id.strip_prefix("gh:").unwrap_or(&c.addon.id);
        hand_picked.contains(&repo.to_ascii_lowercase())
    };
    let is_repo = |c: &Candidate| c.addon.id.starts_with("gh:");
    let mut best: BTreeMap<(String, String), (bool, f64, String)> = BTreeMap::new();
    for candidate in candidates.iter().filter(|c| is_repo(c)) {
        let rank = (
            is_picked(candidate),
            popularity(candidate, 0.0),
            candidate.addon.id.clone(),
        );
        let slot = best
            .entry(identity(candidate))
            .or_insert_with(|| rank.clone());
        let better = rank
            .0
            .cmp(&slot.0)
            .then(rank.1.total_cmp(&slot.1))
            .then(slot.2.cmp(&rank.2));
        if better.is_gt() {
            *slot = rank;
        }
    }
    let mut kept = Vec::with_capacity(candidates.len());
    let mut dropped = Vec::new();
    for candidate in candidates {
        if !is_repo(&candidate) || best[&identity(&candidate)].2 == candidate.addon.id {
            kept.push(candidate);
        } else {
            dropped.push(candidate.addon.id);
        }
    }
    (kept, dropped)
}

/// Fold WoWInterface entries into GitHub ones where they're the same addon. GitHub
/// wins because its builds are versioned and carry digests. The WoWI id is kept
/// so an addon installed from there can still be matched.
pub fn merge_wowi(mut github: Vec<Candidate>, wowi: Vec<Candidate>) -> (Vec<Candidate>, usize) {
    let mut merged_count = 0;
    let mut standalone = Vec::new();
    for entry in wowi {
        let id = entry.addon.ids.wowi.clone().unwrap_or_default();
        let set = entry.folder_set();
        let by_toc_id = github
            .iter()
            .position(|g| !id.is_empty() && g.addon.ids.wowi.as_deref() == Some(id.as_str()));
        let by_folders = || {
            github
                .iter()
                .enumerate()
                .filter(|(_, g)| g.folder_set() == set && !set.is_empty())
                .max_by_key(|(_, g)| g.addon.stars)
                .map(|(at, _)| at)
        };
        match by_toc_id.or_else(by_folders) {
            Some(at) => {
                let target = &mut github[at];
                target.addon.ids.wowi = Some(id);
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
            None => standalone.push(entry),
        }
    }
    github.append(&mut standalone);
    (github, merged_count)
}

#[cfg(test)]
mod tests {
    use feeling_core::catalog::ExternalIds;

    use super::*;

    fn gh(repo: &str, folders: &[&str]) -> Candidate {
        let mut c = Candidate::default();
        c.addon.id = format!("gh:{repo}");
        c.addon.name = repo.split('/').nth(1).unwrap().into();
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c
    }

    fn wowi(id: u64, name: &str, folders: &[&str]) -> Candidate {
        let mut c = Candidate::default();
        c.addon.id = format!("wowi:{id}");
        c.addon.name = name.into();
        c.addon.folders = folders.iter().map(|f| f.to_string()).collect();
        c.addon.ids = ExternalIds {
            wowi: Some(id.to_string()),
            ..ExternalIds::default()
        };
        c
    }

    #[test]
    fn merges_on_a_toc_id_even_when_folders_differ() {
        let mut github = gh("funkydude/BugSack", &["BugSack", "!BugGrabber"]);
        github.addon.ids.wowi = Some("5995".into());
        github.addon.downloads = 100;
        let mut listing = wowi(5995, "BugSack (WoWI)", &["BugSack"]);
        listing.addon.downloads = 900;
        listing.favorites = 3;
        let (out, merged) = merge_wowi(vec![github], vec![listing]);
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
    fn merges_on_identical_folder_sets_ignoring_case_and_order() {
        let github = gh("Nevcairiel/Mapster", &["Mapster", "Mapster_Options"]);
        let listing = wowi(
            1234,
            "Something Else Entirely",
            &["mapster_options", "MAPSTER"],
        );
        let (out, merged) = merge_wowi(vec![github], vec![listing]);
        assert_eq!(merged, 1);
        assert_eq!(out[0].addon.ids.wowi.as_deref(), Some("1234"));
    }

    #[test]
    fn same_name_alone_never_merges() {
        let github = gh("7hedubz/ForeverPlus", &["ForeverPlus_GH"]);
        let listing = wowi(77, "ForeverPlus", &["ForeverPlus"]);
        let (out, merged) = merge_wowi(vec![github], vec![listing]);
        assert_eq!(merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn overlapping_folders_are_not_identical_folders() {
        let github = gh("a/One", &["Shared", "OneOnly"]);
        let listing = wowi(1, "Two", &["Shared", "TwoOnly"]);
        let (out, merged) = merge_wowi(vec![github], vec![listing]);
        assert_eq!(merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn an_empty_folder_set_never_merges() {
        let (out, merged) = merge_wowi(vec![gh("a/One", &[])], vec![wowi(1, "Two", &[])]);
        assert_eq!(merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn the_most_starred_repo_takes_a_folder_match() {
        let mut small = gh("someone/Copy", &["Foo"]);
        small.addon.stars = 1;
        let mut big = gh("author/Foo", &["Foo"]);
        big.addon.stars = 50;
        let (out, _) = merge_wowi(vec![small, big], vec![wowi(9, "Foo", &["Foo"])]);
        let winner = out.iter().find(|c| c.addon.ids.wowi.is_some()).unwrap();
        assert_eq!(winner.addon.id, "gh:author/Foo");
    }

    #[test]
    fn a_toc_id_match_beats_a_folder_match() {
        let same_folders = gh("a/Clone", &["Foo"]);
        let mut declared = gh("b/Real", &["Foo", "FooExtra"]);
        declared.addon.ids.wowi = Some("9".into());
        let (out, _) = merge_wowi(vec![same_folders, declared], vec![wowi(9, "Foo", &["Foo"])]);
        let winner = out.iter().find(|c| c.addon.ids.wowi.is_some()).unwrap();
        assert_eq!(winner.addon.id, "gh:b/Real");
    }

    #[test]
    fn forks_with_the_same_folders_as_the_parent_go_away() {
        let parent = gh("Up/Stream", &["Foo"]);
        let mut copy = gh("me/Stream", &["Foo"]);
        copy.fork_of = Some("Up/Stream".into());
        let mut own_path = gh("you/Stream", &["FooReborn"]);
        own_path.fork_of = Some("Up/Stream".into());
        let (kept, dropped) = drop_redundant_forks(vec![parent, copy, own_path]);
        assert_eq!(dropped, ["gh:me/Stream"]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_fork_of_something_missing_can_stay_when_it_is_the_only_one() {
        let mut revived = gh("new/Keeper", &["Foo"]);
        revived.fork_of = Some("Gone/Abandoned".into());
        let (kept, dropped) = drop_redundant_forks(vec![revived]);
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
        let (kept, dropped) = drop_redundant_forks(vec![a, b]);
        assert_eq!(dropped, ["gh:b/Fork"]);
        assert_eq!(kept[0].addon.id, "gh:a/Fork");
    }

    #[test]
    fn copies_under_the_same_name_and_main_folder_go_away() {
        let mut real = gh("Questie/Questie", &["Questie", "QuestieDB"]);
        real.addon.name = "Questie".into();
        real.addon.downloads = 130_000;
        let mut port = gh("someone/Questie-Forever", &["Questie"]);
        port.addon.name = "Questie".into();
        port.addon.downloads = 4_000;
        let mut other = gh("third/FightClub", &["Questie"]);
        other.addon.name = "Olympus Fight Club".into();
        let (kept, dropped) = drop_copies(vec![port, real, other], &BTreeSet::new());
        assert_eq!(dropped, ["gh:someone/Questie-Forever"]);
        assert_eq!(kept.len(), 2, "a different name is a different project");
    }

    #[test]
    fn a_hand_picked_repo_survives_a_more_popular_copy() {
        let mut picked = gh("Tercioo/Details-Damage-Meter", &["Details"]);
        picked.addon.name = "Details!".into();
        let mut copy = gh("someone/Details", &["Details"]);
        copy.addon.name = "Details".into();
        copy.addon.downloads = 50_000;
        let hand_picked = BTreeSet::from(["tercioo/details-damage-meter".to_string()]);
        let (kept, dropped) = drop_copies(vec![copy, picked], &hand_picked);
        assert_eq!(dropped, ["gh:someone/Details"]);
        assert_eq!(kept[0].addon.id, "gh:Tercioo/Details-Damage-Meter");
    }

    #[test]
    fn a_wowi_entry_is_never_treated_as_a_copy() {
        let mut repo = gh("a/Foo", &["Foo", "FooExtra"]);
        repo.addon.name = "Foo".into();
        let listing = wowi(1, "Foo", &["Foo"]);
        let (kept, dropped) = drop_copies(vec![repo, listing], &BTreeSet::new());
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn equal_copies_are_settled_the_same_way_every_time() {
        let a = gh("a/One", &["Foo"]);
        let b = gh("b/Two", &["Foo"]);
        let mut a2 = a.clone();
        a2.addon.name = "Foo".into();
        let mut b2 = b.clone();
        b2.addon.name = "Foo".into();
        let (first, _) = drop_copies(vec![a2.clone(), b2.clone()], &BTreeSet::new());
        let (second, _) = drop_copies(vec![b2, a2], &BTreeSet::new());
        assert_eq!(first[0].addon.id, second[0].addon.id);
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
