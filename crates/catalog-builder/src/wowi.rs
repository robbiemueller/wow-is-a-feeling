//! WoWInterface. It has no Forever tag in its metadata, so the only way to know
//! is to look inside each zip, once per id and version. We go slowly, one request
//! at a time, and send an honest User-Agent. Nothing is mirrored: the app downloads
//! from their CDN, so their counts stay right.

use std::io::Cursor;

use feeling_core::catalog::{Addon, ExternalIds, Release, Source};
use serde::{Deserialize, Serialize};

use crate::detect::FOREVER_EPOCH;
use crate::http::{Channel, Http, SMALL_BODY, ZIP_BODY};
use crate::model::{Candidate, ZipScan};
use crate::text::{clean_author, clean_name, clean_summary, zip_stem};
use crate::zipscan::{addon_folders, scan_zip};

const API: &str = "https://api.mmoui.com/v3/game/WOW";

/// A row of `filelist.json`. Numbers often arrive as strings.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Row {
    #[serde(rename = "UID", deserialize_with = "lenient_u64")]
    pub id: u64,
    #[serde(rename = "UICATID", default, deserialize_with = "lenient_string")]
    pub category: String,
    #[serde(rename = "UIVersion", default, deserialize_with = "lenient_string")]
    pub version: String,
    #[serde(rename = "UIDate", default, deserialize_with = "lenient_u64")]
    pub date_ms: u64,
    #[serde(rename = "UIName", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "UIAuthorName", default, deserialize_with = "lenient_string")]
    pub author: String,
    #[serde(rename = "UIFileInfoURL", default, deserialize_with = "lenient_string")]
    pub page: String,
    #[serde(rename = "UIDownloadTotal", default, deserialize_with = "lenient_u64")]
    pub downloads: u64,
    #[serde(
        rename = "UIDownloadMonthly",
        default,
        deserialize_with = "lenient_u64"
    )]
    pub monthly: u64,
    #[serde(rename = "UIFavoriteTotal", default, deserialize_with = "lenient_u64")]
    pub favorites: u64,
}

fn lenient_u64<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

fn lenient_string<'de, D: serde::Deserializer<'de>>(de: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// What we know about one WoWInterface file, kept between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WowiState {
    pub version: String,
    /// `None` means this version was checked and doesn't target Forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Candidate>,
}

/// Parse `filelist.json`, skipping malformed rows rather than failing on them.
pub fn parse_filelist(body: &[u8]) -> Result<Vec<Row>, String> {
    let rows: Vec<serde_json::Value> = serde_json::from_slice(body)
        .map_err(|e| format!("the WoWInterface file list doesn't parse: {e}"))?;
    Ok(rows
        .into_iter()
        .filter_map(|row| serde_json::from_value(row).ok())
        .collect())
}

/// The rows worth looking at, seeds first and then the most downloaded this month,
/// cut to `limit`. Seeds stay in even when `limit` would cut them, so small test
/// runs still cover the addons they exist for.
pub fn select_rows(rows: Vec<Row>, seeds: &[u64], limit: Option<usize>) -> Vec<Row> {
    let mut recent: Vec<Row> = rows
        .into_iter()
        .filter(|row| row.date_ms / 1000 >= FOREVER_EPOCH.unsigned_abs() && row.id != 0)
        .collect();
    let seed_rank = |row: &Row| {
        seeds
            .iter()
            .position(|s| *s == row.id)
            .unwrap_or(usize::MAX)
    };
    recent.sort_by(|a, b| {
        seed_rank(a)
            .cmp(&seed_rank(b))
            .then(b.monthly.cmp(&a.monthly))
            .then(a.id.cmp(&b.id))
    });
    if let Some(limit) = limit {
        let keep = limit.max(recent.iter().filter(|r| seed_rank(r) != usize::MAX).count());
        recent.truncate(keep);
    }
    recent
}

/// From a zip listing: is there any folder with a TOC the client could load?
/// Saves downloading zips that only ship, say, `_Vanilla` TOCs.
pub fn has_loadable_toc(files: &[String]) -> bool {
    let paths: Vec<(String, usize)> = files.iter().map(|f| (f.replace('\\', "/"), 0)).collect();
    !addon_folders(&paths).is_empty()
}

