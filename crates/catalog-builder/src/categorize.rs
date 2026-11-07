//! Puts each addon in one of `catalog::CATEGORIES`. The order of the signals runs
//! from most to least deliberate: a hand override, then what the author declared,
//! then what the addon is called, and only then words in its description.

use crate::curated::Override;
use crate::model::Candidate;
use crate::text::tokens;

/// Keywords by category, in priority order: an addon called "Raid Map" is about raids.
const KEYWORDS: [(&str, &[&str]); 12] = [
    (
        "libraries",
        &[
            "lib",
            "libs",
            "library",
            "libraries",
            "ace3",
            "libstub",
            "devtool",
            "devtools",
            "debug",
            "debugger",
        ],
    ),
    (
        "bosses",
        &[
            "boss",
            "bosses",
            "raid",
            "raids",
            "dungeon",
            "dungeons",
            "dbm",
            "bigwigs",
            "littlewigs",
            "encounter",
            "encounters",
            "mythic",
            "keystone",
            "delve",
            "delves",
            "gtfo",
        ],
    ),
    (
        "quests",
        &[
            "quest",
            "quests",
            "questing",
            "questie",
            "leveling",
            "levelling",
            "guide",
            "guides",
            "rxp",
            "restedxp",
            "talent",
            "talents",
            "trainer",
            "training",
            "achievement",
            "achievements",
        ],
    ),
    (
        "nameplates",
        &[
            "nameplate",
            "nameplates",
            "plates",
            "plater",
            "aura",
            "auras",
            "buff",
            "buffs",
            "debuff",
            "debuffs",
            "weakauras",
        ],
    ),
    (
        "maps",
        &[
            "map",
            "maps",
            "minimap",
            "mapster",
            "coords",
            "coordinates",
            "waypoint",
            "waypoints",
            "navigation",
            "tomtom",
            "compass",
            "handynotes",
            "pathfinding",
        ],
    ),
    (
        "bags",
        &[
            "bag",
            "bags",
            "bank",
            "inventory",
            "loot",
            "looter",
            "vendor",
            "junk",
            "mail",
            "postal",
            "pawn",
            "gear",
            "equipment",
        ],
    ),
    (
        "auction",
        &[
            "auction",
            "auctions",
            "auctioneer",
            "auctionator",
            "gold",
            "economy",
            "tsm",
            "tradeskillmaster",
            "market",
            "price",
            "prices",
            "goblin",
        ],
    ),
    (
        "professions",
        &[
            "profession",
            "professions",
            "tradeskill",
            "tradeskills",
            "gather",
            "gathering",
            "gathermate",
            "herbalism",
            "herb",
            "mining",
            "fishing",
            "crafting",
            "recipe",
            "recipes",
            "cooking",
            "skinning",
            "skillet",
            "milling",
            "prospecting",
            "disenchanting",
            "disenchant",
        ],
    ),
    (
        "pvp",
        &[
            "pvp",
            "arena",
            "arenas",
            "battleground",
            "battlegrounds",
            "honor",
            "capping",
        ],
    ),
    (
        "chat",
        &[
            "chat",
            "whisper",
            "whispers",
            "guild",
            "friends",
            "friendlist",
            "social",
            "emote",
            "emotes",
            "prat",
            "wim",
            "messenger",
        ],
    ),
    (
        "combat",
        &[
            "combat",
            "damage",
            "dps",
            "threat",
            "rotation",
            "healer",
            "heal",
            "healing",
            "healbot",
            "vuhdo",
            "tank",
            "cast",
            "castbar",
            "cooldown",
            "cooldowns",
            "gcd",
            "meter",
            "details",
            "recount",
            "skada",
            "decursive",
            "dispel",
            "clique",
            "hekili",
        ],
    ),
    (
        "interface",
        &[
            "ui",
            "interface",
            "bar",
            "bars",
            "actionbar",
            "actionbars",
            "bartender",
            "dominos",
            "unitframe",
            "unitframes",
            "frames",
            "tooltip",
            "tooltips",
            "skin",
            "skins",
            "font",
            "fonts",
            "theme",
            "hud",
            "panel",
            "titan",
            "databar",
            "masque",
            "elvui",
            "ellesmereui",
            "darkmode",
            "camera",
        ],
    ),
];

