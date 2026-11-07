//! Command-line switches. A player never needs any of them: they exist so tests can run
//! against a fake install, and so every state can be screenshotted in `--demo`.

use crate::model::{Section, Tab};
use feeling_core::search::SortBy;
use std::path::PathBuf;

/// A fixed situation for `--demo` to start in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Scene {
    #[default]
    Normal,
    /// Nothing installed yet.
    Empty,
    /// No game anywhere.
    NoGame,
    /// Showing the cached catalog with no network.
    Offline,
    /// First run with no catalog, because none is published yet.
    NoCatalog,
    /// First run with no catalog, because there's no network.
    NoCatalogOffline,
    /// Still looking for everything, as in the first moments of a launch.
    Loading,
}

#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// Show the fixed demo catalog instead of touching the network or game folder.
    pub demo: bool,
    /// Render a few frames, save a PNG here, and exit.
    pub screenshot: Option<PathBuf>,
    /// Use only this install and skip detection.
    pub game: Option<PathBuf>,
    /// Plain http to this machine is allowed, for test rigs. Only `WIAF_TEST_LOOPBACK=1`
    /// turns this on, read once at startup.
    pub test_rig: bool,
    /// Where to fetch the catalog. Only honored on a test rig.
    pub catalog_url: Option<String>,
    /// Save frame counts here after 5 idle seconds, then exit.
    pub frame_report: Option<PathBuf>,
    /// Run the commands in this file, then exit. See `script.rs`.
    pub script: Option<PathBuf>,

    pub tab: Option<Tab>,
    pub section: Option<Section>,
    pub select: Option<String>,
    pub search: Option<String>,
    pub sort: Option<SortBy>,
    pub popup: Option<String>,
    pub progress: Option<f32>,
    /// With `--progress`: show a batch rather than one addon.
    pub batch: bool,
    pub scene: Scene,
    pub egg: Option<String>,
    /// Show the overlay for a zip hovering over the window.
    pub drop_overlay: bool,
    pub yellow: Vec<String>,
    pub red: Vec<String>,
    pub loot: Option<String>,
    pub scale: Option<f32>,
    /// Size of the window in points, at the UI scale in use.
    pub size: Option<[f32; 2]>,
    /// Keep the pointer here, to show hover states and tooltips.
    pub hover: Option<[f32; 2]>,
    /// Open this list header: a category id from the catalog, such as `quests`.
    pub open: Vec<String>,
    /// Open the changelog on the page.
    pub changelog: bool,
    /// How long a simulated demo operation takes.
    pub sim_ms: Option<u64>,
    /// When the process started, for timing how soon the window appears.
    pub started: Option<std::time::Instant>,
}

impl Launch {
    /// Unknown switches are ignored, because a shortcut with a stale one shouldn't stop
    /// the app from starting.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Self {
        let mut launch = Launch::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next();
            match arg.as_str() {
                "--demo" => launch.demo = true,
                "--screenshot" => launch.screenshot = value().map(PathBuf::from),
                "--game" => launch.game = value().map(PathBuf::from),
                "--catalog-url" => launch.catalog_url = value(),
                "--frame-report" => launch.frame_report = value().map(PathBuf::from),
                "--script" => launch.script = value().map(PathBuf::from),
                "--tab" => launch.tab = value().as_deref().and_then(parse_tab),
                "--settings" => {
                    launch.section = value().as_deref().and_then(parse_section);
                    launch.tab = Some(Tab::Settings);
                }
                "--select" => launch.select = value(),
                "--search" => launch.search = value(),
                "--sort" => launch.sort = value().as_deref().and_then(parse_sort),
                "--popup" => launch.popup = value(),
                "--progress" => launch.progress = value().and_then(|v| v.parse().ok()),
                "--batch" => launch.batch = true,
                "--scene" => launch.scene = value().as_deref().map_or(Scene::Normal, parse_scene),
                "--egg" => launch.egg = value(),
                "--drop" => launch.drop_overlay = true,
                "--yellow" => launch.yellow.extend(value()),
                "--red" => launch.red.extend(value()),
                "--loot" => launch.loot = value(),
                "--scale" => launch.scale = value().and_then(|v| v.parse().ok()),
                "--size" => launch.size = value().as_deref().and_then(parse_pair),
                "--hover" => launch.hover = value().as_deref().and_then(parse_pair),
                "--open" => launch.open.extend(value()),
                "--changelog" => launch.changelog = true,
                "--sim-ms" => launch.sim_ms = value().and_then(|v| v.parse().ok()),
                _ => {}
            }
        }
        launch
    }

    /// Runs nobody is sitting in front of. These open without taking focus, so a player's
    /// fullscreen game isn't interrupted.
    pub fn unattended(&self) -> bool {
        self.screenshot.is_some() || self.frame_report.is_some() || self.script.is_some()
    }
}

