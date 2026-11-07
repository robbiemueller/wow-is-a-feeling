//! Real clicks through the real app, with no window: a headless harness presses buttons by
//! their accessibility labels, against a temp-folder install and a loopback server. Nothing
//! here goes near a real game folder or the player's settings.

use crate::actions::Action;
use crate::app::{App, PageKind, Startup};
use crate::args::{Launch, Scene};
use crate::demo;
use crate::dialogs::Popup;
use crate::easter;
use crate::model::{CatalogStatus, GameState, Section, Selection, Tab};
use crate::worker::{Dirs, Event};
use eframe::egui::{self, Key, Modifiers, OutputCommand, ViewportId};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use feeling_core::catalog::{Addon, Catalog, Channel, Gap, Kit, Release, Source};
use feeling_core::state::{Origin, State};
use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FOREVER_TOC: &str = "## Interface: 16001\n";
const WAIT: Duration = Duration::from_secs(30);

/// A tiny web server standing in for the addon sites.
#[derive(Clone)]
struct Site {
    base: String,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

/// How big a `/slow/` file claims to be. It only ever trickles out a little of it.
const SLOW_SIZE: u64 = 4 * 1024 * 1024;

impl Site {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let site = Site {
            base,
            files: Arc::default(),
        };
        let handler = site.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = handler.clone();
                thread::spawn(move || handler.answer(stream));
            }
        });
        site
    }

    fn answer(&self, mut stream: TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0u8; 2048];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buffer[..n]),
            }
        }
        let request = String::from_utf8_lossy(&request).into_owned();
        let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
        if path.starts_with("/slow/") {
            // A big download, forever underway, until the other end hangs up.
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {SLOW_SIZE}\r\nconnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            while stream.write_all(&[0u8; 512]).is_ok() {
                thread::sleep(Duration::from_millis(10));
            }
            return;
        }
        let body = self.files.lock().unwrap().get(&path).cloned();
        let Some(body) = body else {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
            return;
        };
        let etag = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            body.hash(&mut hasher);
            format!("\"{:x}\"", hasher.finish())
        };
        if request
            .to_lowercase()
            .contains(&format!("if-none-match: {etag}"))
        {
            let _ = stream.write_all(b"HTTP/1.1 304 Not Modified\r\nconnection: close\r\n\r\n");
            return;
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\netag: {etag}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    }

    fn put(&self, path: &str, bytes: Vec<u8>) -> String {
        self.files.lock().unwrap().insert(path.to_owned(), bytes);
        format!("{}{path}", self.base)
    }

    fn publish(&self, catalog: &Catalog) -> String {
        self.put("/catalog.json", serde_json::to_vec(catalog).unwrap())
    }
}

/// A zip holding addon folders, each with a TOC and a file that says which version this is.
fn addons_zip(folders: &[&str], version: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for folder in folders {
        let toc = format!("{FOREVER_TOC}## Title: {folder}\n## Version: {version}\n");
        let lua = format!("-- {version}\n");
        let files = [
            (format!("{folder}/{folder}.toc"), toc),
            (format!("{folder}/{folder}.lua"), lua),
        ];
        for (name, text) in files {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
    }
    writer.finish().unwrap().into_inner()
}

fn addon_zip(folder: &str, version: &str) -> Vec<u8> {
    addons_zip(&[folder], version)
}

/// A catalog entry whose zip the site is really serving, the way a hand-picked one looks.
fn publish_addon(site: &Site, id: &str, folder: &str, version: &str) -> Addon {
    publish_bundle(site, id, &[folder], &[folder], version)
}

/// The same with `listed` being what the catalog says and `shipped` what's really in the zip.
fn publish_bundle(
    site: &Site,
    id: &str,
    listed: &[&str],
    shipped: &[&str],
    version: &str,
) -> Addon {
    let folder = listed[0];
    let zip = addons_zip(shipped, version);
    let size = zip.len() as u64;
    let url = site.put(&format!("/{folder}-{version}.zip"), zip);
    Addon {
        id: id.to_owned(),
        name: format!("{folder} Addon"),
        summary: format!("Does {folder} things."),
        author: "Someone".to_owned(),
        category: "misc".to_owned(),
        // A direct source never asks the live sites for a readme.
        source: Source::Direct { url: url.clone() },
        stable: Some(Release {
            version: version.to_owned(),
            url,
            size,
            date: 1_793_000_000,
            ..Default::default()
        }),
        folders: listed.iter().map(|f| (*f).to_owned()).collect(),
        tier: 3,
        downloads: 100,
        trusted: true,
        ..Default::default()
    }
}

fn catalog_of(addons: Vec<Addon>) -> Catalog {
    Catalog {
        schema: 1,
        generated: 1_794_000_000,
        featured: addons.iter().map(|a| a.id.clone()).collect(),
        addons,
        ..Default::default()
    }
}

/// A fake game, fake settings folder and a site, all in a temp folder.
struct Rig {
    home: tempfile::TempDir,
    game: PathBuf,
    site: Site,
    dirs: Dirs,
}

impl Rig {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let game = make_game(&home.path().join("World of Warcraft"));
        let dirs = Dirs {
            state_file: home.path().join("config").join("state.json"),
            cache: home.path().join("cache"),
            data: home.path().join("data"),
        };
        let mut state = State::default();
        // No test should go looking for real app updates.
        state.settings.check_app_updates = false;
        state.settings.backup_wtf = false;
        state.save_to(&dirs.state_file).unwrap();
        Rig {
            home,
            game,
            site: Site::start(),
            dirs,
        }
    }

    fn addons(&self) -> PathBuf {
        self.game.join("_forever_").join("Interface").join("AddOns")
    }

    fn version_on_disk(&self, folder: &str) -> Option<String> {
        let lua = self.addons().join(folder).join(format!("{folder}.lua"));
        let text = fs::read_to_string(lua).ok()?;
        Some(text.trim_start_matches("-- ").trim().to_owned())
    }

    /// Put a folder on disk the way some other tool, or the player, left it.
    fn plant(&self, folder: &str, version: &str) {
        let dir = self.addons().join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{folder}.toc")),
            format!("{FOREVER_TOC}## Title: {folder} from elsewhere\n## Version: {version}\n"),
        )
        .unwrap();
        fs::write(dir.join(format!("{folder}.lua")), format!("-- {version}\n")).unwrap();
    }

    fn set_up_state(&self, change: impl FnOnce(&mut State)) {
        let mut state = State::load_from(&self.dirs.state_file);
        change(&mut state);
        state.save_to(&self.dirs.state_file).unwrap();
    }

    fn saved_state(&self) -> State {
        State::load_from(&self.dirs.state_file)
    }

    /// Start the app the way a real launch would, reading state from disk.
    fn launch(&self) -> Harness<'static, Option<App>> {
        let launch = Launch {
            game: Some(self.game.clone()),
            test_rig: true,
            catalog_url: Some(format!("{}/catalog.json", self.site.base)),
            ..Launch::default()
        };
        let state = State::load_from(&self.dirs.state_file);
        harness(launch, self.dirs.clone(), state)
    }

    /// Start and wait until the game and catalog have been dealt with.
    fn launch_settled(&self) -> Harness<'static, Option<App>> {
        let mut h = self.launch();
        settle(&mut h);
        h
    }
}

fn make_game(root: &Path) -> PathBuf {
    let forever = root.join("_forever_");
    fs::create_dir_all(forever.join("Interface").join("AddOns")).unwrap();
    fs::write(
        forever.join(".flavor.info"),
        "Product Flavor!STRING:0\nwowf\n",
    )
    .unwrap();
    fs::write(forever.join("Wow.exe"), b"").unwrap();
    fs::write(
        root.join(".build.info"),
        "Product!STRING:0|Version!STRING:0|Active!DEC:1\nwowf|1.60.1.12345|1\n",
    )
    .unwrap();
    root.to_path_buf()
}

fn harness(launch: Launch, dirs: Dirs, state: State) -> Harness<'static, Option<App>> {
    let mut launch = Some(launch);
    let mut startup = Some(Startup {
        dirs,
        state,
        lock: None,
        already_running: false,
    });
    Harness::builder()
        .with_size(egui::vec2(1040.0, 700.0))
        .with_max_steps(40)
        .build_ui_state(
            move |ui, slot: &mut Option<App>| {
                let app = slot.get_or_insert_with(|| {
                    App::new(
                        ui.ctx(),
                        launch.take().expect("the app starts once"),
                        startup.take().expect("the app starts once"),
                    )
                });
                app.logic();
                app.draw(ui);
            },
            None,
        )
}

fn demo_harness(scene: Scene, temp: &Path) -> Harness<'static, Option<App>> {
    let launch = Launch {
        demo: true,
        scene,
        sim_ms: Some(30),
        ..Launch::default()
    };
    let dirs = Dirs {
        state_file: temp.join("state.json"),
        cache: temp.join("cache"),
        data: temp.join("data"),
    };
    let mut h = harness(launch, dirs, demo::state(scene));
    h.step();
    h.step();
    h
}

fn app<'a>(h: &'a Harness<'static, Option<App>>) -> &'a App {
    h.state()
        .as_ref()
        .expect("the app starts on the first frame")
}

fn app_mut<'a>(h: &'a mut Harness<'static, Option<App>>) -> &'a mut App {
    h.state_mut()
        .as_mut()
        .expect("the app starts on the first frame")
}

/// Keep drawing frames until the workers have delivered what's being waited for.
fn pump(h: &mut Harness<'static, Option<App>>, what: &str, done: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        h.step();
        if done(app(h)) {
            h.step();
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(4));
    }
}

fn settle(h: &mut Harness<'static, Option<App>>) {
    pump(h, "startup to settle", App::is_settled);
}

fn finish_ops(h: &mut Harness<'static, Option<App>>) {
    pump(h, "the queue to empty", |a| a.queue.is_idle());
    settle(h);
}

/// Press something, and draw until what it changed has fully appeared. A popup's first
/// frame only measures it, so that takes a couple of frames after the click itself.
fn click(h: &mut Harness<'static, Option<App>>, label: &str) {
    h.get_by_label(label).click();
    h.step();
    h.step();
    h.step();
}

/// For when a popup's button shares its label with one behind it: finds the one that's
/// actually on the popup.
fn click_last(h: &mut Harness<'static, Option<App>>, label: &str) {
    let popup = h
        .ctx
        .memory(|m| m.area_rect(egui::Id::unique("static-popup")))
        .expect("a popup is open");
    h.get_all_by_label(label)
        .find(|node| popup.contains(node.rect().center()))
        .expect("that button on the popup")
        .click();
    h.step();
    h.step();
    h.step();
}

/// Close the app the way the window would, which waits for saves.
fn close(mut h: Harness<'static, Option<App>>) {
    app_mut(&mut h).shut_down();
}

fn press(h: &mut Harness<'static, Option<App>>, key: Key) {
    h.key_press(key);
    h.step();
    h.step();
}

fn type_text(h: &mut Harness<'static, Option<App>>, text: &str) {
    h.event(egui::Event::Text(text.to_owned()));
    h.step();
    h.step();
}

fn has_label(h: &Harness<'static, Option<App>>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

fn said(h: &Harness<'static, Option<App>>, part: &str) -> bool {
    app(h).messages.texts().iter().any(|t| t.contains(part))
}

fn managed<'a>(
    h: &'a Harness<'static, Option<App>>,
    id: &str,
) -> Option<&'a feeling_core::state::ManagedAddon> {
    app(h).managed(id)
}

const FOO: &str = "direct:Foo";
const BAR: &str = "direct:Bar";

