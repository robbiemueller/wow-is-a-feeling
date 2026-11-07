//! Plain-text addon lists, for sharing a setup.
//!
//! A list is data from a stranger, so an entry can only ever be a well-formed addon
//! id. Paths, URLs and commands don't parse, and the app looks ids up in its catalog,
//! so a list has no way to run anything or point at a file.

use std::collections::HashSet;

use crate::catalog::Channel;
use crate::state::ManagedAddon;

const HEADER: &str = "# WoW is a Feeling addon list";
const BETA_SUFFIX: &str = " @beta";
const MAX_NAME_LEN: usize = 100;
const MAX_WOWI_ID_LEN: usize = 20;
const MAX_UNKNOWN_LINE_CHARS: usize = 200;

/// What a shared list said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedList {
    /// Addon ids with their channels, in file order, without repeats.
    pub entries: Vec<(String, Channel)>,
    /// Lines that weren't an addon id or a comment, to show the player.
    pub unknown_lines: Vec<String>,
}

/// Write a shareable list. Anything without a shareable id, such as an addon
/// installed from a local zip, is left out.
pub fn export<'a>(addons: impl IntoIterator<Item = &'a ManagedAddon>) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for addon in addons {
        if !is_valid_id(&addon.id) {
            continue;
        }
        out.push_str(&addon.id);
        if addon.channel == Channel::Beta {
            out.push_str(BETA_SUFFIX);
        }
        out.push('\n');
    }
    out
}

/// Read a list. Never fails: anything unusable lands in `unknown_lines`.
pub fn parse(text: &str) -> ParsedList {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut list = ParsedList::default();
    let mut seen = HashSet::new();
    for line in text.split(['\r', '\n']) {
        let content = line.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        match parse_entry(content) {
            Some((id, channel)) => {
                if seen.insert(id.to_ascii_lowercase()) {
                    list.entries.push((id.to_string(), channel));
                }
            }
            None => list
                .unknown_lines
                .push(line.trim().chars().take(MAX_UNKNOWN_LINE_CHARS).collect()),
        }
    }
    list
}

fn parse_entry(content: &str) -> Option<(&str, Channel)> {
    let (id, channel) = match content.split_once('@') {
        None => (content, Channel::Stable),
        Some((id, tag)) => {
            let tag = tag.trim();
            let channel = if tag.eq_ignore_ascii_case("beta") {
                Channel::Beta
            } else if tag.eq_ignore_ascii_case("stable") {
                Channel::Stable
            } else {
                return None;
            };
            (id.trim(), channel)
        }
    };
    is_valid_id(id).then_some((id, channel))
}

