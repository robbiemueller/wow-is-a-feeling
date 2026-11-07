//! Small text helpers: cleaning names and summaries, comparing folder names,
//! and encoding URL parts. Addon metadata is untrusted free text, so everything
//! here is about making it readable, never about interpreting it.

/// Lowercase letters and digits only, for comparing names that differ in punctuation.
pub fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Splits on punctuation and on camelCase, keeping the whole word as well:
/// `GatherMate2` gives `gathermate2`, `gather`, `mate`.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        out.push(word.to_lowercase());
        let chars: Vec<char> = word.chars().collect();
        let mut part = String::new();
        let mut parts = 0;
        for (i, &c) in chars.iter().enumerate() {
            let starts_word = i > 0
                && c.is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || chars[i - 1].is_ascii_digit()
                    || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
            if starts_word && !part.is_empty() {
                push_part(&mut out, &part);
                parts += 1;
                part.clear();
            }
            part.push(c);
        }
        if parts > 0 {
            push_part(&mut out, &part);
        } else if word.ends_with(|c: char| c.is_ascii_digit()) {
            push_part(&mut out, word);
        }
    }
    out
}

fn push_part(out: &mut Vec<String>, part: &str) {
    let trimmed = part
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .to_lowercase();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
}

/// Addon name out of a zip file name: `Questie-v12.6.2+v1.0.11.zip` gives `Questie`.
pub fn zip_stem(file_name: &str) -> String {
    let stem = file_name
        .get(..file_name.len().saturating_sub(4))
        .filter(|_| file_name.to_ascii_lowercase().ends_with(".zip"))
        .unwrap_or(file_name);
    let chars: Vec<(usize, char)> = stem.char_indices().collect();
    for (i, &(at, c)) in chars.iter().enumerate() {
        if !matches!(c, '-' | '_' | ' ' | '.') || at == 0 {
            continue;
        }
        let next = chars.get(i + 1).map(|p| p.1);
        let after = chars.get(i + 2).map(|p| p.1);
        let version_follows = next.is_some_and(|n| n.is_ascii_digit())
            || (matches!(next, Some('v' | 'V')) && after.is_some_and(|n| n.is_ascii_digit()));
        if version_follows {
            return stem[..at].to_string();
        }
    }
    stem.to_string()
}

/// Display name from a TOC title or a listing name. Returns empty when nothing
/// readable is left, and the caller falls back to something else.
pub fn clean_name(raw: &str) -> String {
    let text = drop_packager_tokens(raw).replace(['<', '>', '_'], " ");
    let text = drop_flavor_groups(&text);
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|word| !is_version_word(word) && !is_ace_tag(word))
        .collect();
    let joined = if words.is_empty() {
        text.trim().to_string()
    } else {
        words.join(" ")
    };
    let decoration = |c: char| {
        matches!(
            c,
            '-' | '–' | '—' | ':' | '|' | ',' | '~' | '*' | '=' | '"' | '\'' | ' '
        )
    };
    // A trailing `!` is part of a name like `Details!`, a leading one is a load order trick.
    let trimmed = joined
        .trim_end_matches(decoration)
        .trim_start_matches(|c| decoration(c) || c == '!');
    truncate_at_word(trimmed, 80)
}

/// Addons built on Ace3 often end their title with `-Ace3-`, which means nothing to a player.
fn is_ace_tag(word: &str) -> bool {
    word.trim_matches('-').eq_ignore_ascii_case("ace3") && word.starts_with('-')
}

/// One readable line from a TOC note or a repository description.
pub fn clean_summary(raw: &str) -> String {
    let text = drop_packager_tokens(raw)
        .replace("\\n", " ")
        .replace("\\r", " ");
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut words: Vec<&str> = text
        .split_whitespace()
        .filter(|word| !looks_like_link(word))
        .collect();
    // What's left after dropping links: "... PayPal:" and "... Version: 3.5.4".
    while let Some(last) = words.last().copied() {
        let label = last.trim_end_matches(':').to_ascii_lowercase();
        let is_label = last.ends_with(':')
            && matches!(
                label.as_str(),
                "paypal"
                    | "patreon"
                    | "discord"
                    | "donate"
                    | "kofi"
                    | "ko-fi"
                    | "website"
                    | "web"
                    | "version"
            );
        let is_version_tail = words.len() >= 2
            && words[words.len() - 2].eq_ignore_ascii_case("version:")
            && last.starts_with(|c: char| c.is_ascii_digit());
        if is_version_tail {
            words.truncate(words.len() - 2);
        } else if is_label {
            words.pop();
        } else {
            break;
        }
    }
    let joined = words.join(" ");
    truncate_at_word(joined.trim(), 240)
}

