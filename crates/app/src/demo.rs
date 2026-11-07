//! A small fixed world for `--demo`, used for screenshots and CI smoke tests. Nothing in
//! here reads a clock, a disk or the network, so its pictures come out the same every time.

use crate::args::Scene;
use eframe::egui::{Color32, ColorImage};
use feeling_core::backup::BackupEntry;
use feeling_core::catalog::{Catalog, Channel};
use feeling_core::detect::{FoundBy, Install};
use feeling_core::details::Details;
use feeling_core::scan::LocalFolder;
use feeling_core::selfupdate::{AssetKind, UpdateAsset, UpdateInfo};
use feeling_core::state::{InstallRecord, ManagedAddon, Origin, State};
use feeling_core::toc::Toc;
use std::path::PathBuf;

/// The demo's idea of what time it is: two hours after its catalog was built.
pub const NOW: i64 = 1_794_007_200;

const INSTALL_KEY: &str = "demo";
const ROOT: &str = r"C:\Games\World of Warcraft";

const DEMO: &str = r#"{"schema":1,"generated":1794000000,
"featured":["gh:Questie/Questie","gh:DeadlyBossMods/DeadlyBossMods","gh:Tercioo/Details-Damage-Meter","gh:tukui-org/ElvUI"],
"kits":[
{"name":"New Adventurer","description":"A gentle start: know where to go, find the fastest way there, and see what your trainer has for you.","addons":["gh:Questie/Questie","gh:fusionpit/WhatsTraining","wowi:7032"]},
{"name":"Dungeon Ready","description":"Boss timers, a damage meter and clearer nameplates for your first group.","addons":["gh:DeadlyBossMods/DeadlyBossMods","gh:Tercioo/Details-Damage-Meter","gh:Tercioo/Plater-Nameplates"]}
],
"gaps":[
{"name":"WeakAuras","reason":"The sources this app reads don't list a Forever build of it.","website":"https://github.com/WeakAuras/WeakAuras2","alternatives":["gh:Tercioo/Plater-Nameplates"]},
{"name":"Auctionator","reason":"The sources this app reads don't list a Forever build of it.","alternatives":[]}
],
"addons":[
{"id":"gh:Questie/Questie","name":"Questie","summary":"Marks available quests and objectives on your map, so you always know where to go next.","author":"The Questie Team","category":"quests","source":{"kind":"github","repo":"Questie/Questie"},"stable":{"version":"v12.6.2","url":"https://example.test/q.zip","size":93600000,"date":1793900000},"beta":{"version":"v12.7.0-beta","url":"https://example.test/qb.zip","size":93700000,"date":1793950000},"folders":["Questie"],"downloads":812000,"stars":980,"tier":5},
{"id":"gh:RestedXP/RXPGuides","name":"RestedXP Guide","summary":"Step-by-step leveling routes shown right on screen.","author":"RestedXP","category":"quests","source":{"kind":"github","repo":"RestedXP/RXPGuides"},"stable":{"version":"4.9.1","url":"https://example.test/r.zip","size":23800000,"date":1793800000},"folders":["RXPGuides"],"downloads":310000,"tier":4},
{"id":"gh:fusionpit/WhatsTraining","name":"What's Training?","summary":"Adds a spellbook tab listing what you can learn next and what it costs.","author":"fusionpit","category":"quests","source":{"kind":"github","repo":"fusionpit/WhatsTraining"},"stable":{"version":"6.2","url":"https://example.test/w.zip","size":412000,"date":1793000000},"folders":["WhatsTraining"],"downloads":90000,"tier":3},
{"id":"gh:cjber/shortest-path-forever","name":"Shortest Path Forever","summary":"Finds the fastest route across the world, flight paths included.","author":"cjber","category":"quests","source":{"kind":"github","repo":"cjber/shortest-path-forever"},"stable":{"version":"1.3.0","url":"https://example.test/s.zip","size":2100000,"date":1793500000},"folders":["ShortestPath"],"downloads":12000,"tier":2},
{"id":"gh:DeadlyBossMods/DeadlyBossMods","name":"Deadly Boss Mods","summary":"Timers and warnings for every boss fight.","author":"MysticalOS","category":"bosses","source":{"kind":"github","repo":"DeadlyBossMods/DeadlyBossMods"},"stable":{"version":"12.0.4","url":"https://example.test/d.zip","size":5600000,"date":1793950000},"folders":["DBM-Core","DBM-StatusBarTimers","DBM-GUI"],"downloads":760000,"tier":5},
{"id":"gh:BigWigsMods/BigWigs","name":"BigWigs","summary":"A lean alternative for boss timers and warnings.","author":"Funkeh, Maat, Rabbit","category":"bosses","source":{"kind":"github","repo":"BigWigsMods/BigWigs"},"stable":{"version":"v402.3","url":"https://example.test/b.zip","size":4120000,"date":1793940000},"folders":["BigWigs","BigWigs_Core","BigWigs_Options"],"downloads":540000,"tier":5},
{"id":"gh:Tercioo/Details-Damage-Meter","name":"Details! Damage Meter","summary":"See who did what in every fight, with charts and breakdowns.","author":"Terciob","category":"combat","source":{"kind":"github","repo":"Tercioo/Details-Damage-Meter"},"stable":{"version":"20261104","url":"https://example.test/de.zip","size":6900000,"date":1793930000},"folders":["Details","Details_DataStorage"],"downloads":690000,"tier":5},
{"id":"gh:tukui-org/ElvUI","name":"ElvUI","summary":"A complete replacement for the default interface.","author":"Elv","category":"interface","source":{"kind":"github","repo":"tukui-org/ElvUI"},"stable":{"version":"16.01","url":"https://example.test/e.zip","size":18400000,"date":1793700000},"folders":["ElvUI","ElvUI_Libraries","ElvUI_Options"],"downloads":620000,"tier":5},
{"id":"gh:Nevcairiel/Bartender4","name":"Bartender4","summary":"Move, resize and hide your action bars.","author":"Nevcairiel","category":"interface","source":{"kind":"github","repo":"Nevcairiel/Bartender4"},"stable":{"version":"4.18.4","url":"https://example.test/ba.zip","size":1800000,"date":1793200000},"folders":["Bartender4"],"downloads":410000,"tier":4},
{"id":"gh:tullamods/Dominos","name":"Dominos","summary":"A simple way to rearrange your action bars.","author":"Tuller","category":"interface","source":{"kind":"github","repo":"tullamods/Dominos"},"stable":{"version":"11.2.3","url":"https://example.test/do.zip","size":1500000,"date":1793100000},"folders":["Dominos"],"downloads":330000,"tier":4},
{"id":"gh:openmap-world/openmap","name":"OpenMap","summary":"A world map overlay with paths, caves and dungeon entrances.","author":"OpenMap","category":"maps","source":{"kind":"github","repo":"openmap-world/openmap"},"stable":{"version":"2.1.0","url":"https://example.test/o.zip","size":90300000,"date":1793600000},"folders":["OpenMap"],"downloads":150000,"tier":4},
{"id":"wowi:7032","name":"TomTom","summary":"Waypoints and a crazy arrow pointing where you need to go.","author":"cladhaire","category":"maps","source":{"kind":"wowi","id":7032},"stable":{"version":"v4.0.5","url":"https://example.test/t.zip","size":320000,"date":1793000000},"folders":["TomTom"],"downloads":3100000,"tier":4},
{"id":"gh:Nevcairiel/GatherMate2","name":"GatherMate2","summary":"Remembers every herb and ore node you've found.","author":"Nevcairiel","category":"professions","source":{"kind":"github","repo":"Nevcairiel/GatherMate2"},"stable":{"version":"1.51","url":"https://example.test/g.zip","size":3050000,"date":1792900000},"folders":["GatherMate2"],"downloads":260000,"tier":3},
{"id":"gh:Tercioo/Plater-Nameplates","name":"Plater Nameplates","summary":"Highly customizable nameplates.","author":"Terciob","category":"nameplates","source":{"kind":"github","repo":"Tercioo/Plater-Nameplates"},"stable":{"version":"620","url":"https://example.test/p.zip","size":4400000,"date":1793400000},"folders":["Plater"],"downloads":380000,"tier":4},
{"id":"gh:funkydude/BugSack","name":"BugSack","summary":"Catches Lua errors in a sack instead of popping them in your face.","author":"funkydude","category":"libraries","source":{"kind":"github","repo":"funkydude/BugSack"},"stable":{"version":"v12.0.1","url":"https://example.test/bs.zip","size":120000,"date":1793050000},"folders":["BugSack"],"deps":["!BugGrabber"],"downloads":200000,"tier":3},
{"id":"gh:VgerMods/Pawn","name":"Pawn","summary":"Tells you whether that drop is actually an upgrade.","author":"Vger","category":"misc","source":{"kind":"github","repo":"VgerMods/Pawn"},"stable":{"version":"2.14.1","url":"https://example.test/pa.zip","size":1300000,"date":1793300000},"folders":["Pawn"],"downloads":175000,"tier":3},
{"id":"gh:Legacy-of-Sylvanaar/prat-3-0","name":"Prat 3.0","summary":"Chat improvements: timestamps, colors, scrollback.","author":"Sylvanaar","category":"chat","source":{"kind":"github","repo":"Legacy-of-Sylvanaar/prat-3-0"},"stable":{"version":"3.9.80","url":"https://example.test/pr.zip","size":2400000,"date":1792800000},"folders":["Prat-3.0","Prat-3.0_Libraries"],"downloads":99000,"tier":2},
{"id":"gh:someone/old-addon","name":"Old Abandoned Addon","summary":"Hasn't kept up with Forever.","author":"someone","category":"misc","source":{"kind":"github","repo":"someone/old-addon"},"stable":{"version":"0.9","url":"https://example.test/old.zip","size":90000,"date":1790000000},"folders":["OldAddon"],"downloads":800,"tier":0}
]}"#;

