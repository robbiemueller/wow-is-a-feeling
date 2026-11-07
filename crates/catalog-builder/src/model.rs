//! The types that move through the pipeline and live in the state file.

use std::collections::BTreeSet;

use feeling_core::catalog::{Addon, ExternalIds};
use serde::{Deserialize, Serialize};

use crate::gate::is_safe_folder;

/// What the TOCs inside one zip say. Cached, so a release is only read once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZipScan {
    /// Top-level folders with a TOC the client would load. The main folder comes first.
    pub folders: Vec<String>,
    /// True when at least one of those folders targets Forever.
    pub forever: bool,
    /// The rest describes the main folder's TOC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wowi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wago: Option<String>,
}

/// Required addons kept per TOC. The most any real addon lists is 8.
pub const MAX_DEPS: usize = 64;
/// Former main folders an entry keeps in `formerly`, the most recent. An addon renames its
/// main folder once or twice in its life, and every player's catalog carries the list.
pub const MAX_FORMERLY: usize = 8;
/// Real ids are 8 bytes or fewer, so nothing longer is one.
const MAX_ID_BYTES: usize = 32;

impl ZipScan {
    /// External ids from the TOC, keeping only values that look like real ids.
    /// Packagers can leave tokens such as `@wowi-id@` behind.
    pub fn external_ids(&self) -> ExternalIds {
        ExternalIds {
            curse: self.curse.clone().filter(|v| is_all_digits(v)),
            wowi: self.wowi.clone().filter(|v| is_all_digits(v)),
            wago: self
                .wago
                .clone()
                .filter(|v| is_short_id(v) && v.chars().all(|c| c.is_ascii_alphanumeric())),
        }
    }

    /// Required addons that the zip doesn't already bring along.
    pub fn external_deps(&self) -> Vec<String> {
        self.deps
            .iter()
            .filter(|dep| is_safe_folder(dep))
            .filter(|dep| !self.folders.iter().any(|f| f.eq_ignore_ascii_case(dep)))
            .take(MAX_DEPS)
            .cloned()
            .collect()
    }
}

/// Short enough to be an id on another site at all.
pub fn is_short_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES
}

fn is_all_digits(value: &str) -> bool {
    is_short_id(value) && value.bytes().all(|b| b.is_ascii_digit())
}

/// An addon kept out of the catalog, and why, for a maintainer to look at. Published as
/// `catalog-held.json`. Dates and counts stay out, so the file only changes when a
/// decision does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    pub id: String,
    pub name: String,
    pub folders: Vec<String>,
    pub reason: String,
}

impl Held {
    pub fn new(addon: &Addon, reason: String) -> Self {
        Self {
            id: addon.id.clone(),
            name: addon.name.clone(),
            folders: addon.folders.iter().take(16).cloned().collect(),
            reason,
        }
    }
}

/// One addon on its way into the catalog, with the signals that aren't part of
/// the published schema but decide its category and tier.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub addon: Addon,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toc_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wowi_category: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// WoWInterface favorites.
    #[serde(default)]
    pub favorites: u64,
    /// The repository this one was forked from, when it is a fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<String>,
    /// A newer release of the addon exists and doesn't target Forever any more.
    #[serde(default)]
    pub no_longer_forever: bool,
}

impl Candidate {
    /// Lowercased folder names, for comparing what two addons would install.
    pub fn folder_set(&self) -> BTreeSet<String> {
        self.addon
            .folders
            .iter()
            .map(|f| f.to_ascii_lowercase())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_dependencies_from_old_cached_scans_are_bounded_too() {
        let long = "1".repeat(MAX_ID_BYTES + 1);
        let scan = ZipScan {
            folders: vec!["Foo".into()],
            wowi: Some(long.clone()),
            curse: Some(long.clone()),
            wago: Some(long),
            deps: std::iter::once("../evil".to_string())
                .chain(std::iter::once("Foo".to_string()))
                .chain((0..MAX_DEPS + 10).map(|i| format!("Lib{i}")))
                .collect(),
            ..ZipScan::default()
        };
        assert!(scan.external_ids().is_empty());
        let deps = scan.external_deps();
        assert_eq!(deps.len(), MAX_DEPS);
        assert_eq!(deps[0], "Lib0", "an unsafe name is no dependency at all");

        let real = ZipScan {
            wowi: Some("5086".into()),
            curse: Some("1725334".into()),
            wago: Some("kGr9ReKy".into()),
            ..ZipScan::default()
        };
        let ids = real.external_ids();
        assert_eq!(ids.wowi.as_deref(), Some("5086"));
        assert_eq!(ids.curse.as_deref(), Some("1725334"));
        assert_eq!(ids.wago.as_deref(), Some("kGr9ReKy"));
    }
}