#[test]
fn a_player_installs_updates_rolls_back_and_removes_with_real_files() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));

    // First launch: nothing installed, so Browse, with the addon recommended and selected.
    let mut h = rig.launch_settled();
    assert_eq!(app(&h).tab, Tab::Browse);
    assert_eq!(
        app(&h).status_text(),
        "Forever 1.60.1  |  1 addon  |  updated ".to_owned()
            + &crate::model::age_text(app(&h).now_unix() - 1_794_000_000)
    );
    assert!(has_label(&h, "Foo Addon, 1.0"));
    assert_eq!(app(&h).page_kind(), PageKind::Addon(FOO.to_owned()));

    click(&mut h, "Install");
    assert!(app(&h).casting().is_some(), "the casting bar shows it");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("1.0"));
    let record = managed(&h, FOO).expect("the install is recorded");
    assert_eq!(record.version, "1.0");
    assert_eq!(record.origin, Origin::Catalog);
    assert_eq!(record.folders, vec!["Foo".to_owned()]);
    assert_eq!(
        app(&h).loot.as_ref().map(|(line, _)| line.as_str()),
        Some("You receive addon: Foo Addon.")
    );
    assert!(has_label(&h, "Foo Addon, installed"));
    close(h);
    assert!(
        rig.saved_state()
            .installs
            .values()
            .any(|i| i.managed.contains_key(FOO)),
        "the record reached state.json"
    );

    // A new version is published. The next launch finds it and offers it.
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "2.0",
    )]));
    let mut h = rig.launch_settled();
    assert_eq!(app(&h).tab, Tab::MyAddons, "something's installed now");
    assert_eq!(app(&h).updates, vec![FOO.to_owned()]);
    assert!(has_label(&h, "Update all (1)"));
    assert!(has_label(&h, "Updates available (1)"));
    click(&mut h, "Foo Addon, update");
    click(&mut h, "Update");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("2.0"));
    let record = managed(&h, FOO).unwrap();
    assert_eq!(record.version, "2.0");
    assert_eq!(record.previous_version.as_deref(), Some("1.0"));
    assert!(record.previous_zip.as_ref().unwrap().exists());
    assert!(app(&h).updates.is_empty());
    assert!(!has_label(&h, "Update all (1)"));

    // Roll back. The newer version stays on offer, and can be rolled back to in turn.
    click(&mut h, "Roll back to 1.0");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("1.0"));
    let record = managed(&h, FOO).unwrap();
    assert_eq!(record.version, "1.0");
    assert_eq!(record.previous_version.as_deref(), Some("2.0"));
    assert_eq!(app(&h).updates, vec![FOO.to_owned()]);
    assert!(has_label(&h, "Roll back to 2.0"));

    // The pin takes it out of the update list and survives a restart.
    click(&mut h, "Don't update this addon");
    assert!(managed(&h, FOO).unwrap().pinned);
    assert!(app(&h).updates.is_empty());
    h.step();
    close(h);

    // Remove: asks first, and Cancel means nothing happens.
    let mut h = rig.launch_settled();
    assert!(managed(&h, FOO).unwrap().pinned, "the pin was saved");
    click(&mut h, "Remove");
    assert!(matches!(app(&h).popup, Some(Popup::Remove(_))));
    assert!(has_label(
        &h,
        "Remove Foo Addon? Its folders are deleted. Your saved settings in WTF stay."
    ));
    click_last(&mut h, "Cancel");
    assert!(app(&h).popup.is_none());
    assert!(rig.addons().join("Foo").exists());

    click(&mut h, "Remove");
    click_last(&mut h, "Remove");
    finish_ops(&mut h);
    assert!(!rig.addons().join("Foo").exists());
    assert!(managed(&h, FOO).is_none());
    assert!(
        !rig.game
            .join("_forever_")
            .join("Interface")
            .join(".wiaf")
            .join("j")
            .exists()
            || fs::read_dir(
                rig.game
                    .join("_forever_")
                    .join("Interface")
                    .join(".wiaf")
                    .join("j")
            )
            .unwrap()
            .next()
            .is_none(),
        "no journal left behind"
    );
}

#[test]
fn cancel_stops_a_download_and_changes_nothing() {
    let rig = Rig::new();
    let addon = slow_addon(&rig);
    rig.site.publish(&catalog_of(vec![addon]));

    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    pump(&mut h, "the download to start", |a| {
        a.casting().is_some_and(|c| c.fraction > 0.0)
    });
    assert!(has_label(&h, "Cancel"));
    click(&mut h, "Cancel");
    pump(&mut h, "the op to stop", |a| a.queue.is_idle());
    assert!(!rig.addons().join("Foo").exists(), "nothing was installed");
    assert!(managed(&h, FOO).is_none(), "and nothing was recorded");
    assert!(app(&h).messages.texts().contains(&"Cancelled."));
    assert!(app(&h).casting().is_none());
}

/// An addon whose download never finishes.
fn slow_addon(rig: &Rig) -> Addon {
    let mut addon = publish_addon(&rig.site, FOO, "Foo", "1.0");
    let release = addon.stable.as_mut().unwrap();
    release.url = format!("{}/slow/Foo.zip", rig.site.base);
    release.size = SLOW_SIZE;
    addon
}

#[test]
fn a_failed_install_says_why_and_changes_nothing() {
    let rig = Rig::new();
    let mut addon = publish_addon(&rig.site, FOO, "Foo", "1.0");
    addon.stable.as_mut().unwrap().url = format!("{}/gone.zip", rig.site.base);
    rig.site.publish(&catalog_of(vec![addon]));

    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    finish_ops(&mut h);
    assert!(!rig.addons().join("Foo").exists());
    assert!(managed(&h, FOO).is_none());
    let texts = app(&h).messages.texts();
    assert!(
        texts.iter().any(|t| t.contains("answered with an error")),
        "{texts:?}"
    );
}

#[test]
fn a_bad_zip_is_refused_and_the_old_version_stays() {
    let rig = Rig::new();
    let good = publish_addon(&rig.site, FOO, "Foo", "1.0");
    rig.site.publish(&catalog_of(vec![good]));
    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    finish_ops(&mut h);
    close(h);

    // The next version's zip has no addon in it at all.
    let mut bad = publish_addon(&rig.site, FOO, "Foo", "2.0");
    let junk = {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"nothing to see").unwrap();
        writer.finish().unwrap().into_inner()
    };
    let release = bad.stable.as_mut().unwrap();
    release.size = junk.len() as u64;
    release.url = rig.site.put("/junk.zip", junk);
    rig.site.publish(&catalog_of(vec![bad]));

    let mut h = rig.launch_settled();
    click(&mut h, "Foo Addon, update");
    click(&mut h, "Update");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("1.0"));
    assert_eq!(managed(&h, FOO).unwrap().version, "1.0");
    let texts = app(&h).messages.texts();
    assert!(
        texts.iter().any(|t| t.contains("anything WoW can load")),
        "{texts:?}"
    );
}

#[test]
fn a_folder_from_another_tool_can_be_adopted_and_then_updated() {
    let rig = Rig::new();
    // Put an old copy on disk, the way some other manager left it.
    let folder = rig.addons().join("Foo");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("Foo.toc"),
        format!("{FOREVER_TOC}## Title: Foo from elsewhere\n## Version: 0.5\n"),
    )
    .unwrap();
    fs::write(folder.join("Foo.lua"), "-- 0.5\n").unwrap();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));

    let mut h = rig.launch_settled();
    assert_eq!(app(&h).tab, Tab::MyAddons, "something's on disk");
    assert!(has_label(&h, "Other folders (1)"));
    click(&mut h, "Foo from elsewhere, 0.5");
    assert_eq!(app(&h).page_kind(), PageKind::Other("Foo".to_owned()));
    assert!(has_label(&h, "This addon wasn't installed by this app."));

    click(&mut h, "Manage with this app");
    let record = managed(&h, FOO).expect("adopted");
    assert_eq!(record.origin, Origin::Adopted);
    assert_eq!(record.version, "0.5");
    assert_eq!(record.folders, vec!["Foo".to_owned()]);
    assert_eq!(
        rig.version_on_disk("Foo").as_deref(),
        Some("0.5"),
        "untouched so far"
    );
    assert_eq!(app(&h).updates, vec![FOO.to_owned()], "and it gets updates");
    assert!(has_label(&h, "Update all (1)"));

    click(&mut h, "Update all (1)");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("1.0"));
    let record = managed(&h, FOO).unwrap();
    assert_eq!(record.version, "1.0");
    assert_eq!(record.origin, Origin::Adopted);
    assert_eq!(
        record.previous_version, None,
        "there's no zip of the old one to go back to"
    );
    assert!(!has_label(&h, "Other folders (1)"));
}

#[test]
fn an_unmatched_other_folder_can_only_be_removed_after_asking() {
    let rig = Rig::new();
    let folder = rig.addons().join("Mine");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("Mine.toc"),
        "## Interface: 110000\n## Title: Mine\n",
    )
    .unwrap();
    rig.site.publish(&catalog_of(Vec::new()));

    let mut h = rig.launch_settled();
    click(&mut h, "Mine, out of date");
    assert!(!has_label(&h, "Manage with this app"));
    click(&mut h, "Remove");
    assert!(matches!(app(&h).popup, Some(Popup::Remove(_))));
    assert!(folder.exists(), "only once it's confirmed");
    click_last(&mut h, "Remove");
    finish_ops(&mut h);
    assert!(!folder.exists());
}

#[derive(Debug)]
struct DroppedZip(PathBuf);

impl egui::DroppedFile for DroppedZip {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        fs::read(&self.0).map_err(|e| e.to_string())
    }
}

fn drop_file(h: &mut Harness<'static, Option<App>>, path: PathBuf) {
    h.input_mut().dropped_files.push(Arc::new(DroppedZip(path)));
    h.step();
    h.step();
}

#[test]
fn a_dropped_zip_installs_safely_and_is_tracked_as_local() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let zip = rig.home.path().join("Dropped.zip");
    fs::write(&zip, addon_zip("Dropped", "3.1")).unwrap();
    let not_zip = rig.home.path().join("notes.txt");
    fs::write(&not_zip, "hello").unwrap();

    let mut h = rig.launch_settled();
    drop_file(&mut h, not_zip);
    assert!(
        app(&h)
            .messages
            .texts()
            .contains(&"Only .zip files can be installed this way.")
    );

    drop_file(&mut h, zip);
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Dropped").as_deref(), Some("3.1"));
    let record = managed(&h, "local:Dropped").expect("tracked by its main folder");
    assert_eq!(record.origin, Origin::LocalZip);
    assert_eq!(record.version, "3.1");
    assert_eq!(record.name, "Dropped");
    assert!(app(&h).updates.is_empty(), "local zips don't get updates");

    // Its page has no main button to press, only Remove.
    click(&mut h, "My Addons");
    click(&mut h, "Dropped, 3.1");
    assert_eq!(
        app(&h).page_kind(),
        PageKind::Addon("local:Dropped".to_owned())
    );
    assert!(!has_label(&h, "Install") && !has_label(&h, "Reinstall"));
    assert!(has_label(
        &h,
        "Installed from a zip file, so it doesn't get updates."
    ));
}

#[test]
fn a_zip_dropped_over_a_catalog_addon_takes_its_place() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    finish_ops(&mut h);

    let zip = rig.home.path().join("MyFoo.zip");
    fs::write(&zip, addon_zip("Foo", "9.9")).unwrap();
    drop_file(&mut h, zip);
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("9.9"));
    assert!(
        managed(&h, FOO).is_none(),
        "no more updates for what's been replaced"
    );
    assert_eq!(managed(&h, "local:Foo").unwrap().version, "9.9");
    assert!(said(
        &h,
        "The zip you dropped replaced Foo. Install Foo Addon again to get its own back."
    ));
}