pub fn catalog() -> Catalog {
    serde_json::from_str(DEMO).expect("the demo catalog is valid")
}

fn record(id: &str, name: &str, version: &str, folders: &[&str]) -> ManagedAddon {
    ManagedAddon {
        id: id.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
        channel: Channel::Stable,
        folders: folders.iter().map(|f| (*f).to_owned()).collect(),
        installed_at: NOW - 86_400 * 20,
        updated_at: NOW - 86_400 * 3,
        ..Default::default()
    }
}

/// The settings a demo starts with: a fake install holding a few addons.
pub fn state(scene: Scene) -> State {
    let mut state = State {
        current_install: (scene != Scene::NoGame).then(|| INSTALL_KEY.to_owned()),
        ..Default::default()
    };
    state.stats.launches = 7;
    state.stats.first_launch = NOW - 86_400 * 20;
    state.stats.seconds_open = 8_100;
    state.stats.addons_installed = 9;
    state.stats.updates_applied = 21;
    if scene == Scene::NoGame {
        return state;
    }
    let mut record_list = InstallRecord {
        last_wtf_backup: NOW - 3600,
        ..Default::default()
    };
    if scene != Scene::Empty {
        let mut tomtom = record("wowi:7032", "TomTom", "v4.0.5", &["TomTom"]);
        tomtom.previous_version = Some("v4.0.4".to_owned());
        tomtom.previous_zip = Some(PathBuf::from(r"C:\demo\zips\v4.0.4.zip"));
        let mut dominos = record("gh:tullamods/Dominos", "Dominos", "11.2.2", &["Dominos"]);
        dominos.pinned = true;
        let mut tweaks = record("local:MyTweaks", "My Tweaks", "1.2", &["MyTweaks"]);
        tweaks.origin = Origin::LocalZip;
        let addons = [
            record("gh:Questie/Questie", "Questie", "v12.5.9", &["Questie"]),
            record(
                "gh:DeadlyBossMods/DeadlyBossMods",
                "Deadly Boss Mods",
                "12.0.1",
                &["DBM-Core", "DBM-StatusBarTimers", "DBM-GUI"],
            ),
            record(
                "gh:Tercioo/Details-Damage-Meter",
                "Details! Damage Meter",
                "20261020",
                &["Details", "Details_DataStorage"],
            ),
            record(
                "gh:Nevcairiel/Bartender4",
                "Bartender4",
                "4.18.4",
                &["Bartender4"],
            ),
            record("gh:funkydude/BugSack", "BugSack", "v12.0.1", &["BugSack"]),
            record("gh:VgerMods/Pawn", "Pawn", "2.14.1", &["Pawn"]),
            record("gh:someone/old-addon", "Old Abandoned Addon", "0.9", &["OldAddon"]),
            tomtom,
            dominos,
            tweaks,
        ];
        for addon in addons {
            record_list.managed.insert(addon.id.clone(), addon);
        }
    }
    state.installs.insert(INSTALL_KEY.to_owned(), record_list);
    state
}

