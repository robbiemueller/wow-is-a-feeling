//! Parsing WoW `.toc` files and deciding whether an addon targets Forever.
//!
//! The client picks one TOC per addon folder, trying the game-specific suffix
//! first, then the family suffix, then the plain name. Forever's internal name
//! is "Camelot", and the wiki warns it may be renamed, so both spellings count.
//! Parsing is deliberately forgiving: real addons ship BOMs, CRLF line endings,
//! color codes in titles, and placeholder values like `00000`.

/// Lowest interface number that belongs to Forever.
pub const FOREVER_INTERFACE_MIN: u32 = 16000;
/// Highest interface number that belongs to Forever.
pub const FOREVER_INTERFACE_MAX: u32 = 19999;

/// The suffixes the Forever client looks for, in the order it tries them.
/// An empty string means the plain `Folder.toc`.
pub const TOC_SUFFIXES: [&str; 7] = [
    "_Forever", "-Forever", "_Camelot", "-Camelot", "_Mainline", "-Mainline", "",
];

/// Game types that may load on Forever when a TOC restricts itself.
const FOREVER_GAME_TYPES: [&str; 3] = ["camelot", "forever", "mainline"];

/// Everything we read from one `.toc` file's `##` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toc {
    pub title: Option<String>,
    pub notes: Option<String>,
    pub author: Option<String>,
    /// `None` when absent or still an unexpanded packager token such as `@project-version@`.
    pub version: Option<String>,
    /// Every number from `## Interface:` plus the Forever-specific overrides.
    pub interfaces: Vec<u32>,
    /// True when `## Interface-Forever:` or `## Interface-Camelot:` was present.
    pub has_forever_override: bool,
    /// Required addons (`Dependencies`, `RequiredDeps`, or any key starting with `Dep`).
    pub deps: Vec<String>,
    /// Load-order hints only. Never auto-installed.
    pub optional_deps: Vec<String>,
    pub category: Option<String>,
    pub load_on_demand: bool,
    pub website: Option<String>,
    pub curse_id: Option<String>,
    pub wowi_id: Option<String>,
    pub wago_id: Option<String>,
    /// Lowercased values of `## AllowLoadGameType:`. Empty when unrestricted.
    pub allow_game_types: Vec<String>,
}

impl Toc {
    /// Does this TOC declare that it runs on Forever?
    pub fn targets_forever(&self) -> bool {
        let interface_ok =
            self.has_forever_override || self.interfaces.iter().copied().any(is_forever_interface);
        let game_type_ok = self.allow_game_types.is_empty()
            || self
                .allow_game_types
                .iter()
                .any(|t| FOREVER_GAME_TYPES.contains(&t.as_str()));
        interface_ok && game_type_ok
    }

    /// The highest Forever interface number declared, if any.
    pub fn forever_interface(&self) -> Option<u32> {
        self.interfaces
            .iter()
            .copied()
            .filter(|n| is_forever_interface(*n))
            .max()
    }
}

/// True for interface numbers in Forever's range.
pub fn is_forever_interface(n: u32) -> bool {
    (FOREVER_INTERFACE_MIN..=FOREVER_INTERFACE_MAX).contains(&n)
}

