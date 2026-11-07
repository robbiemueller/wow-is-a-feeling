//! The types that move through the pipeline and live in the state file.

use std::collections::BTreeSet;

use feeling_core::catalog::{Addon, ExternalIds};
use serde::{Deserialize, Serialize};

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
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric())),
        }
    }

    /// Required addons that the zip doesn't already bring along.
    pub fn external_deps(&self) -> Vec<String> {
        self.deps
            .iter()
            .filter(|dep| !self.folders.iter().any(|f| f.eq_ignore_ascii_case(dep)))
            .cloned()
            .collect()
    }
}

fn is_all_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
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