#[test]
fn a_dropped_zip_and_a_rollback_say_what_they_replaced() {
    let rig = Rig::new();
    rig.plant("Mine", "my own");
    rig.site.publish(&catalog_of(vec![publish_bundle(
        &rig.site,
        FOO,
        &["Foo", "FooLib"],
        &["Foo", "FooLib"],
        "1.0",
    )]));
    let mut h = rig.launch_settled();

    // Over a folder nothing else knows about. It's the player's own choice, so no question.
    let zip = rig.home.path().join("Dropped.zip");
    fs::write(&zip, addon_zip("Mine", "2.0")).unwrap();
    drop_file(&mut h, zip);
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Mine").as_deref(), Some("2.0"));
    assert!(said(
        &h,
        "The zip you dropped replaced Mine, and the old copy is gone."
    ));

    // The same zip again only replaces its own older copy, which isn't worth a word.
    app_mut(&mut h).messages = Default::default();
    let zip = rig.home.path().join("Newer.zip");
    fs::write(&zip, addon_zip("Mine", "3.0")).unwrap();
    drop_file(&mut h, zip);
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Mine").as_deref(), Some("3.0"));
    assert!(!said(&h, "replaced"), "{:?}", app(&h).messages.texts());

    // Over a part of a catalog addon, which goes on claiming it.
    click(&mut h, "Browse");
    click(&mut h, "Foo Addon, 1.0");
    click(&mut h, "Install");
    finish_ops(&mut h);
    let zip = rig.home.path().join("JustTheLib.zip");
    fs::write(&zip, addon_zip("FooLib", "9.9")).unwrap();
    drop_file(&mut h, zip);
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("FooLib").as_deref(), Some("9.9"));
    assert!(
        managed(&h, FOO).is_some(),
        "its main folder is still its own"
    );
    assert!(said(
        &h,
        "The zip you dropped replaced FooLib. Install Foo Addon again to get its own back."
    ));
    close(h);

    // A new version drops the folder. Then something else takes the name, and the old
    // version's zip, which still has it, is rolled back to.
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "2.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Foo Addon, update");
    click(&mut h, "Update");
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
    rig.plant("FooLib", "other");
    click(&mut h, "Roll back to 1.0");
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("FooLib").as_deref(), Some("1.0"));
    assert!(said(&h, "Rolling back replaced FooLib"));
}

#[test]
fn required_addons_land_first_and_missing_ones_warn() {
    let rig = Rig::new();
    let foo = publish_addon(&rig.site, FOO, "Foo", "1.0");
    let mut bar = publish_addon(&rig.site, BAR, "Bar", "1.0");
    bar.deps = vec![
        "Foo".to_owned(),
        "NotInCatalog".to_owned(),
        "Blizzard_Thing".to_owned(),
    ];
    let mut catalog = catalog_of(vec![bar, foo]);
    catalog.featured = vec![BAR.to_owned()];
    rig.site.publish(&catalog);

    let mut h = rig.launch_settled();
    click(&mut h, "Bar Addon, 1.0");
    assert!(
        has_label(&h, "Foo Addon"),
        "Requires shows it by name, as a link"
    );
    click(&mut h, "Install");
    let texts: Vec<String> = app(&h)
        .messages
        .texts()
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("requires NotInCatalog")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("requires Foo Addon, so that installs first")),
        "{texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("Blizzard")),
        "the game's own never count"
    );
    finish_ops(&mut h);
    assert!(rig.addons().join("Foo").exists());
    assert!(rig.addons().join("Bar").exists());
    assert!(managed(&h, FOO).is_some() && managed(&h, BAR).is_some());

    // Removing what the other one requires says so.
    click(&mut h, "My Addons");
    click(&mut h, "Foo Addon, 1.0");
    let bar_toc = rig.addons().join("Bar").join("Bar.toc");
    let text = fs::read_to_string(&bar_toc).unwrap() + "## Dependencies: Foo\n";
    fs::write(bar_toc, text).unwrap();
    app_mut(&mut h).run_slash("/reload");
    settle(&mut h);
    click(&mut h, "Remove");
    let Some(Popup::Remove(plan)) = &app(&h).popup else {
        panic!("Remove asks first");
    };
    assert_eq!(plan.dependents, vec!["Bar Addon".to_owned()]);
    assert!(has_label(
        &h,
        "These installed addons require it and may stop working: Bar Addon."
    ));
}

const PINS: &str = "direct:CoolPins";

#[test]
fn a_required_addon_from_a_stranger_stays_out_and_the_player_is_told() {
    let rig = Rig::new();
    // The only entry that provides it, and it says it's popular.
    let mut foo = publish_addon(&rig.site, FOO, "Foo", "1.0");
    foo.trusted = false;
    foo.downloads = 9_000_000;
    let mut bar = publish_addon(&rig.site, BAR, "Bar", "1.0");
    bar.deps = vec!["Foo".to_owned()];
    let mut catalog = catalog_of(vec![foo, bar]);
    catalog.featured = vec![BAR.to_owned()];
    rig.site.publish(&catalog);

    let mut h = rig.launch_settled();
    click(&mut h, "Bar Addon, 1.0");
    assert!(
        !has_label(&h, "Foo Addon"),
        "the page doesn't call a stranger's entry what this requires"
    );
    assert!(has_label(&h, "Foo"), "just the name of the folder");
    click(&mut h, "Install");
    assert!(app(&h).popup.is_none(), "never a popup for this");
    assert!(said(
        &h,
        "Bar Addon requires Foo, which this app can't pick for you. Look for it yourself, \
         in Browse or on the web. Installing Bar Addon anyway."
    ));
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Bar").as_deref(), Some("1.0"));
    assert!(!rig.addons().join("Foo").exists());
    assert!(managed(&h, FOO).is_none());
}

#[test]
fn a_required_addon_that_would_replace_something_is_left_out() {
    let rig = Rig::new();
    rig.plant("Questie", "mine");
    let foo = publish_bundle(
        &rig.site,
        FOO,
        &["Foo", "Questie"],
        &["Foo", "Questie"],
        "1.0",
    );
    let mut bar = publish_addon(&rig.site, BAR, "Bar", "1.0");
    bar.deps = vec!["Foo".to_owned()];
    let mut catalog = catalog_of(vec![foo, bar]);
    catalog.featured = vec![BAR.to_owned()];
    rig.site.publish(&catalog);

    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "Bar Addon, 1.0");
    click(&mut h, "Install");
    assert!(app(&h).popup.is_none(), "a dependency never opens a popup");
    assert!(said(
        &h,
        "Bar Addon requires Foo Addon, which wasn't added because it would replace Questie. \
         Install it from its own page to choose."
    ));
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Bar").as_deref(), Some("1.0"));
    assert!(!rig.addons().join("Foo").exists());
    assert_eq!(rig.version_on_disk("Questie").as_deref(), Some("mine"));
    assert!(managed(&h, FOO).is_none());
}