/// Parse the text of a `.toc` file. Never fails: unknown lines are ignored.
pub fn parse(text: &str) -> Toc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut toc = Toc::default();
    let mut base_interfaces = Vec::new();
    let mut forever_interfaces = Vec::new();

    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("##") else {
            continue;
        };
        let Some((raw_key, raw_value)) = rest.split_once(':') else {
            continue;
        };
        let key = raw_key.trim().to_ascii_lowercase();
        let value = raw_value.trim();
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "title" => toc.title = non_empty(strip_ui_escapes(value)),
            "notes" => toc.notes = non_empty(strip_ui_escapes(value)),
            "author" => toc.author = non_empty(strip_ui_escapes(value)),
            "version" => toc.version = clean_version(value),
            "interface" => base_interfaces = parse_numbers(value),
            "interface-forever" | "interface-camelot" => {
                forever_interfaces.extend(parse_numbers(value));
                toc.has_forever_override = true;
            }
            "optionaldeps" | "optionaldependencies" => {
                toc.optional_deps.extend(parse_name_list(value));
            }
            "loadondemand" => toc.load_on_demand = value == "1",
            "category" | "x-category" => {
                if toc.category.is_none() {
                    toc.category = non_empty(strip_ui_escapes(value));
                }
            }
            "x-website" | "x-url" => {
                if toc.website.is_none() {
                    toc.website = non_empty(value.to_string());
                }
            }
            "x-curse-project-id" => toc.curse_id = non_empty(value.to_string()),
            "x-wowi-id" => toc.wowi_id = non_empty(value.to_string()),
            "x-wago-id" => toc.wago_id = non_empty(value.to_string()),
            "allowloadgametype" => {
                toc.allow_game_types = value
                    .split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            // The TOC format treats any key beginning with "Dep" as a required dependency.
            k if k.starts_with("dep") || k == "requireddeps" => {
                toc.deps.extend(parse_name_list(value));
            }
            _ => {}
        }
    }

    toc.interfaces = forever_interfaces;
    toc.interfaces.extend(base_interfaces);
    toc.deps.sort();
    toc.deps.dedup();
    toc.optional_deps.sort();
    toc.optional_deps.dedup();
    toc
}

/// Pick the TOC the Forever client would load for `folder`, given the file names inside it.
/// Matching ignores case, because Linux installs can't rely on the filesystem doing it.
pub fn pick_toc<'a>(folder: &str, file_names: &'a [String]) -> Option<&'a str> {
    TOC_SUFFIXES.iter().find_map(|suffix| {
        let wanted = format!("{folder}{suffix}.toc");
        file_names
            .iter()
            .find(|name| name.eq_ignore_ascii_case(&wanted))
            .map(String::as_str)
    })
}

