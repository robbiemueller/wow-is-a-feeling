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

/// A name as a reader sees it: case, punctuation and look-alike letters from other
/// alphabets don't count, so `Quеstie` with a Cyrillic `е` folds to the same text as
/// `Questie`. For telling whether two addons would pass for each other, never for display.
pub fn fold_name(text: &str) -> String {
    text.chars()
        .map(plain_form)
        .filter(|c| c.is_alphanumeric())
        .flat_map(fold_letter)
        .collect()
}

/// A folder name with the same look-alikes folded, keeping its punctuation: `Foo_Bar`
/// and `FooBar` are different folders on disk, however alike they read.
pub fn fold_folder(text: &str) -> String {
    text.chars().map(plain_form).flat_map(fold_letter).collect()
}

/// Whether two folder names pass for the same folder: they fold alike, or they're the same
/// ignoring case, which is all the catalog check and the app compare. Folding reads a few
/// capitals from other alphabets apart from their small letters, such as a Greek capital Eta as
/// H and its small letter as n, so neither one covers the other.
pub fn same_folder(a: &str, b: &str) -> bool {
    fold_folder(a) == fold_folder(b) || a.to_lowercase() == b.to_lowercase()
}

/// One of the two ways a folder name is filed in [`FolderMap`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FolderKey {
    Folded(String),
    Lowered(String),
}

fn folder_keys(folder: &str) -> [FolderKey; 2] {
    [
        FolderKey::Folded(fold_folder(folder)),
        FolderKey::Lowered(folder.to_lowercase()),
    ]
}

/// Folder names filed so that names that pass for the same folder, as [`same_folder`] has it,
/// find each other, each with what it belongs to.
#[derive(Debug)]
pub struct FolderMap<T> {
    filed: std::collections::HashMap<FolderKey, Vec<T>>,
}

impl<T> Default for FolderMap<T> {
    fn default() -> Self {
        Self {
            filed: std::collections::HashMap::new(),
        }
    }
}

impl<T: Clone> FolderMap<T> {
    pub fn add(&mut self, folder: &str, value: T) {
        for key in folder_keys(folder) {
            self.filed.entry(key).or_default().push(value.clone());
        }
    }

    /// Everything filed under a folder name that passes for this one. Something filed under one
    /// that matches both ways comes twice.
    pub fn alike(&self, folder: &str) -> impl Iterator<Item = &T> {
        folder_keys(folder)
            .into_iter()
            .filter_map(|key| self.filed.get(&key))
            .flatten()
    }
}

/// A letter or digit drawn in a style, as the plain one: the mathematical bold, italic,
/// script, fraktur, double-struck, sans-serif and monospace alphabets, the letterlike
/// symbols that stand in for a few of their letters, circled, parenthesized and squared
/// letters, and small capitals. A name written in any of them reads as the plain name.
fn plain_form(c: char) -> char {
    let code = u32::from(c);
    let nth = |first: char, at: u32| char::from_u32(u32::from(first) + at).unwrap_or(c);
    match code {
        // Thirteen styles in a row, each A to Z and then a to z.
        0x1D400..=0x1D6A3 => match (code - 0x1D400) % 52 {
            at @ 0..=25 => nth('A', at),
            at => nth('a', at - 26),
        },
        0x1D6A4 => 'i',
        0x1D6A5 => 'j',
        // Five styles of Greek, each 25 capitals, a nabla, 25 small letters and 7 symbols.
        0x1D6A8..=0x1D7C9 => match (code - 0x1D6A8) % 58 {
            17 => 'Θ',
            at @ 0..=24 => nth('Α', at),
            at @ 26..=50 => nth('α', at - 26),
            _ => c,
        },
        // Five styles of 0 to 9.
        0x1D7CE..=0x1D7FF => nth('0', (code - 0x1D7CE) % 10),
        0x2460..=0x2468 => nth('1', code - 0x2460),
        0x24EA => '0',
        0x249C..=0x24B5 => nth('a', code - 0x249C),
        0x24B6..=0x24CF => nth('A', code - 0x24B6),
        0x24D0..=0x24E9 => nth('a', code - 0x24D0),
        // Parenthesized, squared, negative circled and negative squared capitals.
        0x1F110..=0x1F189 if (code - 0x1F110) % 32 < 26 => nth('A', (code - 0x1F110) % 32),
        _ => match c {
            'ℬ' => 'B',
            'ℂ' | 'ℭ' => 'C',
            'ⅅ' => 'D',
            'ℰ' => 'E',
            'ℱ' => 'F',
            'ℋ' | 'ℌ' | 'ℍ' => 'H',
            'ℐ' | 'ℑ' => 'I',
            'ℒ' => 'L',
            'ℳ' => 'M',
            'ℕ' => 'N',
            'ℙ' => 'P',
            'ℚ' => 'Q',
            'ℛ' | 'ℜ' | 'ℝ' => 'R',
            'ℤ' | 'ℨ' => 'Z',
            'ᴀ' => 'a',
            'ʙ' => 'b',
            'ᴄ' => 'c',
            'ⅆ' | 'ᴅ' => 'd',
            'ℯ' | 'ⅇ' | 'ᴇ' => 'e',
            'ꜰ' => 'f',
            'ℊ' | 'ɢ' => 'g',
            'ℎ' | 'ℏ' | 'ʜ' => 'h',
            'ℹ' | 'ⅈ' | 'ɪ' => 'i',
            'ⅉ' | 'ᴊ' => 'j',
            'ᴋ' => 'k',
            'ℓ' | 'ʟ' => 'l',
            'ᴍ' => 'm',
            'ɴ' => 'n',
            'ℴ' | 'ᴏ' => 'o',
            'ᴘ' => 'p',
            'ǫ' => 'q',
            'ʀ' => 'r',
            'ꜱ' => 's',
            'ᴛ' => 't',
            'ᴜ' => 'u',
            'ᴠ' => 'v',
            'ᴡ' => 'w',
            'ʏ' => 'y',
            'ᴢ' => 'z',
            other => other,
        },
    }
}