#[test]
fn replacing_a_folder_on_disk_asks_first_and_cancel_changes_nothing() {
    let rig = Rig::new();
    rig.plant("Questie", "mine");
    rig.site.publish(&catalog_of(vec![publish_bundle(
        &rig.site,
        PINS,
        &["CoolPins", "Questie"],
        &["CoolPins", "Questie"],
        "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "CoolPins Addon, 1.0");

    click(&mut h, "Install");
    assert!(matches!(app(&h).popup, Some(Popup::Replace(_))));
    assert!(app(&h).queue.is_idle(), "nothing has started");
    assert!(has_label(
        &h,
        "Installing CoolPins Addon would replace these folders already in your AddOns:"
    ));
    assert!(has_label(
        &h,
        "Questie: Questie from elsewhere, not installed by this app"
    ));
    click_last(&mut h, "Cancel");
    assert!(app(&h).popup.is_none());
    assert!(app(&h).queue.is_idle(), "and nothing starts");
    assert_eq!(rig.version_on_disk("Questie").as_deref(), Some("mine"));
    assert!(!rig.addons().join("CoolPins").exists());
    assert!(managed(&h, PINS).is_none());

    click(&mut h, "Install");
    press(&mut h, Key::Escape);
    assert!(app(&h).popup.is_none(), "Escape is Cancel too");
    assert!(app(&h).queue.is_idle());

    // Enter is the same button, so it's the same question.
    press(&mut h, Key::Enter);
    h.step();
    assert!(matches!(app(&h).popup, Some(Popup::Replace(_))));
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Questie").as_deref(), Some("1.0"));
    assert_eq!(rig.version_on_disk("CoolPins").as_deref(), Some("1.0"));
    assert_eq!(
        managed(&h, PINS).unwrap().folders,
        vec!["CoolPins".to_owned(), "Questie".to_owned()],
        "it's that addon's from here on"
    );

    // So it only ever asks once.
    click(&mut h, "Reinstall");
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
}

#[test]
fn a_zip_with_a_surprise_folder_is_refused_in_red() {
    const REFUSAL: &str = "This download would also replace Questie, which isn't part of this addon, so nothing was changed.";
    let rig = Rig::new();
    rig.plant("Questie", "mine");
    // The catalog says one thing and the zip brings another, so no question gets asked.
    let surprising = |version: &str| {
        publish_bundle(
            &rig.site,
            PINS,
            &["CoolPins"],
            &["CoolPins", "Questie"],
            version,
        )
    };
    rig.site.publish(&catalog_of(vec![surprising("1.0")]));
    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "CoolPins Addon, 1.0");
    click(&mut h, "Install");
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Questie").as_deref(), Some("mine"));
    assert!(!rig.addons().join("CoolPins").exists());
    assert!(managed(&h, PINS).is_none());
    assert!(
        app(&h).messages.errors().contains(&REFUSAL),
        "{:?}",
        app(&h).messages.texts()
    );
    assert!(has_label(&h, REFUSAL), "it's really on screen");
    close(h);

    // An addon the player already has can't start doing it in an update either.
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, PINS, "CoolPins", "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "CoolPins Addon, 1.0");
    click(&mut h, "Install");
    finish_ops(&mut h);
    close(h);
    rig.site.publish(&catalog_of(vec![surprising("2.0")]));
    let mut h = rig.launch_settled();
    click(&mut h, "Update all (1)");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Questie").as_deref(), Some("mine"));
    assert_eq!(rig.version_on_disk("CoolPins").as_deref(), Some("1.0"));
    assert_eq!(managed(&h, PINS).unwrap().version, "1.0");
    assert!(app(&h).messages.errors().contains(&REFUSAL));
    close(h);

    // A folder new to the disk costs nobody anything, listed or not.
    rig.site.publish(&catalog_of(vec![publish_bundle(
        &rig.site,
        PINS,
        &["CoolPins"],
        &["CoolPins", "CoolPins_Extra"],
        "3.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Update all (1)");
    finish_ops(&mut h);
    assert_eq!(
        rig.version_on_disk("CoolPins_Extra").as_deref(),
        Some("3.0")
    );
    assert_eq!(managed(&h, PINS).unwrap().version, "3.0");
    assert!(app(&h).messages.errors().is_empty());
}

#[test]
fn update_all_never_asks_and_leaves_out_what_would_replace_something_new() {
    const AAA: &str = "direct:Aaa";
    const BBB: &str = "direct:Bbb";
    const SKIPPED: &str = "Aaa Addon wasn't updated because it would replace Extra. \
                           Update it from its own page to choose.";
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![
        publish_addon(&rig.site, AAA, "Aaa", "1.0"),
        publish_addon(&rig.site, BBB, "Bbb", "1.0"),
    ]));
    let mut h = rig.launch_settled();
    app_mut(&mut h)
        .actions
        .push(Action::Install(vec![AAA.to_owned(), BBB.to_owned()]));
    h.step();
    finish_ops(&mut h);
    close(h);

    // Aaa's next version brings a folder that's already there, from somewhere else.
    rig.plant("Extra", "mine");
    rig.site.publish(&catalog_of(vec![
        publish_bundle(&rig.site, AAA, &["Aaa", "Extra"], &["Aaa", "Extra"], "2.0"),
        publish_addon(&rig.site, BBB, "Bbb", "2.0"),
    ]));
    let mut h = rig.launch_settled();
    click(&mut h, "Update all (2)");
    assert!(app(&h).popup.is_none());
    assert!(said(&h, SKIPPED), "{:?}", app(&h).messages.texts());
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Bbb").as_deref(), Some("2.0"));
    assert_eq!(rig.version_on_disk("Aaa").as_deref(), Some("1.0"));
    assert_eq!(rig.version_on_disk("Extra").as_deref(), Some("mine"));
    assert_eq!(app(&h).updates, vec![AAA.to_owned()]);

    // With only that one left, it still doesn't ask.
    app_mut(&mut h).messages = Default::default();
    click(&mut h, "Update all (1)");
    assert!(app(&h).popup.is_none());
    assert!(app(&h).queue.is_idle());
    assert!(said(&h, SKIPPED));
    assert_eq!(rig.version_on_disk("Aaa").as_deref(), Some("1.0"));

    // Its own page is where the player gets to say.
    click(&mut h, "Aaa Addon, update");
    click(&mut h, "Update");
    assert!(has_label(
        &h,
        "Updating Aaa Addon would replace these folders already in your AddOns:"
    ));
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Aaa").as_deref(), Some("2.0"));
    assert_eq!(rig.version_on_disk("Extra").as_deref(), Some("2.0"));
    close(h);

    // After that it's that addon's own, and Update all has nothing to ask about.
    rig.site.publish(&catalog_of(vec![
        publish_bundle(&rig.site, AAA, &["Aaa", "Extra"], &["Aaa", "Extra"], "3.0"),
        publish_addon(&rig.site, BBB, "Bbb", "2.0"),
    ]));
    let mut h = rig.launch_settled();
    click(&mut h, "Update all (1)");
    assert!(app(&h).popup.is_none());
    assert!(!said(&h, "wasn't updated"));
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Extra").as_deref(), Some("3.0"));
}

const DOMINOS: &str = "gh-like:Dominos";
const MASQUE: &str = "gh-like:Masque_Dominos";

/// Real Dominos ships `Masque_Dominos`, which another entry has as its main folder.
fn dominos(site: &Site, version: &str) -> Addon {
    let folders = ["Dominos", "Masque_Dominos"];
    publish_bundle(site, DOMINOS, &folders, &folders, version)
}

#[test]
fn dominos_shares_a_folder_and_only_asks_about_it_once() {
    let rig = Rig::new();
    let masque = publish_addon(&rig.site, MASQUE, "Masque_Dominos", "1.0");
    rig.site
        .publish(&catalog_of(vec![masque.clone(), dominos(&rig.site, "1.0")]));
    let mut h = rig.launch_settled();
    click(&mut h, "Masque_Dominos Addon, 1.0");
    click(&mut h, "Install");
    finish_ops(&mut h);

    // The first install over another addon's folder asks, naming that addon.
    click(&mut h, "Dominos Addon, 1.0");
    click(&mut h, "Install");
    assert!(has_label(
        &h,
        "Masque_Dominos: part of Masque_Dominos Addon"
    ));
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Dominos").as_deref(), Some("1.0"));
    let claims =
        |h: &Harness<'static, Option<App>>, id: &str| managed(h, id).unwrap().folders.clone();
    assert_eq!(
        claims(&h, DOMINOS),
        vec!["Dominos".to_owned(), "Masque_Dominos".to_owned()]
    );
    assert_eq!(claims(&h, MASQUE), vec!["Masque_Dominos".to_owned()]);
    close(h);

    // A plain update of either one, from its own page, says nothing about it.
    rig.site
        .publish(&catalog_of(vec![masque.clone(), dominos(&rig.site, "2.0")]));
    let mut h = rig.launch_settled();
    click(&mut h, "Dominos Addon, update");
    click(&mut h, "Update");
    assert!(app(&h).popup.is_none(), "no question on every update");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Dominos").as_deref(), Some("2.0"));
    assert_eq!(
        rig.version_on_disk("Masque_Dominos").as_deref(),
        Some("2.0")
    );
    assert!(app(&h).messages.errors().is_empty());
    close(h);

    let masque = publish_addon(&rig.site, MASQUE, "Masque_Dominos", "1.1");
    rig.site
        .publish(&catalog_of(vec![masque.clone(), dominos(&rig.site, "3.0")]));
    let mut h = rig.launch_settled();
    click(&mut h, "Update all (2)");
    assert!(app(&h).popup.is_none());
    assert!(!said(&h, "wasn't updated"));
    finish_ops(&mut h);
    assert_eq!(managed(&h, DOMINOS).unwrap().version, "3.0");
    assert_eq!(managed(&h, MASQUE).unwrap().version, "1.1");
    assert!(app(&h).messages.errors().is_empty());

    // Remove Dominos and the folder stays for the addon still claiming it.
    click(&mut h, "Dominos Addon, 3.0");
    click(&mut h, "Remove");
    click_last(&mut h, "Remove");
    finish_ops(&mut h);
    assert!(!rig.addons().join("Dominos").exists());
    assert!(rig.addons().join("Masque_Dominos").exists());
}

#[test]
fn a_shared_folder_the_zip_did_not_bring_is_only_asked_about_once() {
    // The catalog keeps one list of folders per addon, so a build can have fewer than it says.
    let rig = Rig::new();
    let without_it = |version: &str| {
        publish_bundle(
            &rig.site,
            DOMINOS,
            &["Dominos", "Masque_Dominos"],
            &["Dominos"],
            version,
        )
    };
    rig.site.publish(&catalog_of(vec![
        publish_addon(&rig.site, MASQUE, "Masque_Dominos", "1.0"),
        without_it("1.0"),
    ]));
    let mut h = rig.launch_settled();
    click(&mut h, "Masque_Dominos Addon, 1.0");
    click(&mut h, "Install");
    finish_ops(&mut h);
    click(&mut h, "Dominos Addon, 1.0");
    click(&mut h, "Install");
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(
        rig.version_on_disk("Masque_Dominos").as_deref(),
        Some("1.0")
    );
    for _ in 0..2 {
        click(&mut h, "Reinstall");
        assert!(app(&h).popup.is_none(), "it asked again");
        finish_ops(&mut h);
    }
    assert!(rig.addons().join("Masque_Dominos").exists());
    assert!(app(&h).messages.errors().is_empty());
}

#[test]
fn a_folder_the_zip_did_not_bring_stays_the_players_own() {
    let rig = Rig::new();
    rig.plant("Extra", "mine");
    let listed = ["Foo", "Extra"];
    rig.site.publish(&catalog_of(vec![publish_bundle(
        &rig.site,
        FOO,
        &listed,
        &["Foo"],
        "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "Foo Addon, 1.0");
    click(&mut h, "Install");
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(managed(&h, FOO).unwrap().folders, vec!["Foo".to_owned()]);
    assert_eq!(rig.version_on_disk("Extra").as_deref(), Some("mine"));

    // If it had gone in the record, this next install, whose zip doesn't have it either,
    // would delete a folder that was never replaced. So the question comes back instead.
    click(&mut h, "Reinstall");
    assert!(matches!(app(&h).popup, Some(Popup::Replace(_))));
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Extra").as_deref(), Some("mine"));
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("1.0"));
}

#[test]
fn a_long_list_of_folders_scrolls_and_the_buttons_stay_on_screen() {
    let rig = Rig::new();
    let names: Vec<String> = (0..60).map(|i| format!("Taken{i:02}")).collect();
    for name in &names {
        rig.plant(name, "mine");
    }
    let mut listed = vec!["Greedy"];
    listed.extend(names.iter().map(String::as_str));
    rig.site.publish(&catalog_of(vec![publish_bundle(
        &rig.site, FOO, &listed, &listed, "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "Greedy Addon, 1.0");
    click(&mut h, "Install");
    let button = h.get_by_label("Replace them").rect();
    let screen = h.ctx.content_rect();
    assert!(button.max.y <= screen.max.y, "{button:?} is off {screen:?}");
    assert!(has_label(&h, "and 20 more"), "the list stops somewhere");
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Taken59").as_deref(), Some("1.0"));
}

#[test]
fn dominos_on_its_own_needs_no_question_and_the_other_addon_asks_once() {
    let rig = Rig::new();
    let masque = publish_addon(&rig.site, MASQUE, "Masque_Dominos", "1.0");
    rig.site
        .publish(&catalog_of(vec![dominos(&rig.site, "1.0"), masque.clone()]));
    let mut h = rig.launch_settled();
    click(&mut h, "Dominos Addon, 1.0");
    click(&mut h, "Install");
    assert!(app(&h).popup.is_none(), "both folders are new to the disk");
    finish_ops(&mut h);
    assert_eq!(
        rig.version_on_disk("Masque_Dominos").as_deref(),
        Some("1.0")
    );

    click(&mut h, "Masque_Dominos Addon, 1.0");
    click(&mut h, "Install");
    assert!(has_label(&h, "Masque_Dominos: part of Dominos Addon"));
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert!(managed(&h, MASQUE).is_some());

    click(&mut h, "Reinstall");
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
}

#[test]
fn two_addons_in_one_batch_can_share_a_folder_new_to_the_disk() {
    let rig = Rig::new();
    let mut catalog = catalog_of(vec![
        publish_addon(&rig.site, MASQUE, "Masque_Dominos", "1.0"),
        dominos(&rig.site, "1.0"),
    ]);
    catalog.kits = vec![Kit {
        name: "Bars".to_owned(),
        description: "Bars and their skin.".to_owned(),
        addons: vec![MASQUE.to_owned(), DOMINOS.to_owned()],
    }];
    rig.site.publish(&catalog);
    let mut h = rig.launch_settled();
    click(&mut h, "Bars, 2 addons");
    click(&mut h, "Install all (2)");
    assert!(app(&h).popup.is_none());
    finish_ops(&mut h);
    assert!(
        app(&h).messages.errors().is_empty(),
        "the second didn't get refused for what the first just brought: {:?}",
        app(&h).messages.texts()
    );
    assert!(managed(&h, MASQUE).is_some() && managed(&h, DOMINOS).is_some());
}

#[test]
fn a_batch_leaves_out_what_would_replace_something_and_installs_the_rest() {
    let rig = Rig::new();
    rig.plant("Questie", "mine");
    let mut catalog = catalog_of(vec![
        publish_bundle(
            &rig.site,
            PINS,
            &["CoolPins", "Questie"],
            &["CoolPins", "Questie"],
            "1.0",
        ),
        publish_addon(&rig.site, FOO, "Foo", "1.0"),
    ]);
    catalog.kits = vec![Kit {
        name: "Handy Kit".to_owned(),
        description: "Two good things.".to_owned(),
        addons: vec![PINS.to_owned(), FOO.to_owned()],
    }];
    rig.site.publish(&catalog);
    let mut h = rig.launch_settled();
    click(&mut h, "Browse");
    click(&mut h, "Handy Kit, 2 addons");
    click(&mut h, "Install all (2)");
    assert!(app(&h).popup.is_none());
    assert!(said(
        &h,
        "CoolPins Addon wasn't installed because it would replace Questie. \
         Install it from its own page to choose."
    ));
    finish_ops(&mut h);
    assert!(rig.addons().join("Foo").exists());
    assert!(!rig.addons().join("CoolPins").exists());
    assert_eq!(rig.version_on_disk("Questie").as_deref(), Some("mine"));
}

#[test]
fn a_strangers_entry_is_not_matched_to_a_folder_on_disk() {
    let rig = Rig::new();
    // The player's own copy, carrying the id the entry claims and with the name it lists.
    let folder = rig.addons().join("Foo");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("Foo.toc"),
        format!("{FOREVER_TOC}## Title: Foo from elsewhere\n## Version: 0.5\n## X-WoWI-ID: 777\n"),
    )
    .unwrap();
    fs::write(folder.join("Foo.lua"), "-- 0.5\n").unwrap();
    let entry = |trusted: bool| {
        let mut addon = publish_addon(&rig.site, FOO, "Foo", "1.0");
        addon.ids.wowi = Some("777".to_owned());
        addon.downloads = 9_000_000;
        addon.trusted = trusted;
        addon
    };
    rig.site.publish(&catalog_of(vec![entry(false)]));

    let mut h = rig.launch_settled();
    assert!(app(&h).adoptions.is_empty());
    assert!(has_label(&h, "Other folders (1)"));
    click(&mut h, "Foo from elsewhere, 0.5");
    assert_eq!(app(&h).page_kind(), PageKind::Other("Foo".to_owned()));
    assert!(!has_label(&h, "Manage with this app"));

    // On its own page it's only something to install, and that asks about the folder first.
    click(&mut h, "Browse");
    click(&mut h, "Foo Addon, 1.0");
    assert!(has_label(&h, crate::model::UNTRUSTED_NOTE));
    assert!(has_label(&h, "Install") && !has_label(&h, "Manage with this app"));
    click(&mut h, "Install");
    assert!(has_label(
        &h,
        "Foo: Foo from elsewhere, not installed by this app"
    ));
    click_last(&mut h, "Cancel");
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("0.5"));
    close(h);

    // Once the catalog vouches for it, the same folder matches, and says who it's from.
    rig.site.publish(&catalog_of(vec![entry(true)]));
    let mut h = rig.launch_settled();
    click(&mut h, "Foo from elsewhere, 0.5");
    assert!(has_label(
        &h,
        "It matches Foo Addon (Direct link) in the catalog. Let this app manage it and it gets updates."
    ));
    click(&mut h, "Manage with this app");
    assert_eq!(managed(&h, FOO).unwrap().origin, Origin::Adopted);
}

#[test]
fn an_install_asked_for_with_no_game_yet_waits_for_the_first_look_at_the_disk() {
    let rig = Rig::new();
    rig.plant("Foo", "mine");
    let mut foo = publish_addon(&rig.site, FOO, "Foo", "1.0");
    foo.trusted = false;
    rig.site.publish(&catalog_of(vec![foo]));
    let launch = Launch {
        game: Some(rig.home.path().join("nowhere")),
        test_rig: true,
        catalog_url: Some(format!("{}/catalog.json", rig.site.base)),
        ..Launch::default()
    };
    let mut h = harness(launch, rig.dirs.clone(), rig.saved_state());
    settle(&mut h);
    click(&mut h, "Install");
    assert!(matches!(app(&h).popup, Some(Popup::Folder(_))));

    app_mut(&mut h)
        .actions
        .push(Action::BrowseTo(rig.game.clone()));
    pump(
        &mut h,
        "the listing",
        |a| matches!(&a.popup, Some(Popup::Folder(b)) if b.waiting_for.is_none()),
    );
    click(&mut h, "Use this folder");
    // Choosing the folder doesn't make its contents known yet. The question has to wait for
    // them, or the install would run blind and be refused for a folder nobody asked about.
    pump(&mut h, "the question about the folder", |a| {
        matches!(&a.popup, Some(Popup::Replace(_)))
    });
    assert!(app(&h).queue.is_idle());
    assert!(app(&h).messages.errors().is_empty());
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("mine"));
}

#[test]
fn a_folder_another_install_is_about_to_bring_counts_as_taken() {
    let rig = Rig::new();
    let slow = slow_addon(&rig);
    let bar = publish_bundle(&rig.site, BAR, &["Bar", "Foo"], &["Bar", "Foo"], "1.0");
    rig.site.publish(&catalog_of(vec![slow, bar]));
    let mut h = rig.launch_settled();
    click(&mut h, "Foo Addon, 1.0");
    click(&mut h, "Install");
    pump(&mut h, "the download to start", |a| {
        a.casting().is_some_and(|c| c.fraction > 0.0)
    });
    assert!(!rig.addons().join("Foo").exists(), "not on disk yet");
    click(&mut h, "Bar Addon, 1.0");
    click(&mut h, "Install");
    assert!(has_label(&h, "Foo: part of Foo Addon"));
    click_last(&mut h, "Cancel");
    click(&mut h, "Cancel");
    pump(&mut h, "the op to stop", |a| a.queue.is_idle());
}

#[test]
fn a_message_sits_below_an_open_popup_not_across_its_first_lines() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    app_mut(&mut h).yellow("Something else happened a moment ago.");
    h.step();
    h.step();
    let alone = h
        .get_by_label("Something else happened a moment ago.")
        .rect();
    let opened = Action::Install(vec![demo::UNTRUSTED_ID.to_owned()]);
    app_mut(&mut h).actions.push(opened);
    for _ in 0..4 {
        h.step();
    }
    let popup = h
        .ctx
        .memory(|m| m.area_rect(egui::Id::unique("static-popup")))
        .unwrap();
    let message = h
        .get_by_label("Something else happened a moment ago.")
        .rect();
    assert!(
        alone.min.y < popup.max.y && message.min.y >= popup.max.y,
        "{message:?} covers {popup:?}"
    );
    press(&mut h, Key::Escape);
    h.step();
    assert_eq!(
        h.get_by_label("Something else happened a moment ago.")
            .rect()
            .min
            .y,
        alone.min.y,
        "and it goes back where it was"
    );
}

#[test]
fn the_page_says_who_an_addon_really_comes_from() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let note = crate::model::UNTRUSTED_NOTE;
    click(&mut h, "Browse");

    // Trusted, from GitHub: the repo, and no note.
    click(&mut h, "Questie, update");
    assert!(has_label(&h, "GitHub: Questie/Questie"));
    assert!(!has_label(&h, note));

    click(&mut h, "Maps and Navigation (2)");
    click(&mut h, "TomTom, installed");
    assert!(has_label(&h, "WoWInterface"));
    assert!(!has_label(&h, note));

    // One nobody has vouched for.
    click(&mut h, "Quests and Leveling (4)");
    click(&mut h, "Shortest Path Forever, 1.3.0");
    assert!(has_label(&h, "GitHub: cjber/shortest-path-forever"));
    assert!(has_label(&h, note));

    // It claims folders the fake disk already has, so the demo can show the question too.
    click(&mut h, "Install");
    assert!(has_label(
        &h,
        "Installing Shortest Path Forever would replace these folders already in your AddOns:"
    ));
    assert!(has_label(
        &h,
        "HandyNotes_Extras: HandyNotes Extras, not installed by this app"
    ));
    assert!(has_label(&h, "Pawn: part of Pawn"));
    assert!(has_label(&h, "Source: GitHub: cjber/shortest-path-forever"));
    let popup = h
        .ctx
        .memory(|m| m.area_rect(egui::Id::unique("static-popup")))
        .unwrap();
    assert!(
        popup.max.y >= h.get_by_label("Replace them").rect().max.y,
        "the popup holds its buttons"
    );

    // Waiting on the player costs no frames.
    h.run();
    let delay = h.output().viewport_output[&ViewportId::ROOT].repaint_delay;
    assert!(delay > Duration::from_secs(60 * 60), "{delay:?}");

    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert!(managed(&h, demo::UNTRUSTED_ID).is_some());
}

#[test]
fn a_very_long_repo_name_stays_on_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let long = format!("{}/{}", "o".repeat(39), "r".repeat(100));
    let mut catalog = demo::catalog();
    catalog.addons[0].source = Source::Github { repo: long.clone() };
    app_mut(&mut h).bundle = Some(Arc::new(crate::model::CatalogBundle::new(catalog)));
    click(&mut h, "Browse");
    click(&mut h, "Questie, update");
    let link = h.get_by_label(&format!("GitHub: {long}")).rect();
    let share = h.get_by_label("Share").rect();
    assert!(link.width() > 100.0);
    let page_right = h.ctx.content_rect().max.x - 20.0;
    assert!(
        link.max.x <= page_right,
        "{link:?} runs off past {page_right}"
    );
    assert!(link.max.y < share.min.y);
}

#[test]
fn update_all_backs_up_settings_first_and_counts_on_the_bar() {
    let rig = Rig::new();
    let names = ["Aaa", "Bbb", "Ccc", "Ddd"];
    let wtf = rig.game.join("_forever_").join("WTF");
    fs::create_dir_all(wtf.join("Account")).unwrap();
    fs::write(wtf.join("Account").join("SavedVariables.lua"), "x = 1").unwrap();
    rig.set_up_state(|state| state.settings.backup_wtf = true);
    let v1: Vec<Addon> = names
        .iter()
        .map(|n| publish_addon(&rig.site, &format!("direct:{n}"), n, "1.0"))
        .collect();
    rig.site.publish(&catalog_of(v1));

    let mut h = rig.launch_settled();
    let ids: Vec<String> = names.iter().map(|n| format!("direct:{n}")).collect();
    app_mut(&mut h).actions.push(Action::Install(ids));
    h.step();
    assert_eq!(app(&h).casting().unwrap().text, "Installing 1 of 4");
    finish_ops(&mut h);
    let backups = rig.dirs.backups();
    assert!(
        !backups.exists(),
        "installing isn't updating, so no backup yet"
    );
    close(h);

    let v2: Vec<Addon> = names
        .iter()
        .map(|n| publish_addon(&rig.site, &format!("direct:{n}"), n, "2.0"))
        .collect();
    rig.site.publish(&catalog_of(v2));
    let mut h = rig.launch_settled();
    assert!(has_label(&h, "Update all (4)"));
    click(&mut h, "Update all (4)");
    let cast = app(&h).casting().unwrap();
    assert_eq!(
        cast.text, "Backing up your settings",
        "the bar shows the backup first"
    );
    finish_ops(&mut h);
    for name in names {
        assert_eq!(rig.version_on_disk(name).as_deref(), Some("2.0"));
    }
    let made = fs::read_dir(&backups).unwrap().count();
    assert_eq!(made, 1, "exactly one backup");
    assert!(
        app(&h).state.current().unwrap().last_wtf_backup > 0,
        "and it's remembered, so tomorrow is soon enough for the next"
    );
    assert_eq!(app(&h).state.stats.updates_applied, 4);
}

#[test]
fn no_game_asks_for_a_folder_and_then_carries_on_with_the_install() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let launch = Launch {
        game: Some(rig.home.path().join("nowhere")),
        test_rig: true,
        catalog_url: Some(format!("{}/catalog.json", rig.site.base)),
        ..Launch::default()
    };
    let mut h = harness(launch, rig.dirs.clone(), rig.saved_state());
    settle(&mut h);
    assert_eq!(app(&h).game_state, GameState::NotFound);
    assert_eq!(app(&h).tab, Tab::Browse, "browsing still works");
    assert!(app(&h).status_text().starts_with("WoW Forever not found"));

    click(&mut h, "My Addons");
    assert_eq!(app(&h).page_kind(), PageKind::NoGame);
    assert!(has_label(&h, "Couldn't find WoW Forever."));
    assert!(has_label(&h, "Choose folder") && has_label(&h, "Look again"));

    click(&mut h, "Browse");
    click(&mut h, "Install");
    assert!(
        matches!(app(&h).popup, Some(Popup::Folder(_))),
        "asks for a folder first"
    );
    assert!(!rig.addons().join("Foo").exists());

    // Walk to the game with the built-in browser.
    let bad = rig.home.path().to_path_buf();
    app_mut(&mut h).actions.push(Action::BrowseTo(bad));
    pump(
        &mut h,
        "the listing",
        |a| matches!(&a.popup, Some(Popup::Folder(b)) if b.waiting_for.is_none()),
    );
    assert!(has_label(&h, "World of Warcraft"), "subfolders are listed");
    assert!(
        h.get_by_label("Use this folder")
            .accesskit_node()
            .is_disabled(),
        "this folder isn't the game, so it can't be used"
    );
    click(&mut h, "World of Warcraft");
    pump(
        &mut h,
        "the listing",
        |a| matches!(&a.popup, Some(Popup::Folder(b)) if b.waiting_for.is_none()),
    );
    click(&mut h, "Use this folder");
    finish_ops(&mut h);
    assert_eq!(
        rig.version_on_disk("Foo").as_deref(),
        Some("1.0"),
        "and the install carried on"
    );
    assert!(matches!(app(&h).game_state, GameState::Found { .. }));
}

#[test]
fn cancelling_the_folder_browser_drops_the_request() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let launch = Launch {
        game: Some(rig.home.path().join("nowhere")),
        test_rig: true,
        catalog_url: Some(format!("{}/catalog.json", rig.site.base)),
        ..Launch::default()
    };
    let mut h = harness(launch, rig.dirs.clone(), rig.saved_state());
    settle(&mut h);
    click(&mut h, "Install");
    assert!(!app(&h).after_folder.is_empty());
    press(&mut h, Key::Escape);
    assert!(app(&h).popup.is_none());
    assert!(app(&h).after_folder.is_empty());
}

#[test]
fn a_catalog_that_is_not_published_yet_is_handled_honestly() {
    let rig = Rig::new();
    let mut h = rig.launch_settled();
    assert_eq!(
        app(&h).catalog_status,
        CatalogStatus::Missing { offline: false }
    );
    assert_eq!(app(&h).page_kind(), PageKind::NoCatalog);
    assert!(has_label(&h, "No catalog yet."));
    assert!(app(&h).status_text().ends_with("No catalog yet"));
    assert!(has_label(&h, "Try again"));

    // It appears, and Try again finds it.
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    click(&mut h, "Try again");
    settle(&mut h);
    assert_eq!(app(&h).catalog_status, CatalogStatus::Fresh);
    assert!(has_label(&h, "Foo Addon, 1.0"));
}

#[test]
fn first_run_offline_says_so_and_a_cached_catalog_keeps_working() {
    let rig = Rig::new();
    // Nothing listens here.
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let launch = || Launch {
        game: Some(rig.game.clone()),
        test_rig: true,
        catalog_url: Some(format!("http://127.0.0.1:{dead}/catalog.json")),
        ..Launch::default()
    };
    let mut h = harness(launch(), rig.dirs.clone(), rig.saved_state());
    settle(&mut h);
    assert_eq!(
        app(&h).catalog_status,
        CatalogStatus::Missing { offline: true }
    );
    assert!(has_label(&h, "You're offline."));
    assert!(app(&h).status_text().ends_with("Offline, no catalog yet"));
    close(h);

    // Fetch it once while online, then go offline again.
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let h = rig.launch_settled();
    assert_eq!(app(&h).catalog_status, CatalogStatus::Fresh);
    close(h);
    assert!(rig.dirs.cache.join("catalog.json").exists());

    let mut h = harness(launch(), rig.dirs.clone(), rig.saved_state());
    settle(&mut h);
    assert_eq!(app(&h).catalog_status, CatalogStatus::Offline);
    assert!(
        app(&h)
            .status_text()
            .ends_with("Offline, showing the last catalog")
    );
    assert!(has_label(&h, "Foo Addon, 1.0"));
}

#[test]
fn an_unchanged_catalog_costs_one_answer_and_keeps_the_cache() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let h = rig.launch_settled();
    assert_eq!(app(&h).catalog_status, CatalogStatus::Fresh);
    close(h);
    assert!(rig.saved_state().catalog_etag.is_some());

    let h = rig.launch_settled();
    assert_eq!(app(&h).catalog_status, CatalogStatus::Fresh);
    assert_eq!(app(&h).bundle.as_ref().unwrap().catalog.addons.len(), 1);
}

#[test]
fn starter_kits_and_gaps_work_end_to_end() {
    let rig = Rig::new();
    let foo = publish_addon(&rig.site, FOO, "Foo", "1.0");
    let bar = publish_addon(&rig.site, BAR, "Bar", "1.0");
    let mut catalog = catalog_of(vec![foo, bar]);
    catalog.kits = vec![Kit {
        name: "Handy Kit".to_owned(),
        description: "Two good things.".to_owned(),
        addons: vec![FOO.to_owned(), BAR.to_owned(), "direct:Missing".to_owned()],
    }];
    catalog.gaps = vec![Gap {
        name: "Bigger Thing".to_owned(),
        reason: "It isn't here.".to_owned(),
        website: Some("file:///etc/passwd".to_owned()),
        alternatives: vec![FOO.to_owned()],
    }];
    rig.site.publish(&catalog);

    let mut h = rig.launch_settled();
    click(&mut h, "Handy Kit, 2 addons");
    assert!(has_label(&h, "Two good things."));
    click(&mut h, "Install all (2)");
    finish_ops(&mut h);
    assert!(rig.addons().join("Foo").exists() && rig.addons().join("Bar").exists());

    // A gap's page never offers a link that isn't plainly a web address.
    app_mut(&mut h).query = "bigger".to_owned();
    app_mut(&mut h).mark_rows_dirty();
    h.step();
    click(&mut h, "Bigger Thing, not here");
    assert_eq!(app(&h).page_kind(), PageKind::Gap(0));
    assert!(has_label(&h, "Not available here"));
    assert!(
        h.get_by_label("Open its page")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, "Foo Addon, installed");
    assert_eq!(app(&h).page_kind(), PageKind::Addon(FOO.to_owned()));
}

#[test]
fn links_only_reach_the_browser_when_they_are_web_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let opened = |h: &Harness<'static, Option<App>>| {
        h.output()
            .platform_output
            .commands
            .iter()
            .filter(|c| matches!(c, OutputCommand::OpenUrl(_)))
            .count()
    };
    for bad in [
        "file:///c:/windows",
        "steam://run/1",
        "javascript:alert(1)",
        "example.com",
    ] {
        app_mut(&mut h)
            .actions
            .push(Action::OpenLink(bad.to_owned()));
        // Applied at the end of this frame, so the command goes out with the same output.
        h.step();
        assert_eq!(opened(&h), 0, "{bad} must not open");
    }
    app_mut(&mut h)
        .actions
        .push(Action::OpenLink("https://example.com/x".to_owned()));
    h.step();
    assert_eq!(opened(&h), 1);
}

