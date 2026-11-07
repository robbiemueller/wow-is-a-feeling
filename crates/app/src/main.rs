#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod args;
mod cursor;
mod demo;
mod details_cache;
mod dialogs;
mod easter;
mod events;
#[cfg(test)]
mod flows;
mod fun;
mod messages;
mod model;
mod ops;
mod script;
mod skin;
mod startup;
mod theme;
mod views;
mod widgets;
mod worker;

use args::Launch;
use eframe::egui;
use feeling_core::{logging, paths, state::State};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Only this exact value turns on the test rig, and only at startup.
const TEST_RIG_ENV: &str = "WIAF_TEST_LOOPBACK";
const LOCK_FILE: &str = "lock";

fn window_icon() -> Arc<egui::IconData> {
    let img = image::load_from_memory_with_format(
        include_bytes!("../../../assets/icon/png/icon-64.png"),
        image::ImageFormat::Png,
    )
    .expect("the embedded icon is a valid PNG")
    .into_rgba8();
    Arc::new(egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    })
}

/// Save what's on screen as a PNG. `then_close` is for `--screenshot`, which exits after.
pub fn save_screenshot(ctx: &egui::Context, path: PathBuf, then_close: bool) {
    let closer = ctx.clone();
    ctx.request_screenshot(move |image| {
        let bytes: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|c| c.to_srgba_unmultiplied())
            .collect();
        let saved = image::save_buffer(
            &path,
            &bytes,
            image.size[0] as u32,
            image.size[1] as u32,
            image::ColorType::Rgba8,
        );
        if let Err(e) = saved {
            eprintln!("could not save screenshot: {e}");
        }
        if then_close {
            closer.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

/// Another copy of the app holds the lock.
#[derive(Debug)]
pub struct AlreadyRunning;

/// Become the one running copy, by holding this file locked for as long as the app runs.
/// A lock that can't even be attempted, such as on a read-only disk or a filesystem without
/// locks, is no reason to stop anyone, so that's `Ok(None)` and the app runs without one.
pub fn take_lock(state_file: &Path) -> Result<Option<File>, AlreadyRunning> {
    let path = state_file.with_file_name(LOCK_FILE);
    if let Some(dir) = path.parent() {
        paths::ensure_dir(dir);
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path);
    let file = match file {
        Ok(file) => file,
        Err(e) => {
            log::warn!("Couldn't open {}: {e}", path.display());
            return Ok(None);
        }
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Err(AlreadyRunning),
        Err(fs::TryLockError::Error(e)) => {
            log::warn!("Couldn't lock {}: {e}", path.display());
            Ok(None)
        }
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("The app crashed: {info}");
        default_hook(info);
    }));
}

/// How the window starts, apart from its icon.
fn viewport(launch: &Launch, state: &State, already_running: bool) -> egui::ViewportBuilder {
    let settings = &state.settings;
    let mut size = settings.window_size.unwrap_or([1040.0, 700.0]);
    if let Some([w, h]) = launch.size {
        let scale = launch.scale.unwrap_or(settings.ui_scale);
        size = [w * scale, h * scale];
    }
    let mut min = if launch.size.is_some() {
        [320.0, 240.0]
    } else {
        [820.0, 560.0]
    };
    // A second copy only has one thing to say, so it's a small window that says it.
    if already_running {
        size = [520.0, 300.0];
        min = size;
    }
    let mut builder = egui::ViewportBuilder::default()
        .with_title("WoW is a Feeling")
        .with_app_id("wow-is-a-feeling")
        .with_inner_size(size)
        .with_min_inner_size(min)
        .with_decorations(false);
    if let Some(pos) = settings.window_pos
        && !launch.demo
        && !already_running
        && still_on_screen(pos)
    {
        builder = builder.with_position(pos);
    }
    // Test runs must never pull focus away from whatever the player is doing, such as
    // playing the game fullscreen.
    if launch.unattended() {
        builder = builder.with_active(false);
    }
    builder
}

/// Would a window at this position be somewhere a player can reach it? Windows can say.
/// Elsewhere the window manager keeps windows on screen itself.
#[cfg(windows)]
fn still_on_screen(pos: [f32; 2]) -> bool {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    const MONITOR_DEFAULTTONULL: u32 = 0;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MonitorFromPoint(point: Point, flags: u32) -> *mut std::ffi::c_void;
    }
    // The title bar, a little in from the corner, is what has to be reachable.
    let point = Point {
        x: pos[0] as i32 + 120,
        y: pos[1] as i32 + 20,
    };
    // SAFETY: plain values in, and the handle that comes back is only compared with null.
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) };
    !monitor.is_null()
}

#[cfg(not(windows))]
fn still_on_screen(_pos: [f32; 2]) -> bool {
    true
}

#[cfg(windows)]
fn tell_player(text: &str) {
    const MB_ICONERROR: u32 = 0x10;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            window: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (text, caption) = (wide(text), wide("WoW is a Feeling"));
    // SAFETY: both strings are NUL-terminated and outlive the call, and a null window is
    // documented to mean no owner.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn tell_player(text: &str) {
    eprintln!("{text}");
}

fn main() {
    let started = std::time::Instant::now();
    let mut launch = Launch::parse(std::env::args().skip(1));
    launch.started = Some(started);
    launch.test_rig = std::env::var_os(TEST_RIG_ENV).is_some_and(|value| value == "1");

    let dirs = worker::Dirs::real();
    let (state, lock, already_running) = if launch.demo {
        (demo::state(launch.scene), None, false)
    } else {
        logging::init(log::LevelFilter::Info);
        install_panic_hook();
        let (lock, already_running) = match take_lock(&dirs.state_file) {
            Ok(lock) => (lock, false),
            Err(AlreadyRunning) => (None, true),
        };
        let mut state = State::load_from(&dirs.state_file);
        if !already_running {
            state.stats.launches += 1;
            if state.stats.first_launch == 0 {
                state.stats.first_launch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
            }
        }
        (state, lock, already_running)
    };

    let options = eframe::NativeOptions {
        viewport: viewport(&launch, &state, already_running).with_icon(window_icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let startup = app::Startup {
        dirs,
        state,
        lock,
        already_running,
    };
    let started = eframe::run_native(
        "wow-is-a-feeling",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx, launch, startup)))),
    );
    if let Err(e) = started {
        log::error!("Couldn't open the window: {e}");
        tell_player(&format!(
            "WoW is a Feeling couldn't open its window, because it needs OpenGL 2.0 or newer \
             and your graphics driver didn't provide it. Updating your graphics driver usually \
             fixes this.\n\nDetails: {e}"
        ));
    }
}