/// `listfiles/{id}.json` gives `[{"UID":..,"FileList":[..]}]`.
fn parse_listfiles(body: &[u8]) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Listing {
        #[serde(rename = "FileList", default)]
        files: Vec<String>,
    }
    let listing: Vec<Listing> =
        serde_json::from_slice(body).map_err(|e| format!("the file listing doesn't parse: {e}"))?;
    Ok(listing.into_iter().flat_map(|l| l.files).collect())
}

/// `filedetails/{id}.json` gives `[{"UIMD5":"..","UIFileName":"..",..}]`.
fn parse_details(body: &[u8]) -> Result<Details, String> {
    let mut details: Vec<Details> =
        serde_json::from_slice(body).map_err(|e| format!("the file details don't parse: {e}"))?;
    details
        .pop()
        .ok_or_else(|| "the file details are empty".to_string())
}

#[derive(Debug, Default, Deserialize)]
struct Details {
    #[serde(rename = "UIMD5", default)]
    md5: Option<String>,
    #[serde(rename = "UIFileName", default)]
    file_name: Option<String>,
}

fn valid_md5(md5: Option<String>) -> Option<String> {
    md5.map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| m.len() == 32 && m.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The direct download. No version parameter, so an entry never points at a stale copy.
pub fn download_url(id: u64) -> String {
    format!("https://cdn.wowinterface.com/downloads/getfile.php?id={id}")
}

/// Build the catalog entry for a Forever-capable file.
pub fn candidate(row: &Row, scan: &ZipScan, size: u64, md5: Option<String>) -> Candidate {
    let ids = ExternalIds {
        wowi: Some(row.id.to_string()),
        ..scan.external_ids()
    };
    let mut name = clean_name(&row.name);
    if name.is_empty() {
        name = clean_name(scan.title.as_deref().unwrap_or_default());
    }
    if name.is_empty() {
        name = scan.folders.first().cloned().unwrap_or_default();
    }
    let mut author = clean_author(&row.author);
    if author.is_empty() {
        author = clean_author(scan.author.as_deref().unwrap_or_default());
    }
    Candidate {
        addon: Addon {
            id: format!("wowi:{}", row.id),
            name,
            summary: clean_summary(scan.notes.as_deref().unwrap_or_default()),
            author,
            source: Source::Wowi { id: row.id },
            website: row.page.starts_with("https://").then(|| row.page.clone()),
            stable: Some(Release {
                version: row.version.trim().to_string(),
                url: download_url(row.id),
                size,
                date: i64::try_from(row.date_ms / 1000).unwrap_or(0),
                md5,
                ..Release::default()
            }),
            folders: scan.folders.clone(),
            deps: scan.external_deps(),
            downloads: row.downloads,
            ids,
            ..Addon::default()
        },
        toc_category: scan.category.clone(),
        wowi_category: Some(row.category.clone()),
        favorites: row.favorites,
        ..Candidate::default()
    }
}

/// Network access for one file, a trait so the flow can be tested with fakes.
pub trait WowiProbe {
    fn list_files(&mut self, id: u64) -> Result<Vec<String>, String>;
    fn download(&mut self, id: u64) -> Result<Vec<u8>, String>;
    fn details(&mut self, id: u64) -> Result<(Option<String>, Option<String>), String>;
}

pub struct LiveProbe<'a> {
    pub http: &'a Http,
}

impl WowiProbe for LiveProbe<'_> {
    fn list_files(&mut self, id: u64) -> Result<Vec<String>, String> {
        let url = format!("{API}/listfiles/{id}.json");
        let reply = self
            .http
            .get(Channel::Wowi, &url)
            .map_err(|e| e.to_string())?;
        parse_listfiles(&reply.body)
    }

    fn download(&mut self, id: u64) -> Result<Vec<u8>, String> {
        let reply = self
            .http
            .get_with(Channel::Wowi, &download_url(id), &[], ZIP_BODY)
            .map_err(|e| e.to_string())?;
        Ok(reply.body)
    }

    fn details(&mut self, id: u64) -> Result<(Option<String>, Option<String>), String> {
        let url = format!("{API}/filedetails/{id}.json");
        let reply = self
            .http
            .get(Channel::Wowi, &url)
            .map_err(|e| e.to_string())?;
        let details = parse_details(&reply.body)?;
        Ok((valid_md5(details.md5), details.file_name))
    }
}