/// What a TOC's category means. Unknown or catch-all values give `None`, so the
/// next signal gets a say.
fn from_toc_category(category: &str) -> Option<&'static str> {
    let lower = category.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("librar") || has("development") || has("dev tool") {
        Some("libraries")
    } else if has("unit frame")
        || has("raid frame")
        || has("party frame")
        || has("action bar")
        || has("tooltip")
        || has("artwork")
        || has("overhaul")
    {
        Some("interface")
    } else if has("boss") || has("raid") || has("dungeon") {
        Some("bosses")
    } else if has("quest") || has("leveling") {
        Some("quests")
    } else if has("nameplate") || has("buff") || has("aura") {
        Some("nameplates")
    } else if has("map") {
        Some("maps")
    } else if has("bag") || has("inventory") || has("mail") {
        Some("bags")
    } else if has("auction") || has("economy") {
        Some("auction")
    } else if has("profession") || has("tradeskill") {
        Some("professions")
    } else if has("pvp") || has("arena") || has("battleground") {
        Some("pvp")
    } else if has("chat") || has("guild") || has("communication") {
        Some("chat")
    } else if has("combat") || has("class") {
        Some("combat")
    } else {
        None
    }
}

/// `Interface Enhancements` was the default on old addon sites, so it says less
/// than the addon's own name does. It only decides when nothing else can.
fn generic_interface_category(category: &str) -> bool {
    let lower = category.to_ascii_lowercase();
    lower.contains("interface")
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word == "ui")
}

/// WoWInterface's category ids that say something useful. Plug-ins, Miscellaneous,
/// Utility and the classic-era groupings don't, so they fall through to the name.
fn from_wowi_category(id: &str) -> Option<&'static str> {
    match id {
        "17" | "19" | "21" | "26" | "86" | "98" | "99" | "108" | "109" | "113" | "115" | "116"
        | "125" | "126" | "127" | "128" | "129" | "130" | "132" | "133" | "134" | "135" | "136"
        | "147" | "148" => Some("interface"),
        "18" => Some("quests"),
        "20" | "97" => Some("bags"),
        "22" => Some("nameplates"),
        "24" | "138" => Some("maps"),
        "25" | "39" | "56" | "57" | "58" | "59" | "60" | "61" | "62" | "63" | "64" | "112"
        | "120" | "149" | "150" | "151" | "152" | "157" => Some("combat"),
        "35" | "47" | "53" => Some("libraries"),
        "40" => Some("professions"),
        "45" => Some("bosses"),
        "55" | "95" => Some("chat"),
        "94" => Some("auction"),
        "96" => Some("pvp"),
        _ => None,
    }
}

fn from_keywords(words: &[String]) -> Option<&'static str> {
    KEYWORDS.iter().find_map(|(category, keys)| {
        words
            .iter()
            .any(|w| keys.contains(&w.as_str()))
            .then_some(*category)
    })
}

/// `Ace3`, `LibStub` and `LibSharedMedia-3.0` go in libraries whatever else they say.
fn is_library(candidate: &Candidate) -> bool {
    let name = candidate.addon.name.as_str();
    let looks_like_lib = |text: &str| {
        text.eq_ignore_ascii_case("ace3")
            || (text.len() > 3
                && text.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("lib"))
                && text[3..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_uppercase() || c == '-' || c == '_'))
    };
    looks_like_lib(name)
        || candidate
            .addon
            .folders
            .first()
            .is_some_and(|f| looks_like_lib(f))
}

pub fn categorize(candidate: &Candidate, overrides: Option<&Override>) -> String {
    if let Some(category) = overrides.and_then(|o| o.category.as_deref()) {
        return category.to_string();
    }
    if is_library(candidate) {
        return "libraries".to_string();
    }
    let from_name = || from_keywords(&tokens(&candidate.addon.name));
    let from_topics = || {
        let words: Vec<String> = candidate.topics.iter().flat_map(|t| tokens(t)).collect();
        from_keywords(&words)
    };
    let from_summary = || from_keywords(&tokens(&candidate.addon.summary));
    let generic = || {
        candidate
            .toc_category
            .as_deref()
            .is_some_and(generic_interface_category)
            .then_some("interface")
    };
    candidate
        .toc_category
        .as_deref()
        .and_then(from_toc_category)
        .or_else(|| {
            candidate
                .wowi_category
                .as_deref()
                .and_then(from_wowi_category)
        })
        .or_else(from_name)
        .or_else(from_topics)
        .or_else(generic)
        .or_else(from_summary)
        .unwrap_or("misc")
        .to_string()
}

#[cfg(test)]
mod tests {
    use feeling_core::catalog::CATEGORIES;

    use super::*;

    fn candidate(name: &str) -> Candidate {
        let mut c = Candidate::default();
        c.addon.name = name.into();
        c
    }

    fn category(name: &str) -> String {
        categorize(&candidate(name), None)
    }