/// Remove WoW UI escape sequences: `|cAARRGGBB`, `|r`, `|T...|t` textures, `|A...|a` atlases
/// and `|n` newlines. Addon authors love putting these in titles.
pub fn strip_ui_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '|' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('c') | Some('C') => {
                chars.next();
                for _ in 0..8 {
                    if chars.peek().is_some_and(char::is_ascii_hexdigit) {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            Some('r') | Some('R') => {
                chars.next();
            }
            Some('n') | Some('N') => {
                chars.next();
                out.push(' ');
            }
            Some(open @ ('T' | 't' | 'A' | 'a')) => {
                chars.next();
                let close = if open.eq_ignore_ascii_case(&'T') { 't' } else { 'a' };
                while let Some(inner) = chars.next() {
                    if inner == '|' && chars.peek().is_some_and(|n| n.eq_ignore_ascii_case(&close)) {
                        chars.next();
                        break;
                    }
                }
            }
            Some('|') => {
                chars.next();
                out.push('|');
            }
            _ => out.push('|'),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn non_empty(s: String) -> Option<String> {
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `## Version: @project-version@` means the file was never run through a packager.
fn clean_version(value: &str) -> Option<String> {
    let value = strip_ui_escapes(value);
    if value.is_empty() || value.contains('@') {
        None
    } else {
        Some(value)
    }
}

fn parse_numbers(value: &str) -> Vec<u32> {
    value
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Dependencies are comma-separated. Authors sometimes prefix conditions like
/// `[Family:Mainline] Foo`, so drop the bracketed part.
fn parse_name_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| {
            let item = item.trim();
            match item.rfind(']') {
                Some(i) => item[i + 1..].trim(),
                None => item,
            }
        })
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_toc() {
        let toc = parse(
            "## Interface: 120105, 16001\r\n\
             ## Title: |cff00ff00Big|r Wigs |TInterface\\Icon:16|t\r\n\
             ## Notes: Boss mods\r\n\
             ## Author: Funkeh\r\n\
             ## Version: v402\r\n\
             ## RequiredDeps: LibA, LibB\r\n\
             ## OptionalDeps: LibC\r\n\
             ## X-Curse-Project-ID: 2382\r\n\
             ## X-WoWI-ID: 5086\r\n\
             ## X-Wago-ID: e56n8DG8\r\n\
             core.lua\r\n",
        );
        assert_eq!(toc.title.as_deref(), Some("Big Wigs"));
        assert_eq!(toc.notes.as_deref(), Some("Boss mods"));
        assert_eq!(toc.version.as_deref(), Some("v402"));
        assert_eq!(toc.deps, ["LibA", "LibB"]);
        assert_eq!(toc.optional_deps, ["LibC"]);
        assert_eq!(toc.curse_id.as_deref(), Some("2382"));
        assert_eq!(toc.wowi_id.as_deref(), Some("5086"));
        assert_eq!(toc.wago_id.as_deref(), Some("e56n8DG8"));
        assert!(toc.targets_forever());
        assert_eq!(toc.forever_interface(), Some(16001));
    }

    #[test]
    fn handles_a_utf8_bom() {
        let toc = parse("\u{feff}## Interface: 16001\n## Title: Pawn\n");
        assert_eq!(toc.title.as_deref(), Some("Pawn"));
        assert!(toc.targets_forever());
    }

    #[test]
    fn placeholder_interface_is_not_forever() {
        let toc = parse("## Interface: 00000\n## Title: Questie\n");
        assert!(!toc.targets_forever());
    }

    #[test]
    fn retail_only_is_not_forever() {
        assert!(!parse("## Interface: 120105\n").targets_forever());
        assert!(!parse("## Interface: 11508, 30403\n").targets_forever());
    }

    #[test]
    fn forever_override_counts() {
        let toc = parse("## Interface: 120105\n## Interface-Camelot: 16001\n");
        assert!(toc.targets_forever());
        assert_eq!(toc.interfaces[0], 16001);
    }

    #[test]
    fn game_type_restriction_can_exclude_forever() {
        assert!(!parse("## Interface: 120105, 16001\n## AllowLoadGameType: standard\n").targets_forever());
        assert!(parse("## Interface: 16001\n## AllowLoadGameType: standard, camelot\n").targets_forever());
        assert!(parse("## Interface: 16001\n## AllowLoadGameType: mainline\n").targets_forever());
    }

    #[test]
    fn unexpanded_version_token_is_dropped() {
        assert_eq!(parse("## Version: @project-version@\n").version, None);
    }

    #[test]
    fn dep_prefix_keys_are_dependencies() {
        let toc = parse("## Dependencies: Ace3\n## Dep: LibStub\n## DepAnything: Foo\n");
        assert_eq!(toc.deps, ["Ace3", "Foo", "LibStub"]);
    }

    #[test]
    fn conditional_dependency_prefix_is_removed() {
        let toc = parse("## Dependencies: [Family:Mainline] Ace3, LibStub\n");
        assert_eq!(toc.deps, ["Ace3", "LibStub"]);
    }

    #[test]
    fn picks_toc_in_client_order() {
        let files: Vec<String> = ["Foo.toc", "Foo_Mainline.toc", "Foo_Camelot.toc", "Foo_Vanilla.toc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_toc("Foo", &files), Some("Foo_Camelot.toc"));

        let files: Vec<String> = ["Foo.toc", "Foo_Mainline.toc"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_toc("Foo", &files), Some("Foo_Mainline.toc"));

        let files: Vec<String> = ["Foo.toc", "Foo_Vanilla.toc"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_toc("Foo", &files), Some("Foo.toc"));

        let files: Vec<String> = ["Foo_Vanilla.toc"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_toc("Foo", &files), None);
    }

    #[test]
    fn toc_picking_ignores_case_and_keeps_leading_bang() {
        let files: Vec<String> = ["!bugGRABBER_forever.TOC".to_string()].to_vec();
        assert_eq!(pick_toc("!BugGrabber", &files), Some("!bugGRABBER_forever.TOC"));
    }

    #[test]
    fn strips_ui_escapes() {
        assert_eq!(strip_ui_escapes("|cFFFF0000Red|r Text"), "Red Text");
        assert_eq!(strip_ui_escapes("|TInterface\\Foo:16|t Icon"), "Icon");
        assert_eq!(strip_ui_escapes("|Aatlas:16:16|a Atlas"), "Atlas");
        assert_eq!(strip_ui_escapes("One|nTwo"), "One Two");
        assert_eq!(strip_ui_escapes("a || b"), "a | b");
        assert_eq!(strip_ui_escapes("  spaced   out "), "spaced out");
    }
}