/// Examine one file at its current version. `Ok(None)` means it doesn't target Forever.
pub fn examine(row: &Row, probe: &mut dyn WowiProbe) -> Result<Option<Candidate>, String> {
    let files = probe.list_files(row.id)?;
    if !has_loadable_toc(&files) {
        return Ok(None);
    }
    let bytes = probe.download(row.id)?;
    // A new version waiting for approval answers with a normal 200 and a web page.
    if !bytes.starts_with(b"PK") {
        return Err("WoWInterface sent a web page instead of the zip, usually because this version is waiting for approval".to_string());
    }
    let size = bytes.len() as u64;
    let scan = scan_zip(Cursor::new(&bytes), &[&row.name]).map_err(|e| e.to_string())?;
    if !scan.forever || scan.folders.is_empty() {
        return Ok(None);
    }
    let (md5, file_name) = probe.details(row.id)?;
    // The zip's own name can identify the main folder when the listing name doesn't.
    let scan = match file_name.as_deref().map(zip_stem) {
        Some(stem) => {
            scan_zip(Cursor::new(&bytes), &[&row.name, &stem]).map_err(|e| e.to_string())?
        }
        None => scan,
    };
    Ok(Some(candidate(row, &scan, size, md5)))
}

/// Fetch and parse the file list.
pub fn fetch_rows(http: &Http) -> Result<Vec<Row>, String> {
    let url = format!("{API}/filelist.json");
    let reply = http
        .get_with(Channel::Wowi, &url, &[], SMALL_BODY)
        .map_err(|e| format!("the WoWInterface file list: {e}"))?;
    parse_filelist(&reply.body)
}

/// A cached entry with the latest counts from the list. No requests needed.
pub fn refreshed(entry: &Candidate, row: &Row) -> Candidate {
    let mut entry = entry.clone();
    entry.addon.downloads = row.downloads;
    entry.favorites = row.favorites;
    entry.wowi_category = Some(row.category.clone());
    entry
}