fn fold_letter(c: char) -> impl Iterator<Item = char> {
    // Capitals first: a capital Eta reads as H, and its small letter reads as n.
    let (plain, lowered) = match capital_lookalike(c) {
        Some(latin) => (Some(latin), None),
        None => (None, Some(c.to_lowercase())),
    };
    plain
        .into_iter()
        .chain(lowered.into_iter().flatten().map(small_lookalike))
}

/// Capitals from other alphabets that read as a Latin capital, given in lower case.
fn capital_lookalike(c: char) -> Option<char> {
    Some(match c {
        'А' | 'Α' => 'a',
        'В' | 'Β' => 'b',
        'С' | 'Ϲ' => 'c',
        'Ԁ' => 'd',
        'Е' | 'Ё' | 'Ε' => 'e',
        'Н' | 'Η' | 'Һ' => 'h',
        'І' | 'Ї' | 'Ι' | 'Ӏ' => 'i',
        'Ј' => 'j',
        'К' | 'Κ' => 'k',
        'М' | 'Μ' => 'm',
        'Ν' => 'n',
        'О' | 'Ο' => 'o',
        'Р' | 'Ρ' => 'p',
        'Ԛ' => 'q',
        'Ѕ' => 's',
        'Т' | 'Τ' => 't',
        'У' | 'Υ' | 'Ү' => 'y',
        'Ԝ' => 'w',
        'Х' | 'Χ' => 'x',
        'Ζ' => 'z',
        _ => return None,
    })
}

/// Small letters that read as a Latin one: other alphabets, accents and full-width forms.
fn small_lookalike(c: char) -> char {
    match c {
        'а' | 'α' | 'ɑ' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'в' | 'β' | 'ь' => 'b',
        'с' | 'ϲ' | 'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ԁ' | 'ď' | 'đ' => 'd',
        'е' | 'ё' | 'ε' | 'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ɡ' | 'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'н' | 'һ' | 'ĥ' | 'ħ' => 'h',
        'і' | 'ї' | 'ι' | 'ı' | 'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => 'i',
        'ј' | 'ϳ' | 'ĵ' => 'j',
        'к' | 'κ' | 'ķ' => 'k',
        'ӏ' | 'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'м' => 'm',
        'п' | 'η' | 'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'о' | 'ο' | 'σ' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ŏ' | 'ő' | 'ø' => 'o',
        'р' | 'ρ' => 'p',
        'ԛ' => 'q',
        'г' | 'ŕ' | 'ŗ' | 'ř' => 'r',
        'ѕ' | 'ſ' | 'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'т' | 'τ' | 'ţ' | 'ť' | 'ŧ' => 't',
        'и' | 'μ' | 'υ' | 'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => {
            'u'
        }
        'ν' | 'ѵ' => 'v',
        'ԝ' | 'ω' | 'ŵ' => 'w',
        'х' | 'χ' => 'x',
        'у' | 'γ' | 'ү' | 'ý' | 'ÿ' | 'ŷ' => 'y',
        'ζ' | 'ź' | 'ż' | 'ž' => 'z',
        // Full-width letters and digits sit at a fixed distance from their plain forms.
        '\u{ff41}'..='\u{ff5a}' | '\u{ff10}'..='\u{ff19}' => {
            char::from_u32(u32::from(c) - 0xfee0).unwrap_or(c)
        }
        other => other,
    }
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
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
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
    truncate_at_word(
        joined.trim_start_matches(['•', '-', '*', ' ']).trim_end(),
        240,
    )
}

