//! The hidden fun. All text here is original: no lyrics, no quotes from the
//! video, just a wink at it. Everything is inert until someone goes looking.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A link to the music video this app is named after.
pub const VIDEO_URL: &str = "https://www.youtube.com/watch?v=SpC3lZdk2HM";

/// How long the app stays open before someone calls.
pub const MOM_CALLS_AFTER: Duration = Duration::from_secs(3 * 60 * 60);

/// What a slash command in the search box asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    /// Show a yellow system message.
    Say(String),
    /// Really rescan the AddOns folder.
    Reload,
    /// Do a little dance with the portrait.
    Dance,
    /// Flash gold and celebrate.
    Ding,
    /// Show the PWNAGE banner.
    Pwnage,
    /// Open the About panel.
    OpenAbout,
}

/// Facts `/played` and friends can use. Plain numbers, so this stays testable.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    pub seconds_open: u64,
    pub launches: u64,
    pub addons_installed: u64,
    pub updates_applied: u64,
    pub managed: usize,
}

/// Parse a search box line. `None` means it's a normal search.
pub fn parse_slash(input: &str, facts: Facts) -> Option<Slash> {
    let line = input.trim();
    let rest = line.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    Some(match command.as_str() {
        "help" | "?" => Slash::Say(
            "Commands: /played, /reload, /who, /roll, /dance, /ding, /afk, /pwnage, /feeling"
                .to_owned(),
        ),
        "played" => Slash::Say(played_text(facts)),
        "reload" | "rl" => Slash::Reload,
        "who" => Slash::Say(who_text(facts)),
        "roll" | "random" => Slash::Say(roll_text(parts.next())),
        "dance" => Slash::Dance,
        "ding" => Slash::Ding,
        "afk" => Slash::Say("You are now AFK. The addons will keep your seat warm.".to_owned()),
        "pwnage" | "sax" => Slash::Pwnage,
        "feeling" | "about" => Slash::OpenAbout,
        "" => return None,
        _ => Slash::Say(format!("Unknown command: /{command}. Try /help.")),
    })
}

fn played_text(facts: Facts) -> String {
    format!(
        "Time spent in this addon manager: {}. Launches: {}. Addons accepted: {}. Updates: {}.",
        human_duration(facts.seconds_open),
        facts.launches,
        facts.addons_installed,
        facts.updates_applied,
    )
}

fn who_text(facts: Facts) -> String {
    match facts.managed {
        0 => "1 player online: you. Your quest log is empty.".to_owned(),
        1 => "1 addon in your party. Pulling anyway.".to_owned(),
        n => format!("{n} addons in your party. Nobody is tanking."),
    }
}

fn roll_text(max: Option<&str>) -> String {
    let max = max
        .and_then(|m| m.parse::<u64>().ok())
        .filter(|m| *m >= 1)
        .unwrap_or(100);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(4, |d| u64::from(d.subsec_nanos()));
    // Good enough for loot. A dependency for random numbers would not be.
    let roll = (nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) % max + 1;
    format!("You roll {roll} (1-{max}).")
}

/// Short, friendly durations for `/played`.
pub fn human_duration(seconds: u64) -> String {
    let (d, h, m, s) = (
        seconds / 86400,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
    );
    let plural = |n: u64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    if d > 0 {
        format!("{}, {}", plural(d, "day"), plural(h, "hour"))
    } else if h > 0 {
        format!("{}, {}", plural(h, "hour"), plural(m, "minute"))
    } else if m > 0 {
        format!("{}, {}", plural(m, "minute"), plural(s, "second"))
    } else {
        plural(s, "second")
    }
}

/// Addon sizes, priced the way a vendor would. One megabyte is one gold.
pub fn coins(bytes: u64) -> String {
    let silver_total = (bytes + 5_000) / 10_000;
    let (gold, silver) = (silver_total / 100, silver_total % 100);
    match (gold, silver) {
        (0, 0) => "1 silver".to_owned(),
        (0, s) => format!("{s} silver"),
        (g, 0) => format!("{g} gold"),
        (g, s) => format!("{g} gold {s} silver"),
    }
}

const FLAVOR: [&str; 16] = [
    "Now you'll never have to ask in General chat.",
    "Your guild will notice. They won't say anything.",
    "Soulbound to your hard drive.",
    "Smells faintly of warm CRT.",
    "Drop rate: 100%.",
    "Vendor price: free. Always.",
    "Keep out of reach of rogues.",
    "Do not pull before the tank is ready.",
    "One more quest, then bed.",
    "Your mom says hi.",
    "Stacks with itself, somehow.",
    "Some assembly required. Mostly by the author.",
    "Found behind a dusty monitor.",
    "Tastes like 2004.",
    "Grats in advance.",
    "It's not a phase.",
];

/// A flavor line that stays the same for a given addon.
pub fn flavor_line(addon_id: &str) -> &'static str {
    let hash = addon_id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    FLAVOR[(hash % FLAVOR.len() as u64) as usize]
}

/// Click the portrait five times and the monitor needs a quick clean.
#[derive(Debug, Default)]
pub struct MonitorGag {
    clicks: u32,
    last_click: Option<Instant>,
    off_until: Option<Instant>,
}

impl MonitorGag {
    const CLICK_WINDOW: Duration = Duration::from_millis(900);
    const OFF_FOR: Duration = Duration::from_secs(5);