pub fn install() -> (Install, String) {
    let root = PathBuf::from(ROOT);
    let forever = root.join("_forever_");
    let install = Install {
        addons_dir: forever.join("Interface").join("AddOns"),
        wtf_dir: forever.join("WTF"),
        forever_dir: forever,
        root,
        version: Some("1.60.1".to_owned()),
        found_by: FoundBy::BattleNet,
    };
    (install, INSTALL_KEY.to_owned())
}

fn folder(name: &str, title: &str, version: Option<&str>, forever: bool, notes: &str) -> LocalFolder {
    LocalFolder {
        name: name.to_owned(),
        toc_file: Some(format!("{name}.toc")),
        toc: Toc {
            title: Some(title.to_owned()),
            notes: (!notes.is_empty()).then(|| notes.to_owned()),
            version: version.map(str::to_owned),
            interfaces: vec![if forever { 16001 } else { 110_207 }],
            ..Toc::default()
        },
        targets_forever: forever,
        modified: NOW - 86_400,
    }
}

/// What's in the fake AddOns folder when a demo starts.
pub fn disk(state: &State, scene: Scene) -> Vec<LocalFolder> {
    let mut folders = Vec::new();
    if let Some(record) = state.current() {
        for addon in record.managed.values() {
            let forever = addon.id != "gh:someone/old-addon";
            for name in &addon.folders {
                folders.push(folder(name, &addon.name, Some(&addon.version), forever, ""));
            }
        }
    }
    if scene != Scene::Empty && scene != Scene::NoGame {
        folders.push(folder(
            "!BugGrabber",
            "!BugGrabber",
            Some("v12.0.1"),
            true,
            "Catches errors for BugSack to show.",
        ));
        folders.push(folder(
            "GatherMate2",
            "GatherMate2",
            Some("1.48"),
            true,
            "Remembers every herb and ore node you've found.",
        ));
        folders.push(folder(
            "HandyNotes",
            "HandyNotes",
            Some("1.6.9"),
            true,
            "Pins your own notes to the map.",
        ));
        folders.push(folder(
            "HandyNotes_Extras",
            "HandyNotes Extras",
            None,
            true,
            "",
        ));
        folders.push(folder(
            "OldRetailThing",
            "Old Retail Thing",
            Some("9.2"),
            false,
            "Made for another version of the game.",
        ));
    }
    folders.sort_by_cached_key(|f| f.name.to_lowercase());
    folders
}