/// A note that only repeats the addon's name tells a player nothing.
pub fn says_nothing_new(summary: &str, name: &str) -> bool {
    summary.is_empty() || normalize(summary) == normalize(name)
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
                | "core"
                | "beta"
                | "alpha"
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
    fn folding_a_name_sees_through_case_punctuation_and_look_alikes() {
        for same in [
            "Questie",
            "QUESTIE",
            "Quest-ie!",
            "Qu\u{0435}stie",        // Cyrillic small e
            "\u{051A}uestie",        // Cyrillic capital qa
            "Questi\u{0435}",        // Cyrillic small e at the end
            "Qu\u{03B5}st\u{0456}e", // Greek epsilon and Cyrillic i
            "Qu\u{00E9}stie",        // A plain accent
            "Que\u{0301}stie",       // A combining accent
            "Ques\u{200B}tie",       // A zero-width space
            "\u{FF31}\u{FF55}estie", // Full-width letters
        ] {
            assert_eq!(fold_name(same), "questie", "{same}");
        }
        assert_eq!(
            fold_name("\u{0391}\u{03A4}\u{039A}\u{0391}"),
            "atka",
            "Greek capitals"
        );
        assert_eq!(
            fold_name("\u{0422}\u{043E}m\u{0422}\u{043E}m"),
            "tomtom",
            "both alphabets"
        );
        assert_eq!(fold_name("!BugGrabber"), "buggrabber");
        assert_eq!(fold_name("Forever++"), "forever");
        assert_ne!(fold_name("Questie Reloaded"), "questie");
        assert_ne!(fold_name("Bagnon"), fold_name("Baganator"));
        assert_eq!(fold_name("--!!"), "", "nothing left to compare");
    }

    /// `plain` written in a style whose capitals start at `capitals` and small letters at `small`.
    fn styled(plain: &str, capitals: u32, small: u32) -> String {
        plain
            .chars()
            .map(|c| {
                let (start, first) = if c.is_ascii_uppercase() {
                    (capitals, 'A')
                } else {
                    (small, 'a')
                };
                char::from_u32(start + u32::from(c) - u32::from(first)).unwrap()
            })
            .collect()
    }

    #[test]
    fn folding_sees_through_letters_written_in_a_style() {
        let mut same = vec![
            styled("Questie", 0x1D400, 0x1D41A), // bold
            styled("Questie", 0x1D434, 0x1D44E), // italic
            styled("Questie", 0x1D468, 0x1D482), // bold italic
            styled("Questie", 0x1D504, 0x1D51E), // fraktur
            styled("Questie", 0x1D5A0, 0x1D5BA), // sans-serif
            styled("Questie", 0x1D63C, 0x1D656), // sans-serif bold italic
            styled("Questie", 0x1D670, 0x1D68A), // monospace
            styled("QUESTIE", 0x24B6, 0x24D0),   // circled
            styled("questie", 0x249C, 0x249C),   // parenthesized
            styled("QUESTIE", 0x1F130, 0x1F130), // squared
            styled("QUESTIE", 0x1F170, 0x1F170), // negative squared
        ];
        // Script and double-struck borrow some letters from the letterlike symbols.
        same.push("\u{1D4AC}\u{1D4CA}\u{212F}\u{1D4C8}\u{1D4C9}\u{1D4BE}\u{212F}".into());
        same.push(format!("\u{211A}{}", styled("uestie", 0, 0x1D552)));
        // Small capitals.
        same.push("\u{01EB}\u{1D1C}\u{1D07}\u{A731}\u{1D1B}\u{026A}\u{1D07}".into());
        for name in &same {
            assert_eq!(fold_name(name), "questie", "{name}");
            assert_eq!(fold_folder(name), "questie", "{name}");
        }
        assert_eq!(
            fold_name("Bartender\u{1D7D2}"),
            "bartender4",
            "a bold digit"
        );
        assert_eq!(
            fold_name("Bartender\u{2463}"),
            "bartender4",
            "a circled digit"
        );
        assert_eq!(
            fold_name("\u{1D6A8}\u{1D6BB}\u{1D6B1}\u{1D6A8}"),
            "atka",
            "bold Greek capitals"
        );
        assert_eq!(
            fold_name("\u{2113}ib\u{210E}"),
            "libh",
            "a script l and an italic h"
        );
        assert_ne!(
            fold_name(&styled("Bagnon", 0x1D400, 0x1D41A)),
            fold_name("Baganator")
        );
    }

    #[test]
    fn folding_a_folder_keeps_what_makes_it_a_different_folder_on_disk() {
        assert_eq!(fold_folder("DBM-Core"), "dbm-core");
        assert_eq!(fold_folder("Qu\u{0435}stie"), "questie");
        assert_ne!(fold_folder("Foo_Bar"), fold_folder("FooBar"));
        assert_ne!(fold_folder("!BugGrabber"), fold_folder("BugGrabber"));
    }

    #[test]
    fn a_folder_passes_for_another_folded_or_ignoring_case() {
        // A Cyrillic o, which only folding sees through.
        assert!(same_folder("TomT\u{043E}m", "tomtom"));
        assert!(same_folder("TOMTOM", "TomTom"));
        // Folding reads a Greek capital Eta as H and its small letter as n, so these fold apart,
        // and they're still one folder to the catalog check and the app, which ignore case.
        assert_ne!(
            fold_folder("\u{0397}ogHeals"),
            fold_folder("\u{03B7}ogHeals")
        );
        assert!(same_folder("\u{0397}ogHeals", "\u{03B7}ogHeals"));
        // So is a final sigma, which only lowering a whole name gives.
        assert!(same_folder("Hog\u{03A3}", "hog\u{03C2}"));
        assert!(!same_folder("Foo_Bar", "FooBar"));

        let mut map = FolderMap::default();
        map.add("TomT\u{043E}m", 1);
        map.add("\u{0397}ogHeals", 2);
        map.add("Hog\u{03A3}", 3);
        let found = |folder: &str| -> Vec<i32> {
            let mut found: Vec<i32> = map.alike(folder).copied().collect();
            found.dedup();
            found
        };
        assert_eq!(found("TOMTOM"), [1]);
        assert_eq!(found("\u{03B7}ogheals"), [2]);
        assert_eq!(found("hog\u{03C2}"), [3]);
        assert!(found("TomTomForever").is_empty());
    }

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
        assert_eq!(clean_name("Titan Panel [Core] 9.3.5"), "Titan Panel");
        assert_eq!(clean_name("Fightclub [BETA]"), "Fightclub");
        assert_eq!(
            clean_name("Titan Panel [Reputation]"),
            "Titan Panel [Reputation]"
        );
        assert_eq!(clean_name("Prat 3.0"), "Prat 3.0");
        assert_eq!(clean_name("<DBM Core>"), "DBM Core");
        assert_eq!(
            clean_name("<DBM> World Bosses (Vanilla)"),
            "DBM World Bosses (Vanilla)"
        );
        assert_eq!(clean_name("Decursive -Ace3-"), "Decursive");
        assert_eq!(clean_name("IceHUD -Ace3"), "IceHUD");
        assert_eq!(clean_name("MaxDps_Warlock"), "MaxDps Warlock");
        assert_eq!(clean_name("Odd\u{7}Title"), "OddTitle");
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
    fn a_summary_repeating_the_name_says_nothing() {
        assert!(says_nothing_new("Basic Chat Mods", "BasicChatMods"));
        assert!(says_nothing_new("", "Foo"));
        assert!(!says_nothing_new("Chat tweaks", "BasicChatMods"));
    }

    #[test]
    fn summaries_lose_leading_bullets() {
        assert_eq!(
            clean_summary("• Ask anything • Quest help"),
            "Ask anything • Quest help"
        );
        assert_eq!(clean_summary("- Handy"), "Handy");
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