    /// Register a click. Returns true when this click switched the monitor off.
    pub fn click(&mut self, now: Instant) -> bool {
        if self.off_until.is_some() {
            return false;
        }
        let in_time = self
            .last_click
            .is_some_and(|last| now.duration_since(last) <= Self::CLICK_WINDOW);
        self.clicks = if in_time { self.clicks + 1 } else { 1 };
        self.last_click = Some(now);
        if self.clicks >= 5 {
            self.clicks = 0;
            self.off_until = Some(now + Self::OFF_FOR);
            true
        } else {
            false
        }
    }

    /// Is the screen off right now? Clears itself once time is up.
    pub fn is_off(&mut self, now: Instant) -> bool {
        match self.off_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.off_until = None;
                false
            }
            None => false,
        }
    }

    /// When the UI next needs to wake up to turn the screen back on.
    pub fn wake_in(&self, now: Instant) -> Option<Duration> {
        self.off_until
            .map(|until| until.saturating_duration_since(now))
    }
}

/// The message for `MonitorGag` going off.
pub const CLEANING_MESSAGE: &str = "Nothing to see here. Just cleaning the monitor.";

/// The message for leaving the app open too long.
pub const MOM_MESSAGE: &str = "Your mom called. She wants you home.";

/// Occasionally a plush platypus sits on the monitor. About one launch in twenty.
pub fn platypus_visits(launches: u64) -> bool {
    launches % 20 == 13
}

/// The platypus, as a small pixel grid. `.` is transparent.
pub const PLATYPUS: [&str; 7] = [
    "....pppppppp....",
    "..ppppppppppp...",
    ".pppppkpppppppww",
    "ooooppppppppppww",
    "ooo.pppppppppp..",
    "....pp.pp.pp....",
    "....ww.ww.ww....",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn say(input: &str) -> String {
        match parse_slash(input, Facts::default()) {
            Some(Slash::Say(text)) => text,
            other => panic!("expected Say for {input}, got {other:?}"),
        }
    }

    #[test]
    fn normal_searches_are_not_commands() {
        assert_eq!(parse_slash("questie", Facts::default()), None);
        assert_eq!(parse_slash("", Facts::default()), None);
        assert_eq!(parse_slash("/", Facts::default()), None);
    }

    #[test]
    fn commands_are_case_insensitive_and_trimmed() {
        assert_eq!(
            parse_slash("  /RELOAD  ", Facts::default()),
            Some(Slash::Reload)
        );
        assert_eq!(parse_slash("/Dance", Facts::default()), Some(Slash::Dance));
        assert_eq!(
            parse_slash("/pwnage", Facts::default()),
            Some(Slash::Pwnage)
        );
        assert_eq!(
            parse_slash("/feeling", Facts::default()),
            Some(Slash::OpenAbout)
        );
    }

    #[test]
    fn unknown_commands_point_to_help() {
        assert!(say("/nope").contains("/help"));
    }

    #[test]
    fn played_reports_real_numbers() {
        let facts = Facts {
            seconds_open: 3700,
            launches: 7,
            addons_installed: 3,
            updates_applied: 2,
            managed: 3,
        };
        match parse_slash("/played", facts) {
            Some(Slash::Say(text)) => {
                assert!(text.contains("1 hour, 1 minute"), "{text}");
                assert!(text.contains("Launches: 7"), "{text}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rolls_stay_in_range() {
        for _ in 0..50 {
            let text = roll_text(Some("6"));
            let n: u64 = text
                .split_whitespace()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .expect("a number in the roll text");
            assert!((1..=6).contains(&n), "{text}");
        }
        assert!(roll_text(Some("banana")).contains("(1-100)"));
        assert!(roll_text(Some("0")).contains("(1-100)"));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(1), "1 second");
        assert_eq!(human_duration(59), "59 seconds");
        assert_eq!(human_duration(61), "1 minute, 1 second");
        assert_eq!(human_duration(7260), "2 hours, 1 minute");
        assert_eq!(human_duration(90000), "1 day, 1 hour");
    }

    #[test]
    fn coins_price_like_a_vendor() {
        assert_eq!(coins(93_600_000), "93 gold 60 silver");
        assert_eq!(coins(2_000_000), "2 gold");
        assert_eq!(coins(412_000), "41 silver");
        assert_eq!(coins(100), "1 silver");
    }

    #[test]
    fn flavor_is_stable_per_addon() {
        assert_eq!(
            flavor_line("gh:Questie/Questie"),
            flavor_line("gh:Questie/Questie")
        );
    }

    #[test]
    fn five_quick_clicks_clean_the_monitor() {
        let start = Instant::now();
        let mut gag = MonitorGag::default();
        for i in 0..4 {
            assert!(!gag.click(start + Duration::from_millis(100 * i)));
        }
        assert!(!gag.is_off(start));
        assert!(gag.click(start + Duration::from_millis(400)));
        assert!(gag.is_off(start + Duration::from_secs(1)));
        assert!(
            !gag.click(start + Duration::from_secs(2)),
            "ignored while off"
        );
        assert!(!gag.is_off(start + Duration::from_secs(10)));
    }

    #[test]
    fn slow_clicks_never_trigger_it() {
        let start = Instant::now();
        let mut gag = MonitorGag::default();
        for i in 0..10 {
            assert!(!gag.click(start + Duration::from_secs(2 * i)));
        }
    }

    #[test]
    fn platypus_grid_is_rectangular() {
        assert!(PLATYPUS.iter().all(|row| row.len() == PLATYPUS[0].len()));
    }
}
