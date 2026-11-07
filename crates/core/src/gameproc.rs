//! Is the game running? Asked only right before touching addon files, never polled.
//! The binary has one name for every flavor, so a running retail client counts too,
//! which only ever means asking the player to close something.

use std::fs;
use std::path::Path;

use crate::detect::GAME_EXES;

/// True for the name of a game process, in any case.
pub fn is_game_process_name(name: &str) -> bool {
    let name = name.trim();
    GAME_EXES.iter().any(|exe| name.eq_ignore_ascii_case(exe))
}

/// True when any argument of a NUL-separated `/proc/<pid>/cmdline` is the game. Under
/// Wine the game can be `argv[0]` or follow a loader, and its path can use either
/// kind of slash.
pub fn cmdline_runs_game(cmdline: &[u8]) -> bool {
    cmdline.split(|&b| b == 0).any(|arg| {
        let arg = String::from_utf8_lossy(arg);
        arg.rsplit(['/', '\\'])
            .next()
            .is_some_and(is_game_process_name)
    })
}

/// Scan a `/proc`-shaped folder for the game. Processes come and go during the walk,
/// so anything unreadable is skipped.
pub fn proc_has_game(proc_root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()));
        is_pid && process_is_game(&entry.path())
    })
}

fn process_is_game(process: &Path) -> bool {
    fs::read_to_string(process.join("comm")).is_ok_and(|comm| is_game_process_name(&comm))
        || fs::read(process.join("cmdline")).is_ok_and(|cmdline| cmdline_runs_game(&cmdline))
}

#[cfg(windows)]
pub fn is_game_running() -> bool {
    any_process_named(is_game_process_name)
}

#[cfg(windows)]
fn any_process_named(matches: impl Fn(&str) -> bool) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: plain flags, and the returned handle is checked below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return false;
    }
    // SAFETY: PROCESSENTRY32W is plain integers and an array, for which zero is valid.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    let mut found = false;
    // SAFETY: `snapshot` is a live snapshot handle, and `entry` has `dwSize` set.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more && !found {
        let exe = &entry.szExeFile;
        let len = exe.iter().position(|&c| c == 0).unwrap_or(exe.len());
        found = matches(&String::from_utf16_lossy(&exe[..len]));
        // SAFETY: as for Process32FirstW above.
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: `snapshot` is open and not used after this.
    unsafe { CloseHandle(snapshot) };
    found
}

/// The pattern ends in `t?` because macOS can cut process names to 16 characters, one
/// short of the game's, and `pgrep` patterns are regular expressions.
#[cfg(target_os = "macos")]
pub fn is_game_running() -> bool {
    use std::process::{Command, Stdio};

    Command::new("/usr/bin/pgrep")
        .args(["-x", "World of Warcraft?"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn is_game_running() -> bool {
    proc_has_game(Path::new("/proc"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_process(proc_root: &Path, pid: &str, comm: &str, cmdline: &[u8]) {
        let dir = proc_root.join(pid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), comm).unwrap();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn process_names_match_in_any_case_and_nothing_else() {
        for name in [
            "Wow.exe",
            "wow.exe",
            "WOW.EXE",
            "Wow-ARM64.exe",
            "wow-arm64.EXE",
            "Wow.exe\n",
        ] {
            assert!(is_game_process_name(name), "{name:?}");
        }
        for name in [
            "WowClassic.exe",
            "Wow",
            "Wow.exe.bak",
            "Battle.net.exe",
            "wow-is-a-feeling.exe",
            "",
        ] {
            assert!(!is_game_process_name(name), "{name:?}");
        }
    }

    #[test]
    fn command_lines_find_the_game_in_any_position_and_slash_style() {
        assert!(cmdline_runs_game(
            b"C:\\Program Files (x86)\\World of Warcraft\\_forever_\\Wow.exe\0-launcherlogin\0"
        ));
        assert!(cmdline_runs_game(
            b"/usr/bin/wine64-preloader\0/usr/bin/wine64\0Z:\\games\\wow\\_forever_\\wow.exe\0"
        ));
        assert!(cmdline_runs_game(
            b"/opt/wine/bin/wine\0/mnt/games/WoW/_forever_/Wow-ARM64.exe\0"
        ));
    }

    #[test]
    fn command_lines_without_the_game_do_not_match() {
        assert!(!cmdline_runs_game(b""));
        assert!(!cmdline_runs_game(b"\0\0"));
        assert!(!cmdline_runs_game(b"/usr/bin/bash\0-c\0ls\0"));
        assert!(!cmdline_runs_game(
            b"C:\\Battle.net\\Battle.net.exe\0--exec=launch wowf\0"
        ));
        // Only a whole path segment counts, not a mention inside a longer word.
        assert!(!cmdline_runs_game(b"/usr/bin/grep\0Wow.exe-notes\0"));
        assert!(!cmdline_runs_game(b"\xff\xfe\0\xc0"));
    }

    #[test]
    fn a_proc_scan_finds_wine_by_comm_or_cmdline() {
        let temp = tempfile::tempdir().unwrap();
        fake_process(temp.path(), "1", "systemd\n", b"/sbin/init\0");
        assert!(!proc_has_game(temp.path()));

        let by_comm = tempfile::tempdir().unwrap();
        fake_process(by_comm.path(), "1", "systemd\n", b"/sbin/init\0");
        fake_process(by_comm.path(), "4242", "Wow.exe\n", b"");
        assert!(proc_has_game(by_comm.path()));

        let by_cmdline = tempfile::tempdir().unwrap();
        fake_process(
            by_cmdline.path(),
            "99",
            "wine64-preloader\n",
            b"C:\\wow\\_forever_\\Wow.exe\0",
        );
        assert!(proc_has_game(by_cmdline.path()));
    }

    #[test]
    fn a_proc_scan_skips_what_it_cannot_read_or_that_is_not_a_process() {
        let temp = tempfile::tempdir().unwrap();
        // Not a pid, so it doesn't count even though it says Wow.exe.
        fake_process(temp.path(), "self-notes", "Wow.exe\n", b"Wow.exe\0");
        // A process that exited mid-scan.
        fs::create_dir_all(temp.path().join("77")).unwrap();
        fake_process(temp.path(), "78", "bash\n", b"bash\0");
        assert!(!proc_has_game(temp.path()));
        assert!(!proc_has_game(&temp.path().join("missing")));
    }

    #[test]
    fn the_real_check_runs_without_panicking() {
        // The answer depends on the machine, so this only exercises the OS calls.
        let _ = is_game_running();
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_process_walk_sees_real_processes() {
        let exe = std::env::current_exe().unwrap();
        let this_test = exe.file_name().unwrap().to_str().unwrap();
        assert!(any_process_named(
            |name| name.eq_ignore_ascii_case(this_test)
        ));
        assert!(!any_process_named(|name| name == "no-such-process.exe"));
    }
}