/// Links are for the addon's page, and summaries read better without them.
fn looks_like_link(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_alphanumeric());
    let lower = word.to_ascii_lowercase();
    lower.contains("://")
        || lower.starts_with("www.")
        || lower.split('/').next().is_some_and(|host| {
            host.rsplit_once('.').is_some_and(|(name, tld)| {
                !name.is_empty() && matches!(tld, "com" | "net" | "org" | "gg" | "me" | "io" | "tv")
            })
        })
}

/// An author's display name. Addresses and links are dropped: a TOC is public, but
/// the catalog has no reason to repeat anyone's email.
pub fn clean_author(raw: &str) -> String {
    let words: Vec<&str> = raw
        .split_whitespace()
        .filter(|word| !word.contains('@') && !looks_like_link(word))
        .collect();
    clean_name(&words.join(" "))
}

/// `@project-version@` and `@localization(...)@` are packager tokens that were never expanded.
fn drop_packager_tokens(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('@') {
        let after = &rest[start + 1..];
        match after.find('@') {
            Some(end) if !after[..end].contains(' ') || after[..end].contains('(') => {
                out.push_str(&rest[..start]);
                rest = &after[end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Removes `[Forever]`, `(Camelot)` or `[v1.2.3]` style groups, keeping any other brackets.
fn drop_flavor_groups(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open_at) = rest.find(['[', '(']) {
        let close = if rest[open_at..].starts_with('[') {
            ']'
        } else {
            ')'
        };
        let Some(len) = rest[open_at..].find(close) else {
            break;
        };
        let inner = &rest[open_at + 1..open_at + len];
        out.push_str(&rest[..open_at]);
        if !is_flavor_label(inner) {
            out.push_str(&rest[open_at..=open_at + len]);
        }
        rest = &rest[open_at + len + 1..];
    }
    out.push_str(rest);
    out
}

fn is_flavor_label(inner: &str) -> bool {
    let lower = inner.trim().to_ascii_lowercase();
    lower.is_empty()
        || is_version_word(&lower)
        || matches!(
            lower.as_str(),
            "forever"
                | "camelot"
                | "wow forever"
                | "for forever"
                | "retail"
                | "mainline"
                | "classic"
                | "midnight"
        )
}

/// `v2`, `1.2.3` and `12.0.5-beta` count. `3.0` alone doesn't, because of names like `Prat 3.0`.
fn is_version_word(word: &str) -> bool {
    let body = word.strip_prefix(['v', 'V']).unwrap_or(word);
    let has_v = body.len() != word.len();
    if !body.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let numeric = body.split(['-', '+']).next().unwrap_or(body);
    if !numeric.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    has_v || numeric.matches('.').count() >= 2
}

fn truncate_at_word(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    let at_word = match cut.rfind(' ') {
        Some(space) if space > max_chars / 2 => &cut[..space],
        _ => cut.as_str(),
    };
    format!(
        "{}...",
        at_word.trim_end_matches([' ', ',', '.', ';', ':', '-'])
    )
}

/// A version string a player can read: `bundle/v12.6.2+v1.0.11` gives `v12.6.2+v1.0.11`,
/// and `Plater-v663` gives `v663` when the addon's name is the prefix.
pub fn clean_version(tag: &str, names: &[&str]) -> String {
    let tail = tag.rsplit('/').next().unwrap_or(tag).trim();
    for name in names {
        let Some(rest) = tail.get(name.len()..) else {
            continue;
        };
        if name.is_empty() || !tail[..name.len()].eq_ignore_ascii_case(name) {
            continue;
        }
        let stripped = rest.trim_start_matches(['-', '_', '.', ' ']);
        let version_like = stripped.len() != rest.len()
            && (stripped.starts_with(|c: char| c.is_ascii_digit())
                || (stripped.starts_with(['v', 'V'])
                    && stripped[1..].starts_with(|c: char| c.is_ascii_digit())));
        if version_like {
            return stripped.to_string();
        }
    }
    tail.to_string()
}

/// Percent-encodes every byte that isn't unreserved, and leaves `/` alone so a
/// tag with slashes stays a path. `bundle/v1+v2` gives `bundle/v1%2Bv2`.
pub fn encode_path(path: &str) -> String {
    encode(path, |b| b == b'/')
}

/// Percent-encodes a query value.
pub fn encode_query(value: &str) -> String {
    encode(value, |_| false)
}

fn encode(text: &str, keep: impl Fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || keep(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Keeps `digits` significant digits, rounding down. Counts tick up all day, and
/// exact numbers would change the catalog's bytes on every build for no benefit.
pub fn round_sig(n: u64, digits: u32) -> u64 {
    let mut scale = 1u64;
    let mut rest = n;
    while rest >= 10u64.pow(digits) {
        rest /= 10;
        scale *= 10;
    }
    rest * scale
}

/// `owner/repo` with only the characters GitHub allows. These names come from
/// third-party lists and end up inside a query, so nothing else gets through.
pub fn valid_repo_name(name: &str) -> bool {
    let Some((owner, repo)) = name.split_once('/') else {
        return false;
    };
    let allowed = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.len() <= 100
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    allowed(owner) && allowed(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_split_camel_case_and_digits() {
        let t = tokens("GatherMate2");
        assert!(t.contains(&"gathermate2".to_string()));
        assert!(t.contains(&"gather".to_string()));
        assert!(t.contains(&"mate".to_string()));
        let t = tokens("OpenMap Tools");
        for want in ["openmap", "open", "map", "tools"] {
            assert!(t.contains(&want.to_string()), "{want} missing from {t:?}");
        }
        assert!(tokens("Bartender4").contains(&"bartender".to_string()));
        assert!(tokens("BUGS").contains(&"bugs".to_string()));
    }

    #[test]
    fn zip_stems_drop_versions_and_flavors() {
        assert_eq!(zip_stem("Questie-v12.6.2+v1.0.11.zip"), "Questie");
        assert_eq!(zip_stem("Bartender4-4.18.4.zip"), "Bartender4");
        assert_eq!(zip_stem("DBM-Core-12.0.20.zip"), "DBM-Core");
        assert_eq!(zip_stem("Pawn-2.14.1-Forever.zip"), "Pawn");
        assert_eq!(zip_stem("Decursive-2.9.0-RC4-forever.ZIP"), "Decursive");
        assert_eq!(zip_stem("Plain.zip"), "Plain");
        assert_eq!(zip_stem("noextension"), "noextension");
    }

    #[test]
    fn names_lose_flavor_tags_and_versions_but_not_their_identity() {
        assert_eq!(clean_name("Questie"), "Questie");
        assert_eq!(clean_name("Foo [Forever]"), "Foo");
        assert_eq!(clean_name("Foo (Camelot) v1.2.3"), "Foo");
        assert_eq!(clean_name("Titan Panel [Core] 9.3.5"), "Titan Panel [Core]");
        assert_eq!(clean_name("Prat 3.0"), "Prat 3.0");
        assert_eq!(clean_name("<DBM Core>"), "DBM Core");
        assert_eq!(
            clean_name("<DBM> World Bosses (Vanilla)"),
            "DBM World Bosses (Vanilla)"
        );
        assert_eq!(clean_name("Decursive -Ace3-"), "Decursive");
        assert_eq!(clean_name("IceHUD -Ace3"), "IceHUD");
        assert_eq!(clean_name("MaxDps_Warlock"), "MaxDps Warlock");
        assert_eq!(
            clean_name("Titan Panel [Currencies] Multi! v12.0.0 Forever"),
            "Titan Panel [Currencies] Multi! Forever"
        );
        assert_eq!(
            clean_name("v2"),
            "v2",
            "don't erase a name that is only a version"
        );
        assert_eq!(clean_name("!BugGrabber"), "BugGrabber");
        assert_eq!(clean_name("Details!"), "Details!");
        assert_eq!(clean_name("@project-name@"), "");
        assert_eq!(clean_name("Shortest Path Forever"), "Shortest Path Forever");
        assert_eq!(clean_name("  -- Fancy --  "), "Fancy");
    }

    #[test]
    fn long_names_and_summaries_are_cut_on_words() {
        let long = "word ".repeat(100);
        let summary = clean_summary(&long);
        assert!(summary.chars().count() <= 243, "{summary}");
        assert!(summary.ends_with("word..."), "{summary}");
        assert!(clean_name(&long).chars().count() <= 83);
        let unbroken = "x".repeat(500);
        assert!(clean_summary(&unbroken).chars().count() <= 243);
    }

    #[test]
    fn summaries_drop_tokens_and_control_characters() {
        assert_eq!(
            clean_summary("Boss mods\\nfor raids @project-version@"),
            "Boss mods for raids"
        );
        assert_eq!(clean_summary("mail me @ home"), "mail me @ home");
        assert_eq!(clean_summary("a\tb\u{7}c"), "a b c");
        assert_eq!(
            clean_summary("@localization(locale=\"enUS\", key=\"Notes\")@"),
            ""
        );
    }

    #[test]
    fn summaries_lose_links_and_donation_labels() {
        assert_eq!(
            clean_summary("A simple Bagnon plugin. PayPal: www.paypal.me/GoldpawsStuff"),
            "A simple Bagnon plugin."
        );
        assert_eq!(
            clean_summary(
                "All-in-one loot addon. By Zhorax patreon.com/gargul ko-fi.com/gargul discord.gg/x"
            ),
            "All-in-one loot addon. By Zhorax"
        );
        assert_eq!(
            clean_summary("Leveling guides https://tinyurl.com/guidelime-paypal"),
            "Leveling guides"
        );
        assert_eq!(
            clean_summary("The best roleplaying addon. Version: 3.5.4"),
            "The best roleplaying addon."
        );
        assert_eq!(
            clean_summary("Works with node.js and Foo.bar"),
            "Works with node.js and Foo.bar"
        );
    }

    #[test]
    fn authors_lose_addresses_and_links() {
        assert_eq!(clean_author("MooreaTV moorea@ymail.com"), "MooreaTV");
        assert_eq!(clean_author("https://github.com/moody"), "");
        assert_eq!(clean_author("Funkeh, Nebula"), "Funkeh, Nebula");
    }

    #[test]
    fn versions_are_readable_and_tags_with_slashes_survive() {
        assert_eq!(
            clean_version("bundle/v12.6.2+v1.0.11", &["Questie"]),
            "v12.6.2+v1.0.11"
        );
        assert_eq!(clean_version("Plater-v663", &["Plater"]), "v663");
        assert_eq!(
            clean_version("Details.20261104.15400.172", &["Details"]),
            "20261104.15400.172"
        );
        assert_eq!(clean_version("Pawn-2.14.1", &["Pawn"]), "2.14.1");
        assert_eq!(clean_version("v427.8", &["BigWigs"]), "v427.8");
        assert_eq!(clean_version("Classic-0.0.8", &["NDui"]), "Classic-0.0.8");
        assert_eq!(clean_version("Pawnee-1", &["Pawn"]), "Pawnee-1");
    }

    #[test]
    fn encoding_keeps_paths_usable() {
        assert_eq!(
            encode_path("bundle/v12.6.2+v1.0.11"),
            "bundle/v12.6.2%2Bv1.0.11"
        );
        assert_eq!(
            encode_path("Details.20261104.15400.172"),
            "Details.20261104.15400.172"
        );
        assert_eq!(encode_path("a b"), "a%20b");
        assert_eq!(
            encode_query("topic:wow-addon pushed:>=2026-07-01"),
            "topic%3Awow-addon%20pushed%3A%3E%3D2026-07-01"
        );
    }

    #[test]
    fn rounding_keeps_leading_digits() {
        assert_eq!(round_sig(0, 3), 0);
        assert_eq!(round_sig(999, 3), 999);
        assert_eq!(round_sig(1234, 3), 1230);
        assert_eq!(round_sig(4_727_895, 3), 4_720_000);
    }

    #[test]
    fn repo_names_are_strict() {
        assert!(valid_repo_name("Questie/Questie"));
        assert!(valid_repo_name("Legacy-of-Sylvanaar/prat-3-0"));
        assert!(valid_repo_name("user.name/repo_1"));
        assert!(!valid_repo_name("no-slash"));
        assert!(!valid_repo_name("a/b/c"));
        assert!(!valid_repo_name("a/\"){ evil }"));
        assert!(!valid_repo_name("../x"));
        assert!(!valid_repo_name("/x"));
    }
}