fn parse_tab(text: &str) -> Option<Tab> {
    match text {
        "my" | "my-addons" | "myaddons" => Some(Tab::MyAddons),
        "browse" => Some(Tab::Browse),
        "settings" => Some(Tab::Settings),
        _ => None,
    }
}

fn parse_section(text: &str) -> Option<Section> {
    match text {
        "game" => Some(Section::Game),
        "backups" => Some(Section::Backups),
        "lists" => Some(Section::Lists),
        "display" => Some(Section::Display),
        "updates" => Some(Section::Updates),
        "about" => Some(Section::About),
        _ => None,
    }
}

fn parse_sort(text: &str) -> Option<SortBy> {
    match text {
        "popular" => Some(SortBy::Popular),
        "updated" => Some(SortBy::Updated),
        "name" => Some(SortBy::Name),
        _ => None,
    }
}

fn parse_scene(text: &str) -> Scene {
    match text {
        "empty" => Scene::Empty,
        "nogame" => Scene::NoGame,
        "offline" => Scene::Offline,
        "nocatalog" => Scene::NoCatalog,
        "nocatalog-offline" => Scene::NoCatalogOffline,
        "loading" => Scene::Loading,
        _ => Scene::Normal,
    }
}

/// `820x560` or `820,560`.
fn parse_pair(text: &str) -> Option<[f32; 2]> {
    let (a, b) = text.split_once(['x', ','])?;
    Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Launch {
        Launch::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn nothing_at_all_is_a_normal_launch() {
        let launch = parse(&[]);
        assert!(!launch.demo);
        assert!(!launch.test_rig);
        assert!(!launch.unattended());
    }

    #[test]
    fn no_switch_can_turn_on_the_test_rig() {
        for arg in ["--test-rig", "--test_rig", "--loopback", "--catalog-url"] {
            assert!(!parse(&[arg, "1"]).test_rig, "{arg}");
        }
    }

    #[test]
    fn screenshots_can_ask_for_any_state() {
        let launch = parse(&[
            "--demo",
            "--settings",
            "about",
            "--search",
            "ques",
            "--progress",
            "0.5",
            "--size",
            "820x560",
            "--scene",
            "offline",
            "--popup",
            "remove",
        ]);
        assert!(launch.demo);
        assert_eq!(launch.tab, Some(Tab::Settings));
        assert_eq!(launch.section, Some(Section::About));
        assert_eq!(launch.search.as_deref(), Some("ques"));
        assert_eq!(launch.progress, Some(0.5));
        assert_eq!(launch.size, Some([820.0, 560.0]));
        assert_eq!(launch.scene, Scene::Offline);
        assert_eq!(launch.popup.as_deref(), Some("remove"));
    }

    #[test]
    fn unknown_switches_and_bad_values_are_ignored() {
        let launch = parse(&[
            "--nope",
            "--progress",
            "lots",
            "--tab",
            "moon",
            "--size",
            "big",
        ]);
        assert_eq!(launch.progress, None);
        assert_eq!(launch.tab, None);
        assert_eq!(launch.size, None);
    }

    #[test]
    fn test_runs_never_take_focus() {
        assert!(parse(&["--screenshot", "a.png"]).unattended());
        assert!(parse(&["--frame-report", "a.json"]).unattended());
        assert!(parse(&["--script", "a.txt"]).unattended());
    }
}
