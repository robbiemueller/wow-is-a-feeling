//! The addon catalog: one small JSON file, built by CI and fetched by the app.
//!
//! The app makes no GitHub API calls to browse. It downloads this file (with an
//! ETag, so an unchanged catalog costs one 304) and then goes straight to each
//! addon's own download URL. Files are never mirrored.

use serde::{Deserialize, Serialize};

/// Bump when an old app could no longer read a new catalog.
pub const CATALOG_SCHEMA: u32 = 1;

/// Where the app fetches the catalog from.
pub const CATALOG_URL: &str =
    "https://raw.githubusercontent.com/robbiemueller/wow-is-a-feeling/catalog/catalog.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub schema: u32,
    /// Unix seconds when this catalog was built.
    pub generated: i64,
    pub addons: Vec<Addon>,
    /// Addon ids to show first, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub featured: Vec<String>,
    /// Hand-picked sets of addons a new player can install together.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kits: Vec<Kit>,
    /// Well-known addons we can't offer, and what to use instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Addon {
    /// Stable id: `gh:owner/repo`, `wowi:1234`, or `direct:name`. One repository that
    /// publishes several addons, each under its own tag prefix (`<line>/v1.2.3`), gives each
    /// extra one `gh:owner/repo/<line>`, with `<line>` lowercase `[a-z0-9-]`. `source` still
    /// names the plain repository.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// One of [`CATEGORIES`]' ids.
    pub category: String,
    pub source: Source,
    /// The addon's home on the web, for "open its page".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// Latest stable Forever build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable: Option<Release>,
    /// Latest pre-release Forever build, when newer than stable or when no stable exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta: Option<Release>,
    /// Folders this addon puts in `Interface/AddOns`. The first is its main folder.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Folder names of required addons, from the main TOC.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
    /// Total downloads as reported by the source.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub downloads: u64,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub stars: u32,
    /// 0 poor (grey), 1 common (white), 2 uncommon (green), 3 rare (blue),
    /// 4 epic (purple), 5 legendary (orange). Purely a popularity color.
    #[serde(default)]
    pub tier: u8,
    /// Ids on other sites, used to match addons already installed by other tools.
    #[serde(default, skip_serializing_if = "ExternalIds::is_empty")]
    pub ids: ExternalIds,
    /// Unix seconds when this addon first appeared, for the "Newest" sort. 0 when unknown.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub created: i64,
    /// Whether the app may pick this entry by itself: to manage a folder already on disk, or
    /// to fill a dependency. Set by the catalog builder for hand-picked and moderated entries
    /// and ones long established under the same name. A stranger's brand new repo isn't. Its
    /// folders, ids and name are claims, so it only gets installed when a player asks for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted: bool,
    /// Whether a person chose this entry: seeded, hand-picked or part of an approved pair.
    /// Only these get installed by themselves as another addon's dependency, because any
    /// addon can ship a library's folder name long before anything needs it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reviewed: bool,
    /// Screenshots, for a source whose details don't list them (WoWInterface). Written by a
    /// stranger, so the app filters these and fetches them with `Http::get_untrusted`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Addons that go with this one, such as the voice packs it plays, as groups that a
    /// person wrote down: an addon's own files never say. Only a `reviewed` entry's groups
    /// count, and the app installs none of them unless the player says so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<PackGroup>,
    /// Main folders this addon had before, that the catalog vouches for: a rename its author
    /// made, or a main folder it moved away from. An installed copy whose main folder is here
    /// is still this addon and gets its updates. A different addon from the same repository
    /// never gets in here, so an update can't turn an installed addon into it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formerly: Vec<String>,
}

/// One kind of pack for an addon: any one of `ids` will do.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackGroup {
    /// What the group is for, in a few words, such as "Quest voices".
    pub name: String,
    /// The addon does nothing useful until one of these is installed, as with Spoken and
    /// its quest voices. Otherwise each is a welcome extra.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needed: bool,
    /// Catalog ids, in the order to offer them. The first is the one offered by default.
    pub ids: Vec<String>,
    /// Folder name prefixes that meet the group too, for packs the catalog doesn't list:
    /// other languages, or ones from another tool. "SpokenQuestsAudio" meets quest voices in
    /// any language, so nobody is told their own voices are missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub met_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Github { repo: String },
    Wowi { id: u64 },
    Direct { url: String },
}