#[test]
fn the_demo_stays_offline_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    assert_eq!(app(&h).tab, Tab::MyAddons);
    assert_eq!(
        app(&h).status_text(),
        "Forever 1.60.1  |  18 addons  |  updated 2 hours ago"
    );
    // Update everything, the whole way through.
    assert!(has_label(&h, "Update all (3)"));
    click(&mut h, "Update all (3)");
    finish_ops(&mut h);
    assert!(app(&h).updates.is_empty());
    assert!(has_label(&h, "Update all (0)"));
    // Remove something, install something, drop something.
    click(&mut h, "Questie, v12.6.2");
    click(&mut h, "Roll back to v12.5.9");
    finish_ops(&mut h);
    assert_eq!(
        managed(&h, "gh:Questie/Questie").unwrap().version,
        "v12.5.9"
    );
    click(&mut h, "Remove");
    click_last(&mut h, "Remove");
    finish_ops(&mut h);
    assert!(managed(&h, "gh:Questie/Questie").is_none());
    click(&mut h, "Browse");
    click(&mut h, "Questie, v12.6.2");
    click(&mut h, "Install");
    finish_ops(&mut h);
    assert!(managed(&h, "gh:Questie/Questie").is_some());
    app_mut(&mut h)
        .actions
        .push(Action::ZipDropped(PathBuf::from("Thing.zip")));
    h.step();
    finish_ops(&mut h);
    assert!(managed(&h, "local:Thing").is_some());

    drop(h);
    let left: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert!(left.is_empty(), "the demo wrote {left:?}");
}