/// Readme-style text for the page. Original words, with no real addon's text in them.
pub fn details(summary: &str) -> Details {
    Details {
        about: format!(
            "{summary}\n\nThis text stands in for the addon's readme, which the real app \
             fetches when you stop on a row for a moment. Long readmes scroll, and the buttons \
             stay put along the bottom of the page.\n\nNothing here came from the network: \
             the demo keeps to itself."
        ),
        changelog: Some(
            "Fixes a few things nobody reported.\n\nBreaks a few things nobody will notice."
                .to_owned(),
        ),
        images: vec!["demo://screenshot".to_owned()],
    }
}

/// A made-up screenshot: a little map with a road across it.
pub fn screenshot() -> ColorImage {
    const W: usize = 480;
    const H: usize = 240;
    let mut pixels = Vec::with_capacity(W * H);
    for y in 0..H {
        for x in 0..W {
            let (fx, fy) = (x as f32 / W as f32, y as f32 / H as f32);
            let hill = ((fx * 9.0).sin() + (fy * 7.0 + fx * 3.0).cos()) * 0.5;
            let road = (fy - 0.5 - (fx * 6.0).sin() * 0.25).abs() < 0.012;
            let base = 58.0 + hill * 20.0;
            pixels.push(if road {
                Color32::from_rgb(214, 170, 60)
            } else {
                Color32::from_rgb(
                    (base * 0.8) as u8,
                    (base * 1.1) as u8,
                    (base * 0.55) as u8,
                )
            });
        }
    }
    ColorImage::new([W, H], pixels)
}

pub fn backups() -> Vec<BackupEntry> {
    let day = 86_400;
    [1, 2, 9]
        .iter()
        .map(|days_ago| {
            let created = NOW - days_ago * day;
            BackupEntry {
                path: PathBuf::from(format!(r"C:\demo\backups\WTF-{created}.zip")),
                created,
                size: 1_840_000 + (*days_ago as u64) * 37_000,
            }
        })
        .collect()
}

pub fn update_info() -> UpdateInfo {
    UpdateInfo {
        version: "0.2.0".to_owned(),
        notes: "What changed, in plain words.\n\nDropped zips remember their names now. \
                The casting bar stops one pixel earlier, because it was bothering someone.\n\n\
                Backups are a little faster on slow disks."
            .to_owned(),
        date: NOW - 3600,
        asset: UpdateAsset {
            url: "https://example.test/update".to_owned(),
            sha256: String::new(),
            size: 5_500_000,
            kind: AssetKind::Exe,
        },
    }
}

/// The zips a real install would have kept, so rollback can be tried in the demo.
pub fn kept_zips(version: &str, prev: Option<&ManagedAddon>) -> Vec<PathBuf> {
    let zip = |v: &str| PathBuf::from(format!(r"C:\demo\zips\{v}.zip"));
    let mut kept = vec![zip(version)];
    kept.extend(prev.map(|p| zip(&p.version)));
    kept
}

/// The last lines of a log that doesn't exist.
pub fn log_lines() -> Vec<String> {
    vec!["2026-11-06 12:00:00Z [INFO] This is a demo, so nothing was logged.".to_owned()]
}