/// True for `gh:owner/repo`, `wowi:1234` and `direct:name`, in a strict character
/// set that can't spell a path, a URL or a command.
pub fn is_valid_id(id: &str) -> bool {
    let Some((kind, rest)) = id.split_once(':') else {
        return false;
    };
    match kind {
        "gh" => rest
            .split_once('/')
            .is_some_and(|(owner, repo)| is_safe_name(owner) && is_safe_name(repo)),
        "wowi" => {
            !rest.is_empty()
                && rest.len() <= MAX_WOWI_ID_LEN
                && rest.bytes().all(|b| b.is_ascii_digit())
        }
        "direct" => is_safe_name(rest),
        _ => false,
    }
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Origin;

    fn managed(id: &str, channel: Channel) -> ManagedAddon {
        ManagedAddon {
            id: id.to_string(),
            channel,
            ..Default::default()
        }
    }

    fn stable(id: &str) -> (String, Channel) {
        (id.to_string(), Channel::Stable)
    }

    fn beta(id: &str) -> (String, Channel) {
        (id.to_string(), Channel::Beta)
    }

    #[test]
    fn exports_the_header_ids_and_beta_marks() {
        let addons = [
            managed("gh:Questie/Questie", Channel::Stable),
            managed("wowi:1234", Channel::Beta),
            managed("direct:some-addon", Channel::Stable),
        ];
        assert_eq!(
            export(&addons),
            "# WoW is a Feeling addon list\n\
             gh:Questie/Questie\n\
             wowi:1234 @beta\n\
             direct:some-addon\n"
        );
    }

    #[test]
    fn export_with_nothing_is_just_the_header() {
        assert_eq!(export(&[]), "# WoW is a Feeling addon list\n");
    }

    #[test]
    fn export_leaves_out_what_cannot_be_shared() {
        let mut local = managed("local:My Zip", Channel::Stable);
        local.origin = Origin::LocalZip;
        let addons = [
            local,
            managed("C:\\Users\\Pat\\addon.zip", Channel::Stable),
            managed("", Channel::Stable),
            managed("wowi:1", Channel::Stable),
        ];
        assert_eq!(export(&addons), "# WoW is a Feeling addon list\nwowi:1\n");
    }

    #[test]
    fn what_is_exported_parses_back() {
        let addons = [
            managed("gh:funkydude/BigWigs", Channel::Beta),
            managed("wowi:5086", Channel::Stable),
            managed("direct:elvui.source_1", Channel::Beta),
        ];
        let parsed = parse(&export(&addons));
        assert_eq!(
            parsed.entries,
            [
                beta("gh:funkydude/BigWigs"),
                stable("wowi:5086"),
                beta("direct:elvui.source_1"),
            ]
        );
        assert!(parsed.unknown_lines.is_empty());
    }

    #[test]
    fn parsing_tolerates_comments_blanks_crlf_and_stray_spaces() {
        let text = "\u{feff}# my list\r\n\
                    \r\n\
                    \t  gh:a/b  \r\n\
                    wowi:1   @beta\r\n\
                    wowi:2@beta\r\n\
                    wowi:3 @ BETA # inline comment\r\n\
                    wowi:4 @stable\r\n\
                    wowi:5#comment\r\n\
                       # indented comment\r\n\
                    \n\n";
        let parsed = parse(text);
        assert_eq!(
            parsed.entries,
            [
                stable("gh:a/b"),
                beta("wowi:1"),
                beta("wowi:2"),
                beta("wowi:3"),
                stable("wowi:4"),
                stable("wowi:5"),
            ]
        );
        assert!(
            parsed.unknown_lines.is_empty(),
            "{:?}",
            parsed.unknown_lines
        );
    }

    #[test]
    fn old_mac_line_endings_work_too() {
        assert_eq!(
            parse("wowi:1\rwowi:2\r").entries,
            [stable("wowi:1"), stable("wowi:2")]
        );
    }

    #[test]
    fn repeats_keep_the_first_entry() {
        let parsed = parse("gh:A/B\ngh:a/b @beta\nwowi:1\nwowi:1\n");
        assert_eq!(parsed.entries, [stable("gh:A/B"), stable("wowi:1")]);
        assert!(parsed.unknown_lines.is_empty());
    }

    #[test]
    fn a_list_can_never_name_a_path_or_run_anything() {
        for line in [
            "C:\\Windows\\System32\\calc.exe",
            "/etc/passwd",
            "../../../secrets",
            "~/.ssh/id_rsa",
            "gh:../..",
            "gh:owner/..",
            "gh:../repo",
            "gh:owner/repo/../../other",
            "gh:owner/repo/extra",
            "gh:owner\\repo",
            "gh:owner/repo;rm -rf /",
            "gh:owner/$(whoami)",
            "gh:owner/`whoami`",
            "gh:/repo",
            "gh:owner/",
            "gh:owner",
            "gh:",
            "wowi:../1",
            "wowi:12a",
            "wowi:",
            "wowi:-1",
            "wowi:123456789012345678901234567890",
            "direct:../../etc/passwd",
            "direct:C:\\evil.exe",
            "direct:a/b",
            "direct:.hidden",
            "direct:",
            "direct:name with spaces",
            "https://evil.example/addon.zip",
            "file:///C:/evil.exe",
            "http:evil",
            "local:thing",
            "cmd /c calc",
            "powershell -enc AAAA",
            "just some words",
            "GH:owner/repo",
        ] {
            let parsed = parse(line);
            assert!(
                parsed.entries.is_empty(),
                "accepted {line:?}: {:?}",
                parsed.entries
            );
            assert_eq!(parsed.unknown_lines, [line], "for {line:?}");
        }
    }

    #[test]
    fn a_bad_channel_tag_does_not_count_as_stable() {
        let parsed = parse("wowi:1 @nightly\nwowi:2 @\nwowi:3 @beta @beta\n");
        assert!(parsed.entries.is_empty());
        assert_eq!(parsed.unknown_lines.len(), 3);
    }

    #[test]
    fn unknown_lines_are_trimmed_and_capped() {
        let long = format!("  {}  ", "é".repeat(5000));
        let parsed = parse(&long);
        assert_eq!(
            parsed.unknown_lines[0].chars().count(),
            MAX_UNKNOWN_LINE_CHARS
        );
    }

    #[test]
    fn overlong_ids_are_rejected() {
        assert!(is_valid_id(&format!("direct:{}", "a".repeat(MAX_NAME_LEN))));
        assert!(!is_valid_id(&format!(
            "direct:{}",
            "a".repeat(MAX_NAME_LEN + 1)
        )));
    }

    #[test]
    fn empty_input_gives_an_empty_list() {
        assert_eq!(parse(""), ParsedList::default());
        assert_eq!(parse("# only a comment\n\n"), ParsedList::default());
    }

    #[test]
    fn good_lines_survive_next_to_bad_ones() {
        let parsed = parse("wowi:1\nnonsense\ngh:a/b @beta\n");
        assert_eq!(parsed.entries, [stable("wowi:1"), beta("gh:a/b")]);
        assert_eq!(parsed.unknown_lines, ["nonsense"]);
    }
}