#[test]
fn every_scene_draws_and_settles() {
    let dir = tempfile::tempdir().unwrap();
    for scene in [
        Scene::Normal,
        Scene::Empty,
        Scene::NoGame,
        Scene::Offline,
        Scene::NoCatalog,
        Scene::NoCatalogOffline,
        Scene::Loading,
    ] {
        let mut h = demo_harness(scene, dir.path());
        // `run` stops once nothing asks for another frame right away, and it checks that
        // every control has a name a screen reader can read.
        h.run();
        for tab in ["My Addons", "Browse", "Settings"] {
            click(&mut h, tab);
            h.run();
        }
    }
}

#[test]
fn every_settings_page_draws_and_can_be_operated() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    click(&mut h, "Settings");
    h.run();
    assert_eq!(app(&h).section, Section::Game);
    assert!(has_label(&h, "Game folder") && has_label(&h, "Through Battle.net"));

    click(&mut h, "Backups");
    assert_eq!(app(&h).section, Section::Backups);
    let before = app(&h).state.settings.backup_wtf;
    click(&mut h, "Back up settings before updating, once a day");
    assert_eq!(app(&h).state.settings.backup_wtf, !before);
    let keep = app(&h).state.settings.keep_previous_version;
    click(&mut h, "Keep the previous version of each addon");
    assert_eq!(app(&h).state.settings.keep_previous_version, !keep);
    h.step();
    assert_eq!(h.get_all_by_label("Restore").count(), 3);
    h.get_all_by_label("Restore").next().unwrap().click();
    h.step();
    h.step();
    h.step();
    assert!(matches!(app(&h).popup, Some(Popup::Restore { .. })));
    click_last(&mut h, "Restore");
    finish_ops(&mut h);
    assert!(app(&h).messages.texts().contains(&"Settings restored."));
    click(&mut h, "Back up now");
    finish_ops(&mut h);

    click(&mut h, "Addon lists");
    assert!(
        h.get_by_label("Install these")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, "Copy my list");
    let copied =
        h.output().platform_output.commands.iter().any(
            |c| matches!(c, OutputCommand::CopyText(text) if text.contains("gh:Questie/Questie")),
        );
    let said = app(&h)
        .messages
        .texts()
        .iter()
        .any(|t| t.starts_with("Copied your list"));
    assert!(copied || said, "the list goes to the clipboard");

    app_mut(&mut h).list_text =
        "# a list\ngh:Questie/Questie\ngh:Nevcairiel/GatherMate2 @beta\nwowi:999\nrm -rf /\n"
            .to_owned();
    h.step();
    click(&mut h, "Install these");
    let Some(Popup::List(preview)) = &app(&h).popup else {
        panic!("a list is shown before anything happens");
    };
    assert_eq!(
        preview.add,
        vec![(
            "gh:Nevcairiel/GatherMate2".to_owned(),
            "GatherMate2".to_owned(),
            Channel::Beta
        )]
    );
    assert_eq!(preview.have, vec!["Questie".to_owned()]);
    assert_eq!(
        preview.unknown.len(),
        2,
        "unknown ids and strange lines are shown, not run"
    );
    click_last(&mut h, "Install these");
    // Its folder is on disk already, from some other tool. Same question as its own button.
    assert!(matches!(app(&h).popup, Some(Popup::Replace(_))));
    click(&mut h, "Replace them");
    finish_ops(&mut h);
    assert_eq!(
        managed(&h, "gh:Nevcairiel/GatherMate2").unwrap().channel,
        Channel::Beta
    );

    click(&mut h, "Display");
    assert_eq!(app(&h).section, Section::Display);
    // The slider itself, the way a screen reader or the keyboard would move it.
    h.get_by_role_and_label(egui::accesskit::Role::Slider, "UI scale")
        .increment_accesskit();
    h.step();
    h.step();
    assert!((app(&h).state.settings.ui_scale - 1.05).abs() < 0.001);
    app_mut(&mut h).actions.push(Action::SetScale(1.5));
    h.step();
    h.step();
    assert_eq!(h.ctx.zoom_factor(), 1.5);
    assert_eq!(app(&h).state.settings.ui_scale, 1.5);
    app_mut(&mut h).actions.push(Action::SetScale(1.0));
    h.step();
    h.step();

    click(&mut h, "App updates");
    assert_eq!(app(&h).section, Section::Updates);
    let on = app(&h).state.settings.check_app_updates;
    click(&mut h, "Check for a new version at launch");
    assert_eq!(app(&h).state.settings.check_app_updates, !on);
    click(&mut h, "Check now");
    h.step();
    assert!(
        app(&h)
            .messages
            .texts()
            .iter()
            .any(|t| t.starts_with("You have the latest version"))
    );

    click(&mut h, "About");
    assert_eq!(app(&h).section, Section::About);
    assert!(has_label(&h, "Watch the video"));
    assert!(has_label(&h, "Source code"));
    assert!(has_label(
        &h,
        "World of Warcraft is a trademark of Blizzard Entertainment, Inc. This project is not affiliated with or endorsed by Blizzard Entertainment."
    ));
    click(&mut h, "Copy diagnostics");
    h.step();
    assert!(
        app(&h)
            .messages
            .texts()
            .contains(&"Copied the diagnostics to the clipboard.")
    );
    h.run();
}