    #[test]
    fn well_known_names_land_in_the_right_place() {
        for (name, want) in [
            ("Questie", "quests"),
            ("RestedXP Guides", "quests"),
            ("What's Training?", "quests"),
            ("Deadly Boss Mods", "bosses"),
            ("BigWigs", "bosses"),
            ("LittleWigs", "bosses"),
            ("GTFO", "bosses"),
            ("Plater Nameplates", "nameplates"),
            ("ForeverAuras", "nameplates"),
            ("OpenMap", "maps"),
            ("TomTom", "maps"),
            ("HandyNotes", "maps"),
            ("Bartender4", "interface"),
            ("Dominos", "interface"),
            ("ElvUI", "interface"),
            ("Titan Panel", "interface"),
            ("BetterBags", "bags"),
            ("ArkInventory", "bags"),
            ("Pawn", "bags"),
            ("Auctionator", "auction"),
            ("TradeSkillMaster", "auction"),
            ("GatherMate2", "professions"),
            ("SkillUp Forever", "misc"),
            ("Prat 3.0", "chat"),
            ("WIM", "chat"),
            ("Capping", "pvp"),
            ("Details!", "combat"),
            ("HealBot", "combat"),
            ("Ace3", "libraries"),
            ("LibSharedMedia-3.0", "libraries"),
            ("BugSack", "misc"),
        ] {
            assert_eq!(category(name), want, "{name}");
        }
    }

    #[test]
    fn libraries_are_detected_by_name_or_folder_without_catching_other_words() {
        assert_eq!(category("LibStub"), "libraries");
        assert_eq!(category("Liberation Army"), "misc");
        assert_eq!(category("Library of Things"), "libraries");
        let mut c = candidate("Shared Stuff");
        c.addon.folders = vec!["LibFoo-1.0".into()];
        assert_eq!(categorize(&c, None), "libraries");
    }

    #[test]
    fn declared_categories_beat_the_name() {
        let mut c = candidate("Raid Map Thing");
        c.toc_category = Some("Map & Minimap".into());
        assert_eq!(categorize(&c, None), "maps");
        c.toc_category = Some("UI Overhaul".into());
        assert_eq!(categorize(&c, None), "interface");
        c.toc_category = Some("Boss Encounters".into());
        assert_eq!(categorize(&c, None), "bosses");
    }

    #[test]
    fn a_generic_interface_declaration_says_less_than_the_name() {
        let mut c = candidate("ForeverAuras");
        c.toc_category = Some("Interface Enhancements".into());
        assert_eq!(categorize(&c, None), "nameplates");
        c.addon.name = "SilverDragon".into();
        assert_eq!(
            categorize(&c, None),
            "interface",
            "with nothing else to go on it still counts"
        );
        c.addon.name = "Grid2".into();
        c.toc_category = Some("Raid Frames".into());
        assert_eq!(
            categorize(&c, None),
            "interface",
            "raid frames aren't raid bosses"
        );
    }

    #[test]
    fn catch_all_declarations_fall_through() {
        let mut c = candidate("Quest Thing");
        c.toc_category = Some("Miscellaneous".into());
        assert_eq!(categorize(&c, None), "quests");
        c.toc_category = None;
        c.wowi_category = Some("27".into());
        assert_eq!(categorize(&c, None), "quests");
        c.addon.name = "Mystery".into();
        c.addon.summary = "Helps with your bank".into();
        assert_eq!(categorize(&c, None), "bags");
        c.topics = vec!["wow-addon".into(), "pvp".into()];
        assert_eq!(
            categorize(&c, None),
            "pvp",
            "topics come before the summary"
        );
    }

    #[test]
    fn wowi_categories_help_when_the_toc_says_nothing() {
        let mut c = candidate("Mystery");
        c.wowi_category = Some("45".into());
        assert_eq!(categorize(&c, None), "bosses");
        c.wowi_category = Some("94".into());
        assert_eq!(categorize(&c, None), "auction");
        c.wowi_category = Some("160".into());
        assert_eq!(categorize(&c, None), "misc");
    }

    #[test]
    fn overrides_win_and_nothing_falls_outside_the_list() {
        let over = Override {
            category: Some("pvp".into()),
            ..Override::default()
        };
        assert_eq!(categorize(&candidate("Questie"), Some(&over)), "pvp");
        let known: Vec<&str> = CATEGORIES.iter().map(|(id, _)| *id).collect();
        for (category, _) in KEYWORDS {
            assert!(
                known.contains(&category),
                "{category} isn't a real category"
            );
        }
        for id in 0..200 {
            if let Some(category) = from_wowi_category(&id.to_string()) {
                assert!(known.contains(&category));
            }
        }
    }

    #[test]
    fn keywords_match_whole_words_only() {
        assert_eq!(
            category("Barbaric Wizard"),
            "misc",
            "'bar' inside a word doesn't count"
        );
        assert_eq!(category("Mapleton"), "misc");
        assert_eq!(category("SuperMap"), "maps", "camelCase parts do count");
    }
}