impl Default for Source {
    fn default() -> Self {
        Source::Direct { url: String::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// Bytes. 0 when unknown.
    #[serde(default)]
    pub size: u64,
    /// Unix seconds.
    #[serde(default)]
    pub date: i64,
    /// Lowercase hex, when the source publishes one. Verified after download.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// WoWInterface publishes MD5. Weak, but it still catches corrupt downloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// How zip paths map onto AddOns folders. Empty means each top-level
    /// folder in the zip is an addon folder, which covers packaged releases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<Move>,
}

/// Maps a path prefix inside the zip to a folder in `Interface/AddOns`.
/// Source-archive installs need this: GitHub wraps everything in `repo-tag/`,
/// and some addons split plugins out with `.pkgmeta` `move-folders`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Move {
    /// Path prefix inside the zip, with `/` separators and no trailing slash.
    /// A leading `*/` matches any single first segment, because the wrapper
    /// folder of a GitHub source archive is derived from the tag name.
    /// `*/` on its own matches everything inside that wrapper folder, for addons
    /// whose repository root is the addon folder (Details).
    /// When several moves match a path, the longest `from` wins.
    pub from: String,
    /// Folder name directly under `Interface/AddOns`.
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wowi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wago: Option<String>,
}

impl ExternalIds {
    pub fn is_empty(&self) -> bool {
        self.curse.is_none() && self.wowi.is_none() && self.wago.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Kit {
    pub name: String,
    pub description: String,
    pub addons: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gap {
    pub name: String,
    /// Why it's missing, in one short sentence.
    pub reason: String,
    /// Where its author publishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// Ids of addons in this catalog that do a similar job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
}

/// Browse categories, as `(id, display name)`, in display order.
pub const CATEGORIES: [(&str, &str); 13] = [
    ("quests", "Quests and Leveling"),
    ("bosses", "Bosses and Dungeons"),
    ("interface", "Interface"),
    ("maps", "Maps and Navigation"),
    ("bags", "Bags and Inventory"),
    ("auction", "Auction and Economy"),
    ("combat", "Combat"),
    ("nameplates", "Nameplates and Auras"),
    ("professions", "Professions and Gathering"),
    ("chat", "Chat and Social"),
    ("pvp", "Player vs. Player"),
    ("libraries", "Libraries and Tools"),
    ("misc", "Everything Else"),
];

/// Display name for a category id. Unknown ids fall back to "Everything Else",
/// so an old app can still read a catalog with a newer category.
pub fn category_name(id: &str) -> &'static str {
    CATEGORIES
        .iter()
        .find(|(cat, _)| *cat == id)
        .map_or("Everything Else", |(_, name)| name)
}

impl Addon {
    /// The release to install on the given channel. A stable-channel addon with
    /// no stable build falls back to beta, because some authors only ship
    /// Forever builds as pre-releases.
    pub fn release(&self, channel: Channel) -> Option<&Release> {
        match channel {
            Channel::Stable => self.stable.as_ref().or(self.beta.as_ref()),
            Channel::Beta => match (&self.stable, &self.beta) {
                (Some(stable), Some(beta)) if stable.date > beta.date => Some(stable),
                (_, Some(beta)) => Some(beta),
                (stable, None) => stable.as_ref(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str, date: i64) -> Release {
        Release {
            version: version.into(),
            url: format!("https://example.test/{version}.zip"),
            date,
            ..Default::default()
        }
    }

    #[test]
    fn channel_selection_falls_back_sensibly() {
        let mut addon = Addon {
            stable: Some(release("1.0", 100)),
            beta: Some(release("1.1-beta", 200)),
            ..Default::default()
        };
        assert_eq!(addon.release(Channel::Stable).unwrap().version, "1.0");
        assert_eq!(addon.release(Channel::Beta).unwrap().version, "1.1-beta");

        addon.beta = Some(release("0.9-beta", 50));
        assert_eq!(addon.release(Channel::Beta).unwrap().version, "1.0");

        addon.stable = None;
        assert_eq!(addon.release(Channel::Stable).unwrap().version, "0.9-beta");

        addon.beta = None;
        assert!(addon.release(Channel::Stable).is_none());
        assert!(addon.release(Channel::Beta).is_none());
    }

    #[test]
    fn minimal_entry_round_trips_and_stays_small() {
        let addon = Addon {
            id: "gh:Questie/Questie".into(),
            name: "Questie".into(),
            category: "quests".into(),
            source: Source::Github {
                repo: "Questie/Questie".into(),
            },
            stable: Some(release("v12.6.2", 1)),
            folders: vec!["Questie".into()],
            tier: 5,
            ..Default::default()
        };
        let json = serde_json::to_string(&addon).unwrap();
        assert!(
            !json.contains("stars"),
            "empty fields should be omitted: {json}"
        );
        assert!(
            !json.contains("\"ids\""),
            "empty fields should be omitted: {json}"
        );
        assert!(
            !json.contains("created"),
            "empty fields should be omitted: {json}"
        );
        let back: Addon = serde_json::from_str(&json).unwrap();
        assert_eq!(back, addon);
    }

    #[test]
    fn packs_round_trip_and_stay_out_of_entries_without_them() {
        let spoken = Addon {
            id: "gh:rusty-key/spoken-wow".into(),
            name: "Spoken".into(),
            category: "quests".into(),
            packs: vec![
                PackGroup {
                    name: "Quest voices".into(),
                    needed: true,
                    ids: vec!["gh:rusty-key/spoken-wow/quests-audio".into()],
                    met_by: Vec::new(),
                },
                PackGroup {
                    name: "Place voices".into(),
                    needed: false,
                    ids: vec!["gh:rusty-key/spoken-wow/places-audio".into()],
                    met_by: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let json = serde_json::to_string(&spoken).unwrap();
        assert!(json.contains(r#""packs":[{"name":"Quest voices","needed":true,"ids":"#));
        assert!(
            !json.contains(r#""needed":false"#),
            "an extra says nothing about needing: {json}"
        );
        let back: Addon = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spoken);

        let plain = serde_json::to_string(&Addon::default()).unwrap();
        assert!(!plain.contains("packs"), "{plain}");
        // What an app from before packs makes of a catalog with them: it reads the rest.
        let old: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(old["id"], "gh:rusty-key/spoken-wow");
    }

    #[test]
    fn unknown_fields_and_categories_are_tolerated() {
        let json = r#"{"schema":1,"generated":5,"future_field":true,"addons":[
            {"id":"wowi:1","name":"X","category":"brand-new","source":{"kind":"wowi","id":1},"also_new":1}
        ]}"#;
        let catalog: Catalog = serde_json::from_str(json).unwrap();
        assert_eq!(catalog.addons.len(), 1);
        assert_eq!(
            category_name(&catalog.addons[0].category),
            "Everything Else"
        );
    }
}