#[test]
fn popup_buttons_never_sit_on_top_of_the_notes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    app_mut(&mut h).popup = Some(Popup::AppUpdate(Box::new(demo::update_info())));
    h.step();
    h.step();
    h.step();
    let notes = h.get_by_label_contains("What changed").rect();
    let button = h.get_by_label("Update now").rect();
    assert!(button.min.y > notes.max.y, "{button:?} overlaps {notes:?}");
    let popup = h
        .ctx
        .memory(|m| m.area_rect(egui::Id::unique("static-popup")))
        .unwrap();
    assert!(popup.max.y >= button.max.y, "the popup holds its buttons");
}

#[test]
fn the_update_prompt_offers_exactly_what_the_spec_says() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let show = |h: &mut Harness<'static, Option<App>>| {
        app_mut(h).popup = Some(Popup::AppUpdate(Box::new(demo::update_info())));
        h.step();
        h.step();
    };
    show(&mut h);
    assert!(has_label(&h, "Version 0.2.0 is ready."));
    assert!(has_label(&h, "(You have 0.1.0.)"));
    assert!(has_label(&h, "Update now") && has_label(&h, "Later"));
    click(&mut h, "Later");
    assert!(app(&h).popup.is_none());

    show(&mut h);
    click(&mut h, "Skip this version");
    assert_eq!(app(&h).state.skip_app_version.as_deref(), Some("0.2.0"));

    show(&mut h);
    press(&mut h, Key::Escape);
    assert!(app(&h).popup.is_none(), "Escape cancels");

    show(&mut h);
    click(&mut h, "Update now");
    assert!(app(&h).popup.is_none());
    assert_eq!(app(&h).casting().unwrap().text, "Updating to 0.2.0");
    finish_ops(&mut h);
}

#[test]
fn the_keyboard_alone_can_drive_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let selected = |h: &Harness<'static, Option<App>>| app(h).selection().cloned();
    let first = Selection::Addon("gh:DeadlyBossMods/DeadlyBossMods".to_owned());
    assert_eq!(selected(&h), Some(first.clone()));

    press(&mut h, Key::ArrowDown);
    press(&mut h, Key::ArrowDown);
    assert_eq!(
        selected(&h),
        Some(Selection::Addon("gh:Questie/Questie".to_owned()))
    );
    // Over the "Installed" header in one press.
    press(&mut h, Key::ArrowDown);
    assert_eq!(
        selected(&h),
        Some(Selection::Addon("gh:Nevcairiel/Bartender4".to_owned()))
    );
    press(&mut h, Key::ArrowUp);
    press(&mut h, Key::ArrowUp);
    press(&mut h, Key::ArrowUp);
    press(&mut h, Key::ArrowUp);
    press(&mut h, Key::ArrowUp);
    assert_eq!(selected(&h), Some(first), "and it stops at the top");

    // Enter presses the page's main button.
    press(&mut h, Key::Enter);
    assert!(app(&h).casting().is_some());
    finish_ops(&mut h);
    assert_eq!(
        managed(&h, "gh:DeadlyBossMods/DeadlyBossMods")
            .unwrap()
            .version,
        "12.0.4"
    );

    // A click on a row leaves the keyboard with the list, so Enter still works after it.
    click(&mut h, "Bartender4, 4.18.4");
    press(&mut h, Key::Enter);
    assert_eq!(app(&h).casting().unwrap().text, "Reinstalling Bartender4");
    finish_ops(&mut h);

    // Ctrl+F focuses search, typing filters, Enter gives the keyboard back without
    // installing anything, Escape clears.
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.step();
    h.step();
    assert!(h.ctx.memory(|m| m.focused()) == Some(crate::views::toolbar::search_id()));
    type_text(&mut h, "pawn");
    assert_eq!(app(&h).query, "pawn");
    assert_eq!(app(&h).rows.len(), 2, "its header, and the one row");
    assert_eq!(
        selected(&h),
        Some(Selection::Addon("gh:VgerMods/Pawn".to_owned()))
    );
    press(&mut h, Key::Enter);
    assert!(h.ctx.memory(|m| m.focused()).is_none());
    assert!(
        app(&h).casting().is_none(),
        "no install from pressing Enter in search"
    );
    press(&mut h, Key::Escape);
    assert_eq!(app(&h).query, "");
    assert!(app(&h).rows.len() > 5);

    // `/` from anywhere focuses search and starts a command.
    type_text(&mut h, "/");
    assert!(h.ctx.memory(|m| m.focused()) == Some(crate::views::toolbar::search_id()));
    assert_eq!(app(&h).query, "/");
    press(&mut h, Key::Escape);
    assert_eq!(app(&h).query, "");
    press(&mut h, Key::Escape);
    assert!(
        h.ctx.memory(|m| m.focused()).is_none(),
        "a second Escape lets go"
    );
}

#[test]
fn slash_commands_do_what_they_say_and_clear_the_box() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let command = |h: &mut Harness<'static, Option<App>>, line: &str| {
        app_mut(h).focus_search = true;
        h.step();
        type_text(h, line);
        let rows_before = app(h).rows.len();
        assert_eq!(app(h).query, line);
        assert_eq!(rows_before, app(h).rows.len(), "a command isn't a search");
        press(h, Key::Enter);
        assert_eq!(app(h).query, "", "{line} clears the box");
    };

    command(&mut h, "/help");
    assert!(
        app(&h)
            .messages
            .texts()
            .iter()
            .any(|t| t.starts_with("Commands:"))
    );
    command(&mut h, "/played");
    assert!(
        app(&h)
            .messages
            .texts()
            .iter()
            .any(|t| t.starts_with("Time spent"))
    );
    command(&mut h, "/nonsense");
    assert!(
        app(&h)
            .messages
            .texts()
            .iter()
            .any(|t| t.contains("Try /help"))
    );

    command(&mut h, "/dance");
    assert!(app(&h).fun.is_dancing(Instant::now()));
    assert_eq!(
        h.output().viewport_output[&ViewportId::ROOT].repaint_delay,
        Duration::ZERO,
        "a dance needs frames"
    );
    command(&mut h, "/ding");
    assert!(app(&h).messages.texts().contains(&"Ding!"));
    command(&mut h, "/pwnage");
    assert!(app(&h).fun.pwnage_showing(Instant::now()));
    assert!(
        has_label(&h, "Watch the video"),
        "the link is in a yellow message"
    );
    command(&mut h, "/reload");
    assert!(app(&h).messages.texts().contains(&"Reloading your addons."));
    command(&mut h, "/feeling");
    assert_eq!(app(&h).tab, Tab::Settings);
    assert_eq!(app(&h).section, Section::About);
}

#[test]
fn five_clicks_on_the_portrait_clean_the_monitor() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    for _ in 0..5 {
        click(&mut h, "Portrait");
    }
    assert!(app_mut(&mut h).fun.gag.is_off(Instant::now()));
    assert!(app(&h).messages.texts().contains(&easter::CLEANING_MESSAGE));
}

#[test]
fn sorting_and_collapsing_rebuild_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    click(&mut h, "Browse");
    assert!(has_label(&h, "Starter kits (2)"));
    assert!(has_label(&h, "Quests and Leveling (4)"));
    assert!(
        !has_label(&h, "RestedXP Guide, 4.9.1"),
        "categories start closed"
    );
    click(&mut h, "Quests and Leveling (4)");
    assert!(has_label(&h, "RestedXP Guide, 4.9.1"));
    click(&mut h, "Quests and Leveling (4)");
    assert!(!has_label(&h, "RestedXP Guide, 4.9.1"));
    click(&mut h, "Starter kits (2)");
    assert!(!has_label(&h, "New Adventurer, 3 addons"));

    click(&mut h, "Sort by: Most popular");
    assert!(has_label(&h, "A to Z"));
    click(&mut h, "A to Z");
    assert_eq!(app(&h).sort, feeling_core::search::SortBy::Name);
    assert!(has_label(&h, "Sort by: A to Z"));

    // Sort only shows on Browse.
    click(&mut h, "My Addons");
    assert!(!has_label(&h, "Sort by: A to Z"));
}

#[test]
fn escape_closes_an_open_menu_without_touching_the_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    click(&mut h, "Browse");
    app_mut(&mut h).query = "boss".to_owned();
    app_mut(&mut h).mark_rows_dirty();
    h.step();
    click(&mut h, "Sort by: Most popular");
    assert!(has_label(&h, "A to Z"));
    press(&mut h, Key::Escape);
    assert!(!has_label(&h, "A to Z"), "the menu closed");
    assert_eq!(app(&h).query, "boss", "and the search is still there");
}

#[test]
fn a_long_list_only_draws_what_shows_and_the_arrows_scroll_it() {
    let rig = Rig::new();
    let addons: Vec<Addon> = (0..400)
        .map(|i| {
            let mut addon = publish_addon(
                &rig.site,
                &format!("direct:A{i}"),
                &format!("A{i:03}"),
                "1.0",
            );
            addon.downloads = 1000 - i;
            addon
        })
        .collect();
    let mut catalog = catalog_of(addons);
    catalog.featured.clear();
    rig.site.publish(&catalog);

    let mut h = rig.launch_settled();
    click(&mut h, "Everything Else (400)");
    h.step();
    assert_eq!(app(&h).rows.len(), 401);
    let drawn = h
        .query_all_by_role(egui::accesskit::Role::Button)
        .filter(|node| {
            node.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with("A0") || l.starts_with("A1"))
        })
        .count();
    assert!(drawn < 40, "only the visible rows exist, but {drawn} do");

    assert!(
        !has_label(&h, "A100 Addon, 1.0"),
        "too far down to be drawn yet"
    );
    for _ in 0..100 {
        h.key_press(Key::ArrowDown);
        h.step();
    }
    h.step();
    assert_eq!(
        app(&h).selection(),
        Some(&Selection::Addon("direct:A100".to_owned()))
    );
    assert!(
        has_label(&h, "A100 Addon, 1.0"),
        "the list followed the selection"
    );
    assert!(app(&h).list_offset > 0.0);
    assert!(
        !has_label(&h, "A000 Addon, 1.0"),
        "and the top has scrolled away"
    );

    // Back up past where the view starts.
    for _ in 0..100 {
        h.key_press(Key::ArrowUp);
        h.step();
    }
    h.step();
    assert!(has_label(&h, "A000 Addon, 1.0"));
}

#[test]
fn an_addon_listed_twice_is_selected_where_it_was_clicked() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    click(&mut h, "Browse");
    click(&mut h, "Quests and Leveling (4)");
    assert_eq!(h.get_all_by_label("Questie, update").count(), 2);
    h.get_all_by_label("Questie, update")
        .last()
        .unwrap()
        .click();
    h.step();
    h.step();
    press(&mut h, Key::ArrowDown);
    assert_eq!(
        app(&h).selection(),
        Some(&Selection::Addon("gh:RestedXP/RXPGuides".to_owned())),
        "the arrow carries on from the row that was clicked, not the first one"
    );
}