/// A file whose new version couldn't be read: its URL now serves the new zip, so
/// the old size and checksum would make every install fail.
pub fn without_integrity(entry: &Candidate) -> Candidate {
    let mut entry = entry.clone();
    if let Some(release) = entry.addon.stable.as_mut() {
        release.size = 0;
        release.md5 = None;
    }
    entry
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn row(id: u64, monthly: u64, date_ms: u64) -> Row {
        Row {
            id,
            category: "24".into(),
            version: "1.0".into(),
            date_ms,
            name: format!("Addon {id}"),
            author: "Someone".into(),
            page: format!("https://www.wowinterface.com/downloads/info{id}"),
            downloads: 1000,
            monthly,
            favorites: 7,
        }
    }

    const RECENT: u64 = (FOREVER_EPOCH as u64 + 1000) * 1000;
    const OLD: u64 = (FOREVER_EPOCH as u64 - 1000) * 1000;

    #[test]
    fn filelist_tolerates_strings_nulls_and_junk_rows() {
        let body = br#"[
            {"UID":"3896","UICATID":"24","UIVersion":"v1.54.04","UIDate":1792507606000,"UIName":"Atlas",
             "UIAuthorName":"arith","UIFileInfoURL":"https://x.test/a","UIDownloadTotal":"4727895",
             "UIDownloadMonthly":"654","UIFavoriteTotal":"5328","UIDir":["Atlas"],"UISiblings":null},
            {"UID":7032,"UICATID":24,"UIVersion":null,"UIDate":"1792507606000","UIName":null},
            {"no":"id"},
            "garbage"
        ]"#;
        let rows = parse_filelist(body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, 3896);
        assert_eq!(rows[0].downloads, 4_727_895);
        assert_eq!(rows[0].date_ms, 1_792_507_606_000);
        assert_eq!(rows[1].version, "");
        assert_eq!(rows[1].date_ms, 1_792_507_606_000);
        assert!(parse_filelist(b"<html>").is_err());
    }

    #[test]
    fn selection_filters_by_date_and_puts_seeds_first() {
        let rows = vec![
            row(1, 10, RECENT),
            row(2, 99, RECENT),
            row(3, 5, OLD),
            row(4, 1, RECENT),
            row(5, 50, RECENT),
        ];
        let picked = select_rows(rows.clone(), &[4], None);
        let ids: Vec<u64> = picked.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            [4, 2, 5, 1],
            "seed first, then by monthly downloads, old ones gone"
        );

        let picked = select_rows(rows.clone(), &[4], Some(2));
        assert_eq!(picked.iter().map(|r| r.id).collect::<Vec<_>>(), [4, 2]);

        let picked = select_rows(rows, &[4, 1], Some(1));
        assert_eq!(
            picked.iter().map(|r| r.id).collect::<Vec<_>>(),
            [4, 1],
            "a small limit keeps seeds"
        );
    }

    #[test]
    fn loadable_toc_check_matches_the_client() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(has_loadable_toc(&list(&[
            "BugSack/",
            "BugSack/forever.lua",
            "BugSack/BugSack.toc"
        ])));
        assert!(has_loadable_toc(&list(&["Foo/Foo_Camelot.toc"])));
        assert!(has_loadable_toc(&list(&["Foo/Foo_Mainline.toc"])));
        assert!(has_loadable_toc(&list(&["Foo\\Foo.toc"])));
        assert!(!has_loadable_toc(&list(&[
            "Foo/Foo_Vanilla.toc",
            "Foo/Foo_TBC.toc"
        ])));
        assert!(
            has_loadable_toc(&list(&["Wrapper/Sub/Sub.toc"])),
            "the installer unwraps this"
        );
        assert!(
            has_loadable_toc(&list(&["Foo.toc", "core.lua"])),
            "and this"
        );
        assert!(!has_loadable_toc(&list(&[
            "__MACOSX/__MACOSX.toc",
            "images/images.toc",
            "OpenMe/OpenMe.toc"
        ])));
        assert!(!has_loadable_toc(&[]));
    }

    #[test]
    fn listings_and_details_parse() {
        let files =
            parse_listfiles(br#"[{"UID":5995,"FileList":["BugSack/","BugSack/BugSack.toc"]}]"#)
                .unwrap();
        assert_eq!(files.len(), 2);
        assert!(parse_listfiles(b"{}").is_err());
        let details = parse_details(br#"[{"UID":"1","UIMD5":"24806C83D9411629B3F3474E17CAEEED","UIFileName":"BugSack-v1.zip"}]"#).unwrap();
        assert_eq!(
            valid_md5(details.md5).as_deref(),
            Some("24806c83d9411629b3f3474e17caeeed")
        );
        assert_eq!(valid_md5(Some("short".into())), None);
        assert_eq!(valid_md5(None), None);
        assert!(parse_details(b"[]").is_err());
    }

    struct Fake {
        files: Result<Vec<String>, String>,
        zip: Result<Vec<u8>, String>,
        md5: Result<(Option<String>, Option<String>), String>,
        downloads: u32,
    }

    impl WowiProbe for Fake {
        fn list_files(&mut self, _: u64) -> Result<Vec<String>, String> {
            self.files.clone()
        }
        fn download(&mut self, _: u64) -> Result<Vec<u8>, String> {
            self.downloads += 1;
            self.zip.clone()
        }
        fn details(&mut self, _: u64) -> Result<(Option<String>, Option<String>), String> {
            self.md5.clone()
        }
    }

    fn zip_with(files: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn fake(zip: Vec<u8>) -> Fake {
        Fake {
            files: Ok(vec!["Foo/Foo.toc".into()]),
            zip: Ok(zip),
            md5: Ok((Some("a".repeat(32)), Some("Foo-1.0.zip".into()))),
            downloads: 0,
        }
    }

    #[test]
    fn a_forever_zip_becomes_a_complete_entry() {
        let zip = zip_with(&[
            (
                "Foo/Foo.toc",
                "## Interface: 16001\n## Title: Foo Title\n## Notes: Does foo\n## RequiredDeps: Ace3\n",
            ),
            ("__MACOSX/x", ""),
        ]);
        let size = zip.len() as u64;
        let mut probe = fake(zip);
        let mut r = row(42, 1, RECENT);
        r.name = "Foo".into();
        let entry = examine(&r, &mut probe).unwrap().unwrap();
        assert_eq!(entry.addon.id, "wowi:42");
        assert_eq!(entry.addon.name, "Foo");
        assert_eq!(entry.addon.summary, "Does foo");
        assert_eq!(entry.addon.folders, ["Foo"]);
        assert_eq!(entry.addon.deps, ["Ace3"]);
        assert_eq!(entry.addon.ids.wowi.as_deref(), Some("42"));
        assert_eq!(entry.addon.source, Source::Wowi { id: 42 });
        let release = entry.addon.stable.unwrap();
        assert_eq!(
            release.url,
            "https://cdn.wowinterface.com/downloads/getfile.php?id=42"
        );
        assert_eq!(release.size, size);
        assert_eq!(release.md5, Some("a".repeat(32)));
        assert_eq!(release.date, i64::try_from(RECENT / 1000).unwrap());
    }

    #[test]
    fn no_loadable_toc_means_no_download() {
        let mut probe = fake(Vec::new());
        probe.files = Ok(vec!["Foo/Foo_Vanilla.toc".into()]);
        assert_eq!(examine(&row(1, 1, RECENT), &mut probe), Ok(None));
        assert_eq!(probe.downloads, 0);
    }

    #[test]
    fn a_zip_for_another_game_version_is_a_clean_negative() {
        let mut probe = fake(zip_with(&[("Foo/Foo.toc", "## Interface: 120105\n")]));
        assert_eq!(examine(&row(1, 1, RECENT), &mut probe), Ok(None));
    }

    #[test]
    fn failures_are_errors_so_they_get_retried() {
        let good = zip_with(&[("Foo/Foo.toc", "## Interface: 16001\n")]);

        let mut probe = fake(good.clone());
        probe.files = Err("HTTP 502".into());
        assert!(examine(&row(1, 1, RECENT), &mut probe).is_err());

        let mut probe = fake(good.clone());
        probe.zip = Err("HTTP 503".into());
        assert!(examine(&row(1, 1, RECENT), &mut probe).is_err());

        let mut probe = fake(b"<!DOCTYPE html PUBLIC>World of Warcraft AddOns".to_vec());
        let waiting = examine(&row(1, 1, RECENT), &mut probe).unwrap_err();
        assert!(waiting.contains("waiting for approval"), "{waiting}");

        let mut probe = fake(b"PK but not really a zip".to_vec());
        assert!(examine(&row(1, 1, RECENT), &mut probe).is_err());

        let mut probe = fake(good);
        probe.md5 = Err("HTTP 500".into());
        assert!(examine(&row(1, 1, RECENT), &mut probe).is_err());
    }

    #[test]
    fn names_fall_back_from_listing_to_title_to_folder() {
        let scan = ZipScan {
            folders: vec!["Foo".into()],
            forever: true,
            title: Some("Foo Title".into()),
            ..ZipScan::default()
        };
        let mut r = row(1, 1, RECENT);
        assert_eq!(candidate(&r, &scan, 1, None).addon.name, "Addon 1");
        r.name = String::new();
        assert_eq!(candidate(&r, &scan, 1, None).addon.name, "Foo Title");
        let bare = ZipScan {
            folders: vec!["Foo".into()],
            ..ZipScan::default()
        };
        assert_eq!(candidate(&r, &bare, 1, None).addon.name, "Foo");
    }

    #[test]
    fn the_zip_name_can_correct_the_main_folder() {
        let zip = zip_with(&[
            (
                "Alpha/Alpha.toc",
                "## Interface: 16001
## Notes: alpha
",
            ),
            (
                "Omega/Omega.toc",
                "## Interface: 16001
## Notes: omega
",
            ),
        ]);
        let mut probe = fake(zip);
        probe.md5 = Ok((None, Some("Omega-1.0.zip".into())));
        let mut r = row(1, 1, RECENT);
        r.name = "Something Else".into();
        let entry = examine(&r, &mut probe).unwrap().unwrap();
        assert_eq!(entry.addon.folders, ["Omega", "Alpha"]);
        assert_eq!(
            entry.addon.summary, "omega",
            "details follow the main folder"
        );
    }

    #[test]
    fn refreshed_entries_take_new_counts_and_stale_ones_lose_integrity_data() {
        let scan = ZipScan {
            folders: vec!["Foo".into()],
            forever: true,
            ..ZipScan::default()
        };
        let entry = candidate(&row(1, 1, RECENT), &scan, 123, Some("a".repeat(32)));
        let mut newer = row(1, 1, RECENT);
        newer.downloads = 5000;
        newer.favorites = 9;
        newer.category = "45".into();
        let fresh = refreshed(&entry, &newer);
        assert_eq!(fresh.addon.downloads, 5000);
        assert_eq!(fresh.favorites, 9);
        assert_eq!(fresh.wowi_category.as_deref(), Some("45"));
        assert_eq!(fresh.addon.stable.as_ref().unwrap().size, 123);

        let stale = without_integrity(&entry);
        let release = stale.addon.stable.unwrap();
        assert_eq!(release.size, 0);
        assert_eq!(release.md5, None);
    }
}