#[test]
fn switching_a_managed_addon_to_beta_offers_the_beta() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    click(&mut h, "Questie, update");
    assert!(has_label(&h, "v12.5.9 installed, v12.6.2 available"));
    click(&mut h, "Use beta versions");
    assert_eq!(
        managed(&h, "gh:Questie/Questie").unwrap().channel,
        Channel::Beta
    );
    h.step();
    assert!(has_label(&h, "v12.5.9 installed, v12.7.0-beta available"));

    // An addon with no beta at all has nothing to switch to.
    click(&mut h, "Bartender4, 4.18.4");
    assert!(
        h.get_by_label("Use beta versions")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn a_new_install_can_choose_beta_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Empty, dir.path());
    click(&mut h, "Browse");
    click(&mut h, "Questie, v12.6.2");
    assert!(
        h.get_by_label("Don't update this addon")
            .accesskit_node()
            .is_disabled(),
        "there's nothing to pin until it's installed"
    );
    click(&mut h, "Use beta versions");
    click(&mut h, "Install");
    finish_ops(&mut h);
    let record = managed(&h, "gh:Questie/Questie").unwrap();
    assert_eq!(record.channel, Channel::Beta);
    assert_eq!(record.version, "v12.7.0-beta");
}

#[test]
fn a_kept_zip_that_went_missing_takes_rollback_with_it() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    finish_ops(&mut h);
    close(h);
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "2.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Foo Addon, update");
    click(&mut h, "Update");
    finish_ops(&mut h);
    assert!(has_label(&h, "Roll back to 1.0"));

    let zip = managed(&h, FOO).unwrap().previous_zip.clone().unwrap();
    fs::remove_file(zip).unwrap();
    app_mut(&mut h).run_slash("/reload");
    settle(&mut h);
    assert!(!has_label(&h, "Roll back to 1.0"));
    assert_eq!(managed(&h, FOO).unwrap().previous_version, None);
}

#[test]
fn with_keep_previous_off_nothing_is_kept_for_rollback() {
    let rig = Rig::new();
    rig.set_up_state(|state| state.settings.keep_previous_version = false);
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    finish_ops(&mut h);
    close(h);
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "2.0",
    )]));
    let mut h = rig.launch_settled();
    click(&mut h, "Foo Addon, update");
    click(&mut h, "Update");
    finish_ops(&mut h);
    assert_eq!(rig.version_on_disk("Foo").as_deref(), Some("2.0"));
    assert!(!has_label(&h, "Roll back to 1.0"));
    assert!(!rig.dirs.cache.join("zips").exists());
}

#[test]
fn worker_reports_reach_the_player_in_the_right_color() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let worker = app(&h).worker.clone();
    worker.send(Event::GameRunning(true));
    worker.send(Event::Recovered(Ok(vec![
        "An interrupted install was undone.".to_owned(),
    ])));
    worker.send(Event::Recovered(Err("A file stays locked.".to_owned())));
    h.step();
    h.step();
    h.step();
    let texts = app(&h).messages.texts();
    assert!(texts.iter().any(|t| {
        *t == "WoW is running. Type /reload in game to load updates. New addons appear after restarting the game."
    }));
    assert!(texts.contains(&"An interrupted install was undone."));
    assert!(texts.contains(&"A file stays locked."));
    assert!(
        has_label(&h, "A file stays locked."),
        "screen readers hear them too"
    );
    worker.send(Event::GameRunning(false));
    h.step();
}

#[test]
fn a_failed_launch_check_says_nothing_and_a_manual_one_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    let worker = app(&h).worker.clone();
    worker.send(Event::UpdateCheck {
        manual: false,
        result: Err("Couldn't verify this update's signature.".to_owned()),
    });
    h.step();
    assert!(app(&h).messages.texts().is_empty(), "silent at launch");
    assert!(app(&h).popup.is_none());

    worker.send(Event::UpdateCheck {
        manual: true,
        result: Err("Couldn't verify this update's signature.".to_owned()),
    });
    h.step();
    assert_eq!(
        app(&h).messages.texts(),
        vec!["Couldn't verify this update's signature."]
    );

    // A version the player skipped stays quiet at launch, but Check now still shows it.
    app_mut(&mut h).state.skip_app_version = Some("0.2.0".to_owned());
    let found = || Ok(Some(Box::new(demo::update_info())));
    worker.send(Event::UpdateCheck {
        manual: false,
        result: found(),
    });
    h.step();
    assert!(app(&h).popup.is_none());
    worker.send(Event::UpdateCheck {
        manual: true,
        result: found(),
    });
    h.step();
    assert!(matches!(app(&h).popup, Some(Popup::AppUpdate(_))));
}

#[test]
fn a_popup_waits_its_turn_behind_another() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    click(&mut h, "Remove");
    assert!(matches!(app(&h).popup, Some(Popup::Remove(_))));
    let worker = app(&h).worker.clone();
    worker.send(Event::UpdateCheck {
        manual: false,
        result: Ok(Some(Box::new(demo::update_info()))),
    });
    h.step();
    h.step();
    assert!(
        matches!(app(&h).popup, Some(Popup::Remove(_))),
        "one at a time"
    );
    press(&mut h, Key::Escape);
    h.step();
    assert!(
        matches!(app(&h).popup, Some(Popup::AppUpdate(_))),
        "then the next"
    );
}

#[test]
fn hovering_a_row_shows_an_item_tooltip() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    h.get_by_label("Questie, update").hover();
    // Let the tooltip's delay pass on the harness's clock.
    for _ in 0..30 {
        h.step();
    }
    assert!(has_label(&h, "Binds when installed"));
    assert!(has_label(&h, "Requires Interface 16001"));
    assert!(has_label(&h, "Size: 93 gold 60 silver"));
    assert!(has_label(
        &h,
        "Use: Marks available quests and objectives on your map, so you always know where to go next."
    ));
    let flavor = format!("\"{}\"", easter::flavor_line("gh:Questie/Questie"));
    assert!(has_label(&h, &flavor));
}

#[test]
fn nothing_asks_for_a_frame_while_idle() {
    let quiet = |h: &Harness<'static, Option<App>>| {
        let delay = h.output().viewport_output[&ViewportId::ROOT].repaint_delay;
        assert!(
            delay > Duration::from_secs(60 * 60),
            "the only thing left on a timer is mom, but the next frame is due in {delay:?}"
        );
    };

    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    h.run();
    quiet(&h);
    click(&mut h, "Browse");
    h.run();
    quiet(&h);

    let rig = Rig::new();
    rig.site.publish(&catalog_of(vec![publish_addon(
        &rig.site, FOO, "Foo", "1.0",
    )]));
    let mut h = rig.launch_settled();
    h.run();
    pump(&mut h, "the details pause", |a| {
        a.details.next_wake(Instant::now()).is_none()
    });
    h.run();
    quiet(&h);
}

#[test]
fn messages_fade_and_only_then_stop_asking_for_frames() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = demo_harness(Scene::Normal, dir.path());
    app_mut(&mut h).yellow("Hello");
    // A new message's first frames are busy, and then it should go quiet.
    h.run();
    let delay = h.output().viewport_output[&ViewportId::ROOT].repaint_delay;
    assert!(
        delay > Duration::from_secs(4) && delay <= Duration::from_secs(5),
        "during the hold it sleeps: {delay:?}"
    );
}

#[test]
fn only_one_copy_can_hold_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let state_file = dir.path().join("config").join("state.json");
    let first = crate::take_lock(&state_file).unwrap();
    assert!(first.is_some(), "it really is held");
    assert!(
        crate::take_lock(&state_file).is_err(),
        "the second copy is told"
    );
    drop(first);
    assert!(
        crate::take_lock(&state_file).is_ok_and(|lock| lock.is_some()),
        "and it's free once the first has gone"
    );
}

#[test]
fn a_lock_that_cannot_be_made_stops_nobody() {
    let dir = tempfile::tempdir().unwrap();
    // The config "folder" is a file, so the lock file can't be created in it.
    let blocked = dir.path().join("config");
    fs::write(&blocked, b"x").unwrap();
    let lock = crate::take_lock(&blocked.join("state.json"));
    assert!(lock.is_ok_and(|lock| lock.is_none()));
}

#[test]
fn a_second_copy_shows_only_the_already_running_popup() {
    let dir = tempfile::tempdir().unwrap();
    let launch = Launch::default();
    let dirs = Dirs {
        state_file: dir.path().join("state.json"),
        cache: dir.path().join("cache"),
        data: dir.path().join("data"),
    };
    let mut launch_slot = Some(launch);
    let mut startup = Some(Startup {
        dirs,
        state: State::default(),
        lock: None,
        already_running: true,
    });
    let mut h = Harness::builder()
        .with_size(egui::vec2(1040.0, 700.0))
        .build_ui_state(
            move |ui, slot: &mut Option<App>| {
                let app = slot.get_or_insert_with(|| {
                    App::new(
                        ui.ctx(),
                        launch_slot.take().unwrap(),
                        startup.take().unwrap(),
                    )
                });
                app.logic();
                app.draw(ui);
            },
            None,
        );
    h.step();
    h.step();
    assert!(has_label(&h, "WoW is a Feeling is already running."));
    assert!(has_label(&h, "Close"));
    press(&mut h, Key::Escape);
    assert!(
        matches!(app(&h).popup, Some(Popup::AlreadyRunning)),
        "nothing to go back to"
    );
    close(h);
    let wrote: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert!(
        wrote.is_empty(),
        "a second copy starts no jobs and writes nothing: {wrote:?}"
    );
}

#[test]
fn a_finished_session_saves_its_window_and_its_time() {
    let rig = Rig::new();
    rig.site.publish(&catalog_of(Vec::new()));
    let mut h = rig.launch_settled();
    app_mut(&mut h).shut_down();
    let state = rig.saved_state();
    assert!(state.current_install.is_some());
    assert!(
        state
            .installs
            .contains_key(state.current_install.as_ref().unwrap())
    );
}

#[test]
fn closing_mid_download_cancels_it_without_a_trace() {
    let rig = Rig::new();
    let addon = slow_addon(&rig);
    rig.site.publish(&catalog_of(vec![addon]));
    let mut h = rig.launch_settled();
    click(&mut h, "Install");
    pump(&mut h, "the download to start", |a| {
        a.casting().is_some_and(|c| c.fraction > 0.0)
    });
    app_mut(&mut h).shut_down();
    assert!(!rig.addons().join("Foo").exists());
    assert!(
        rig.saved_state()
            .installs
            .values()
            .all(|i| i.managed.is_empty())
    );
}

#[test]
fn a_catalog_full_of_hostile_text_cannot_hurt_anything() {
    let rig = Rig::new();
    let mut evil = publish_addon(&rig.site, FOO, "Foo", "1.0");
    evil.name = format!("Evil\n\u{202e}Name{}", "x".repeat(5000));
    evil.summary = "line one\nline two\u{0}".repeat(500);
    evil.author = "\u{200b}".repeat(100);
    evil.website = Some("javascript:alert(1)".to_owned());
    evil.category = "no-such-category".to_owned();
    evil.deps = vec!["../../../etc".to_owned()];
    let release = evil.stable.as_mut().unwrap();
    release.version = "1.0\n\n\n".to_owned();
    let mut catalog = catalog_of(vec![evil]);
    catalog.gaps = vec![Gap {
        name: "Gap\r\nName".to_owned(),
        reason: "\u{202e}reason".to_owned(),
        website: Some("file:///etc/passwd".to_owned()),
        alternatives: vec!["nope".to_owned()],
    }];
    rig.site.publish(&catalog);

    let mut h = rig.launch_settled();
    let bundle = app(&h).bundle.clone().unwrap();
    let addon = &bundle.catalog.addons[0];
    assert!(addon.name.len() <= 80 + 4 && !addon.name.contains('\n'));
    assert!(!addon.name.contains('\u{202e}'));
    assert!(addon.summary.chars().count() <= 600 && !addon.summary.contains('\n'));
    assert!(!addon.stable.as_ref().unwrap().version.contains('\n'));
    assert!(addon.author.is_empty());
    assert!(
        crate::model::addon_page_url(addon).is_some_and(|u| u.starts_with("http")),
        "the website is refused and only the checked source remains"
    );
    h.run();
}
