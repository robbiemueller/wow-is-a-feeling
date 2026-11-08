# WoW is a Feeling: design

A lightweight, free, open-source addon manager for **World of Warcraft: Forever only**, with a vanilla (2004) feel. Named after the *Pure Pwnage* music video "WoW is a feeling". Not affiliated with Blizzard Entertainment.

## Principles

1. **Performance first.** Nothing runs when the window is closed. Nothing redraws when the window is idle. No background service, tray icon, autostart, telemetry, ads, accounts or plugin system.
2. **Never need a website.** Browse, install, update and remove all happen inside the app.
3. **Never lose someone's addons.** Installs are atomic and journaled, and folders we don't own are left alone.
4. **Forever only.** No flavor switcher, no other game versions.
5. **Original art and words only.** No Blizzard textures, logos, icons, sounds or fonts. No song lyrics, video frames, clips or audio. Easter eggs use our own text and link out to the video.

## Facts about Forever

Verified against real files, Blizzard's CDN config and the BigWigs packager in November 2026.

| Thing | Value |
|---|---|
| Product code | `wowf`, in `_forever_/.flavor.info`, root `.build.info`, Battle.net `product.db` |
| Folder | `World of Warcraft/_forever_/Interface/AddOns` |
| Windows binary | `Wow.exe` or `Wow-ARM64.exe` (compare case-insensitively) |
| macOS binary | `World of Warcraft.app`, in `/Applications/World of Warcraft/_forever_`, macOS 12+ |
| Linux | No native client. Runs in Wine, Lutris, Bottles, Proton or Faugus prefixes |
| Interface number | 16001 today, treat 16000-19999 as Forever |
| Game version | 1.60.1 (never hardcode the build) |
| TOC suffix order | `_Forever`, `-Forever`, `_Camelot`, `-Camelot`, `_Mainline`, `-Mainline`, plain |
| `release.json` flavor | `forever`, alias `camelot` |
| Asset naming | often contains `forever` or `camelot` |

Traps found in real addons:

- A UTF-8 BOM before `##` (Pawn).
- `Questie.toc` declares `00000`, yet the addon is fine via `Questie_Forever.toc`.
- Zips that are only partly for Forever (DBM-Core 4 of 8 folders, BigWigs 4 of 12).
- Junk top-level folders (`__MACOSX`, `images`, `OpenMe`).
- Folder names with a leading `!` (`!BugGrabber`).
- Big zips: Questie and OpenMap are around 90 MB, and voice and map packs reach 1.3 GB.
- `/releases/latest` can point at another flavor (VuhDo's latest is a vanilla build), so scan recent releases.
- Tags with slashes and `+` (`bundle/v12.6.2+v1.0.11`).
- Same-named repos that are different projects (`ForeverPlus`, `ForeverUI`). Sources are only linked by a pair someone approved, never by name, TOC IDs or folders.
- The `_retail_`, `_classic_` and `_classic_era_` folders can be leftovers with no game in them. Folder presence alone proves nothing.

## Layout

```
crates/core             UI-free library. Everything except drawing.
crates/app              The egui app, binary name wow-is-a-feeling.
crates/catalog-builder  CI tool that builds catalog.json.
crates/xtask            Release chores: signing keys, latest.json.
assets/                 Icon, fonts, skin textures. All original or OFL.
catalog/                Hand-maintained seeds, curated entries, kits, gaps.
site/                   Landing page for GitHub Pages.
```

## Core modules

`crates/core` has no UI dependency. It blocks freely: the app calls it from worker threads, and it never spawns threads it doesn't join. Errors use `error::Error`, whose `Display` text is written for players, not programmers.

| Module | Job |
|---|---|
| `toc` | Parse `.toc` files, pick the TOC the client would load, decide Forever support |
| `catalog` | The catalog data model |
| `paths` | App config, cache and log directories |
| `logging` | Small rotating file logger, no dependencies beyond `log` |
| `detect` | Find and validate Forever installs per OS |
| `scan` | Read `Interface/AddOns` into folders with parsed TOCs, and group them |
| `state` | Persistent settings and managed-addon records, written atomically |
| `search` | Fast fuzzy search over the catalog, blended with popularity |
| `deps` | Required-dependency resolution and removal warnings |
| `gameproc` | Is the game running? Checked only before acting, never polled |
| `listfile` | Plain-text addon list export and import |
| `net` | HTTP with ETag support, streaming downloads with progress and cancel |
| `extract` | Safe zip extraction: no traversal, no bombs, no file types known to run on a double-click |
| `install` | Staging, journaled atomic swap, rollback, crash recovery |
| `backup` | WTF settings backup and restore |
| `selfupdate` | Signed app updates through GitHub Releases |
| `details` | On-demand README and changelog text for the parchment panel |

### Install transaction

Staging lives in `<_forever_>/Interface/.wiaf/`, which is on the same volume as `AddOns` and outside it, so the game never scans it.

1. Hold `.wiaf/j/<txn>.lock` open for the whole transaction, so `recover` leaves a live one alone.
2. Download to `.wiaf/d/<txn>.zip` and verify size and digest when known. WoWInterface is the exception: its link always serves the newest upload, so a catalog entry's size and MD5 can describe an older file. A download that starts with markup is a web page, not a zip, and fails as one before those checks. WoWInterface sends one, with a success code, for every download of an addon while a new upload of it waits for approval (`UIPending` in its file details), so the app asks it why before telling the player.
3. Extract into `.wiaf/s/<txn>/` through the safe extractor.
4. Require at least one folder with a TOC. Warn, don't fail, when no folder targets Forever.
5. Write `.wiaf/j/<txn>.json` listing every folder to swap, flushed to disk.
6. Take `.wiaf/swap.lock`. For each folder: rename the old one into `.wiaf/b/<txn>/`, then rename the new one into `AddOns`. Retry renames with backoff on transient locks from antivirus and cloud-sync tools.
7. Mark the journal committed, delete the backup and staging, delete the journal.
8. On any failure, undo in reverse. On startup, a leftover pending journal rolls back; a committed one finishes cleanup.

Nothing is deleted until the journal says, durably, which way the transaction went. Staged addon files aren't synced one by one, because that's slow on big addons: a process crash is fully safe, while a power cut just after a commit can leave a new addon incomplete, which a reinstall fixes.

Folders we own are recorded in `state`. Removal deletes only folders no other managed addon claims. A catalog install replaces only its record's folders and ones the player agreed to, and anything else in its zip that's already on disk stops it. A zip the player dropped or a rollback replaces same-named folders through the same backup-and-swap and says so. Two addons may claim one folder (shared libraries). `UI-SPEC.md` has the details under Replacing folders.

### Safe extraction

Never call `ZipArchive::extract`. Hand-roll it on top of `zip`:

- Open zips with `ArchiveOffset::Known(0)`, like the catalog scan, so both read the same file list. Data in front of a zip gets it refused.
- Use `enclosed_name()`; reject symlinks, `:`, Windows device names, trailing dots or spaces, and case-only duplicates.
- Never trust declared sizes. Wrap readers in `take(limit)`, with per-file, per-archive, entry-count and ratio caps.
- Cap what planning has to hold or walk: 64 MB of file list, 1,024 bytes and 64 levels per path, 20,000 folders. Real zips reach 11 MB, 139 bytes, 11 levels and 1,698 folders.
- Skip junk (`__MACOSX`, `.git`, `.github`, `.DS_Store`, `Thumbs.db`).
- Don't write executables, scripts, shortcuts or disk images (`exe dll scr com msi bat cmd ps1 sh jar vbs app dylib so`, then `js py url reg iso zip` and the rest of the list in `extract.rs`). Skip them, log them, and report them in the result. Dev scripts appear in real source archives, so skipping beats failing. A list only stops what it names, so this keeps out known lures rather than promising nothing runnable gets through. Nothing the game loads is on it.
- Only keep top-level folders that contain a TOC.

### Self-update

Checked once at launch, in the background, after the first frame. Never silent: the player sees the version and notes and picks **Update now**, **Later** or **Skip this version**.

- `https://github.com/robbiemueller/wow-is-a-feeling/releases/latest/download/latest.json` plus `latest.json.minisig`. That URL is a plain redirect, so it doesn't touch GitHub's REST rate limit.
- Verify the signature over `latest.json` with the public key embedded in the app, using `minisign-verify`, then compare versions, download the asset, and check its SHA-256 from the signed file.
- Windows: rename the running exe aside and the verified download into its place, and rename it back if that fails, so a working exe stays at the path. The old file is only deleted after that, with `self-replace`. Bare Linux binary: swap with `self-replace`. AppImage: replace `$APPIMAGE`, believed only when the running exe is inside `$APPDIR`, because child processes inherit both. macOS: replace the whole `.app`.

`latest.json`:

```json
{
  "version": "0.2.0",
  "notes": "What changed, in plain words.",
  "date": 1794000000,
  "assets": {
    "windows-x64": { "url": "...", "sha256": "...", "size": 123, "kind": "exe" },
    "linux-x64-appimage": { "url": "...", "sha256": "...", "size": 123, "kind": "appimage" },
    "linux-x64-bin": { "url": "...", "sha256": "...", "size": 123, "kind": "bin.gz" },
    "macos-universal": { "url": "...", "sha256": "...", "size": 123, "kind": "app.zip" }
  }
}
```

`bin.gz` is one gzipped binary and `app.zip` is a zipped `.app`, so the updater needs no tar support.

## Catalog

`crates/core/src/catalog.rs` is the schema. CI builds one `catalog.json` and force-pushes it to the orphan `catalog` branch, so clones stay small. The app fetches it from `raw.githubusercontent.com` with `If-None-Match`, caches it, and parses it on a worker thread.

Update checks compare installed versions against the catalog: one request, not one per addon.

### Sources

- **GitHub** is the backbone. Seeds: the public-domain strongbox catalogue, topic and code search with a token, and `catalog/seeds.toml`. Detection order per release, newest first, because each step exists for a real addon:
  1. `release.json` metadata with flavor `forever`/`camelot` or a 16xxx interface.
  2. A zip asset whose name contains `forever` or `camelot`.
  3. Read the TOC inside candidate zips with HTTP range requests.
- **WoWInterface** has no Forever tag in its metadata, so scan zips: filter `api.mmoui.com/v3/game/WOW/filelist.json` to recently updated addons, download each zip once per version, and read its TOCs. Cache by id and version. Go slowly (one at a time), send an honest User-Agent with the project URL, and never mirror anything. The app downloads straight from their CDN, so their download counts stay intact.
- **Curated source installs** (`catalog/curated.toml`): addons whose authors publish tags but no packaged zips, such as Details. The app installs GitHub's source archive using `Release.moves`. An addon whose `.pkgmeta` pulls in externals can't be installed this way, because its tags don't contain the libraries it needs.
- **Not used, on purpose:** CurseForge and Wago (their terms need approval or a key), the Tukui API (asks for staff permission), and any scraping of websites. Well-known gaps appear in `Catalog.gaps` with alternatives. ElvUI is one of them: its tags need 20 externals, and its packaged zips are only on Tukui's CDN. `curated.toml` keeps its entry, so it appears if that changes.

Tier is a popularity color only: top 10 legendary, next 5% epic, next 15% rare, next 30% uncommon, rest common. Grey is reserved for addons that no longer target Forever.

### Who owns a name

Discovery has no review, so anyone with a free GitHub account gets looked at, and everything in their repository is theirs to write: its title, folders, TOC ids and download counts. None of that proves anything, so identity works like this.

- **First come, oldest wins. Hand-picked beats everything.** Hand-picked means `catalog/seeds.toml`, anything `catalog/curated.toml` names except in `[pins]`, which only says which addon an entry is and what it was, and both sides of an approved merge. A hand-picked repository that GitHub renames isn't hand-picked under its new name until those files use it, and the build summary names it until then.
- **Age.** `Addon.created` is when an entry first appeared. It comes from the last published catalog, because the scan cache can reset. An entry that catalog left out keeps the age the scan cache remembers from the last catalog it was in, for the 30 days after that catalog's day, so one build it was held or left out of doesn't make it new. The scan cache fills in from the last catalog whatever it has no newer note of, so this holds from the first build that keeps a memory. After GitHub renames a repository, its age is the one its old id had, and what the old id was is the renamed entry's alone: once the rename is published, the scan cache forgets the old id, and anything at the old id with the renamed entry's name and main folder is new, such as a stranger's copy once an account rename frees the name. A new entry gets the build's time. An entry that changes its name or main folder counts as new from then on, unless it's hand-picked, so a repo can't get established as one thing and then retitle itself. Everything listed before ages were kept shares one old date, 2026-07-01.
- **Trust.** `Addon.trusted` is what lets the app pick an entry by itself. It's set for hand-picked entries, WoWInterface listings (moderated, and identified by listing id) and anything at least 14 days old, and for nothing else. An entry that isn't trusted has its ids on other sites cleared, because they're only claims. Among the rest an id stays with whoever got there first, and a listing always owns its own id.
- **Reviewed.** `Addon.reviewed` is set for hand-picked entries and nothing else. The app only installs a reviewed entry by itself as another addon's dependency, or a pack a reviewed addon lists whose download is known to be small (see Packs), because any addon can ship a library's folder long before anything needs it, and age or a listing proves nothing about that. To let a library be picked that way, seed it. Big packs are the exception: a release line, or an id `[[packs]]` names, whose stable or beta download is over 100 MB, or of unknown size, goes out unreviewed, and still trusted. Apps up to 0.1.2 install any reviewed dependency by themselves whatever its size, and a voice pack can be 1.5 GB. Later apps never install a pack that big, or one whose size isn't known, unasked, and still bring one whose build on the player's channel is known to be 100 MB or less (see Packs). They offer packs by their groups, which only needs them trusted. The build summary names each one.
- **Dependencies.** A folder that addons in the last catalog require, and that nothing listed with it then still ships, can only be filled by something hand-picked. Anything else that starts shipping it is held, however old it is, WoWInterface listings included. Both sides come from the last catalog, so the order of the pass can't decide it, and a newcomer that requires a folder can't get whatever ships it next held.
- **Merging.** A WoWInterface listing merges into a GitHub entry only when `merges` in `curated.toml` approves that pair, and GitHub's build is the one offered. A TOC id or matching folders never merge anything, remove a listing or move its counts. A listing that isn't approved stays its own entry, or is held if its name or main folder is taken.
- **Collisions.** Candidates go in order, hand-picked first and then oldest. Each claims its name, folded for case, punctuation and letters that only look Latin, and its folders. A hand-given name that ends in a parenthetical, such as a pack's "(English)", claims the name without it too, so nobody else lists as "Spoken Audio: Quests". One whose name or main folder is already claimed is held. So is one with a side folder claimed by something strictly ahead of it, and one that has just added a folder another listed entry already had, however old it is. Entries from before ages were kept stay as they were among themselves: only exact copies in repositories, with the same name and main folder, compete. The same goes for two hand-picked entries.
- **Vouched folders.** A folder in an entry's `formerly` (see One product per entry) is claimed by that entry too, and so is the old main folder a WoWInterface, direct or release-line entry moves away from. An entry missing from a build, because it's held, left out over its pin or trimmed, keeps those claims while the last catalog lists it, or for the 30 days after the day of the last catalog the scan cache remembers it in. A held entry's `was` folders count for as long as it's there, because a person wrote them down. An entry left out on purpose claims nothing (see Left out on purpose). While an entry is missing from a build, its own GitHub owner may ship a folder vouched for it, as it may take its name and main folder. After GitHub renames a repository, its claims go with it, so its old id claims nothing against it. Age decides, as it does for current folders: hand-picked first, then oldest, and two entries from before ages were kept are level, as are two hand-picked ones. Anything else that ships a vouched folder is held unless it has the better claim: it ranks ahead of the entry vouched for or level with it, and the last catalog already listed it with that folder, unless it's hand-picked. A folder an entry has only now added is a claim from today, however old the entry is. While the folder was that entry's main one, anything younger shipping it was held, so a younger one listed with it got it while the entry was away or after its rename. One with the better claim keeps the folder, the vouch goes, and the build summary lists it under "former main folders no longer vouched for". Folders count as the same when they fold alike, as names do, or match ignoring case, as the catalog check and the app compare them. The gate drops any vouch for a folder another entry in the catalog ships, with a warning naming both, so no catalog goes out with one, and it never refuses a catalog for that. So a stranger can neither take the installed copies a rename left behind nor strip the vouch for them.
- **Gaps.** Only a hand-picked entry closes a gap. Each gap has a hand-written `folders` list with the addon's real folder names, and `reserved` does the same for seeded repositories with no entry yet. Anything else claiming one of those, or a name that reads the same, is held.
- **Forks.** A fork goes away only when it has the same folders as its real parent or a better-known sibling. Some other repository with those folders doesn't count, and a hand-picked fork always stays.
- **Caps.** An owner gets 40 entries unless they're hand-picked, and new repositories past that aren't even read. A zip past 256 folders counts as having none, and text, dependencies and ids from a TOC are cut to size before they reach the scan cache.
- **Held** means not published, and listed with the reason in `catalog-held.json`, which the catalog branch also carries, so a maintainer can look. To let something in, seed it or approve a merge. A held entry doesn't age: one that was never listed stays new until it is, and one that was keeps its age for the 30 days after the day of its last listing (see Age). An established entry held in a build keeps its name and main folder as claims at its rank, and so does one missing from it that the last catalog lists, or the scan cache remembers from a catalog in those 30 days. A hand-picked one whose hand-given name ends in a parenthetical claims the name without it too, as it does when listed. Anything that ranks behind it with that name or main folder is held, unless it's hand-picked or from the same GitHub owner, whose own move that is. So a stranger can't take an entry's name while it's away for a build and then shut it out as the older entry.
- **Left out on purpose.** Some entries a build doesn't list claim nothing, neither names nor folders. That's one a maintainer excluded, a listing folded into the repository `merges` approves, a repository GitHub says is archived, disabled or empty, and a listing WoWInterface's list no longer has, each of which is said again on every run. It's also a fork dropped beside its parent or a better-known sibling, and a fork gone from a later build that has its parent with the folders the fork was last published with, unless the fork is hand-picked. GitHub not finding a repository isn't one of these, because a repository made private or suspended can come back, so that fails closed: an author's own move across sources, or to another GitHub owner without a transfer, waits out the old entry's claims. Seeding the new entry, or excluding the old id, lets it in at once, and the build summary says so for each one under "held for an entry this build doesn't list".

The zip scan reads a zip the way the installer does, through its own name cleaning and layout rules, so a zip can't show the catalog one list of folders and install another. A zip claiming to start past its first byte has two possible directories, and is treated as having nothing in it.

### One product per entry

An installed addon is only ever updated to a newer build of the same addon. Some repositories publish several addons: Spoken's publishes the addon under `spoken/v3.8.3` and each voice pack under its own tag prefix, such as `quests-audio/v2.5.0`, and ClassicUI Forever releases standalone modules beside the main addon. Taking a repository's newest release, whatever it holds, would turn the listing into another addon, and an update would then replace the installed addon's folders with that one's.

- **Pinned to a product.** A GitHub entry the last catalog listed keeps its product: a new build only counts if it still ships the entry's main folder, compared ignoring case. That folder is the `main` that `[pins]` in `curated.toml` names for the entry, or else the last catalog's main folder, or else the main folder the entry was last published with, which the scan cache remembers for every entry until a maintainer excludes it, so a catalog that left it out doesn't make it another addon. Its name, folders and parent are kept only for the 30 days they count, so a flood of entries that come and go costs the scan cache a few hundred bytes each. With none of those, and no last catalog to read, the scan cache's last result says. After GitHub renames a repository, the scan cache keeps the id it had on every later result, so the pin is still found under the id the last catalog used, in every build after the rename. A newer release that doesn't ship it, whether another product or a rename, is skipped without counting as a failed read. The build summary names every entry that passed over a newer release, every one kept at its last build because nothing in reach qualified, and every one left out for that, so a maintainer can decide. A last build is only kept while its file can still be there, which a GitHub rename doesn't change: the old link still leads to the file. An entry left out stays out until a build that ships its main folder is in reach, or `[pins]` says which folder it should list.
- **The pin stays first.** A build that ships the pinned folder lists it first, whatever else it holds, and that folder's TOC gives the entry its name, notes, author, dependencies, category and ids. So a pinned entry's main folder never moves by itself: a release bundling the entry's addon with another of the repository's is still listed as the entry's addon, and a later release of the other one alone is passed over.
- **Releases of several addons.** Once a zip read in a release is another addon's, reading goes on past the usual five until the pinned addon's build turns up, up to 20, because its own zip can sit behind bigger ones and zips for other flavors. A release with no build of the pinned addon in what was read is remembered as another addon's, so it's read again only when its files or the pin change. The summary names one that still had zips nobody read.
- **Saying which addon it is.** `[pins."<id>"]` takes `main`, the folder that identifies the entry's addon, and `was`, main folders the addon really had before. `main` is only for a repository's own GitHub entry outside `[[split]]`: a WoWInterface or direct entry holds one addon, and a `[[split]]` entry's tag prefix decides its product, so those aren't pinned. `main` fixes an entry the last catalog had already turned into another product: foreversixty's nightly data pack had replaced the addon. Together with `was` it lets a real rename through. `was` alone works for any entry, release lines and `[[split]]` repositories included, so a person can write back a vouch that was lost or never made.
- **What it used to be.** `Addon.formerly` lists main folders the catalog vouches were this addon's. The builder carries it over from the last catalog, or for an entry that catalog left out, from what the scan cache remembers the entry was last published with, so a build away loses nothing, and after GitHub renames a repository, from what its old id was published with. It adds the `was` folders of a pin, and the old main folder whenever a WoWInterface, direct or release-line entry's main folder changes, since each of those holds one addon. A GitHub repository can hold several addons, so a main folder that moves there is never vouched for by itself: the next release could be the repository's other addon. A pinned entry's main folder doesn't move, and the summary names a move in an entry with no pin, such as a `[[split]]` repository's own, so a person can write `was`. Each folder vouched for is a claim (see Vouched folders), it's never the entry's current main folder, and the list keeps the 8 most recent: the folder it just moved away from, then the ones it carried, then `was` folders not in it yet. So what's vouched for keeps its place, and the summary names a `was` folder that doesn't fit. The app counts an installed copy whose main folder is in `formerly` as the same addon, so a rename reaches everyone who has it.
- **Release lines.** `[[split]]` in `curated.toml` names a seeded repository, the tag prefix of its own addon (`main`) and the prefixes that are addons of their own (`lines`). Tags look like `<prefix>/v1.2.3`, and a prefix matches a whole segment, so `quests-audio` never takes `quests-audio-deDE/v1.0.1`. The repository's own entry keeps `gh:owner/repo` and reads only `main`'s tags. Each line becomes `gh:owner/repo/<line>`, lowercase, and reads only its own. Tags under any other prefix, such as desktop apps, retired packs and languages nobody has checked, are never read.
- Each prefix's newest releases are fetched on their own, so a busy prefix can't push another out of reach, and the prefix is cut off before detection, so it never decides prerelease, flavor or the version text. Downloads count each line's own zips.
- Lines are hand-picked, so they're trusted from their first build, and reviewed unless their download is over 100 MB, as each of Spoken's is, or of unknown size (see Reviewed). A line's `source` is the plain repository and its `website` is its current release's page. Its name and summary come from `[overrides]`, because a pack's TOC title describes one of its folders. The app shows a line's summary rather than the repository's README and releases, which are about the main addon.

### Packs

Some addons do nothing until another addon is installed beside them: Spoken plays voices that come in separate voice packs, and neither side's TOC says so. `[[packs]]` in `curated.toml` writes that down as groups on the addon: a `name` such as "Quest voices", whether it's `needed`, the `ids` that will do, in the order to offer them, and `met_by`, folder name prefixes that also meet it, for packs the catalog doesn't list, such as other languages or another tool's. The builder copies the groups onto `Addon.packs`, drops ids the catalog doesn't list and groups left empty, and warns about both. Naming an id there hand-picks it, so it's trusted, and reviewed unless its download is over 100 MB or of unknown size (see Reviewed). Pack names say their language, such as "Spoken Audio: Quests (English)".

The app reads only a reviewed entry's groups, and only trusted ids the catalog has. A group is met when any folder of any of its ids is in AddOns, whoever installed it, or any folder there starts with one of its `met_by` prefixes. A pack is an id a reviewed addon lists under its packs. The packs popup before a first install, the pack's own page and a pasted list's preview each show its size. A pack is picked by itself as another addon's dependency only when its download is known to be 100 MB or less, reviewed or not. A bigger one is left out with a yellow line giving its size, and so is one whose size the catalog doesn't give, which could be any size, with a line saying it's unknown, or that there's no build yet when there's none. Either way the Requires line says "(install it yourself)". A pack known to be up to 100 MB comes along as a dependency even when it's published unreviewed, which happens when its build on the other channel is big, because the reviewed addon's hand-written group vouches for it. The size that counts is the build the player's channel would install. That gate is for packs only, not for every dependency. Any other required addon a person reviewed comes along whatever its size, because the addon that requires it won't load without it, as Musician's plugins need Musician. Apps before 0.1.3 have no gate, and they're protected because the catalog publishes big packs, and packs of unknown size, unreviewed, which those apps never install by themselves. See UI-SPEC.md.

### What stops a build

One bad addon is dropped with a warning, and one stranger must never be able to stop the catalog for everyone, so very little is a refusal.

- More than half of the last catalog is missing and no source said why. A repository GitHub says is gone, archived or empty explains itself, and so does one a maintainer excluded, so a flood of junk that gets deleted doesn't freeze anything. An outage or a lost scan cache does stop it.
- More than half of the last catalog's hand-picked entries are missing, for any reason. A stranger can't cause that, but a bad answer from a source can.
- `catalog.json` can't be read. Ages come from it, and without it an entry keeps its age only if the scan cache remembers it from the last 30 days.

Running the workflow by hand with `accept_smaller` skips all three, once someone has looked. The catalog also has a size limit of 8 MiB, past which the newest and largest entries that aren't hand-picked are held and the largest is named in the log. `catalog-state.json` over 95 MiB would stop every push, so the previous copy stays instead, the catalog still goes out, and the log names the largest repositories and the size of the memory of what each entry was published as.

## UI

Quest log layout, rendered with egui on the glow backend.

- **Frame:** frameless window with our own chrome. Dark stone, gold trim, round portrait with the CRT logo overlapping the top-left corner, gold title, small red close button, resizable.
- **Toolbar:** black search box ("Search addons, or type /help"), category and sort filters, red **Update all (n)**.
- **Left list:** collapsible headers like quest zones, addon names colored by tier, version at the right. Virtualized with `show_rows`; thousands of rows must scroll at full speed.
- **Right page:** parchment with description, author, source, version, date, size, requirements, on-demand README and changelog, and red buttons: **Install**, **Update**, **Remove**, **Share**. Labels stay literal.
- **Tabs along the bottom:** My Addons, Browse, Settings.
- **Casting bar** for downloads, drawn only while something runs. Red error text and yellow system messages at top center, which fade out.
- **Tooltips** styled like item tooltips (dark navy, silver border, colored title, green "Use:" line, yellow flavor quote).

Stable keyboard paths: `/` or Ctrl+F focuses search, arrows move the selection, Enter acts on it. AccessKit stays on.

### Easter eggs (hidden, labels stay clear)

All text is our own. No lyrics.

- Slash commands in the search box: `/help`, `/played`, `/reload` (really rescans), `/who`, `/roll`, `/dance`, `/ding`, `/afk`, `/pwnage`, `/feeling`.
- Click the portrait five times: the CRT switches off and a yellow message insists it's only being cleaned.
- Leave the app open for hours: a system message says your mom called.
- Item-style tooltips with flavor lines and sizes priced in gold and silver.
- A pink plush platypus occasionally sits on the monitor.
- The About panel credits *Pure Pwnage* and links to the video.

## Performance budget

Enforced in CI where practical. Provisional until measured.

| Metric | Commit | Stretch |
|---|---|---|
| Windows exe | 12 MB or less | 9 MB |
| Windows zip | 8 MB or less | 5 MB |
| Linux tar.gz or AppImage | 10 MB or less | 7 MB |
| macOS universal dmg | 15 MB or less | 10 MB |
| Window visible, warm start | Under 400 ms | Under 250 ms |
| Idle RAM with catalog loaded | Under 120 MB working set | Under 70 MB |
| Idle CPU | 0 redraws, under 0.1% over 60 s | |
| Search across 20k names | Under 10 ms | Under 3 ms |
| Background processes when closed | None | |

## Legal lines

- Display name **WoW is a Feeling**, repo `wow-is-a-feeling`. The full phrase appears only as the song's title in About.
- "for World of Warcraft: Forever" is fine as a compatibility statement.
- Disclaimer in the app and README: not affiliated with or endorsed by Blizzard Entertainment.
- Never mirror addon files. Leatrix's author has filed DMCA notices against mirrors, and others have too.
- Touch only `Interface/AddOns`, our `.wiaf` staging folder and reads of `WTF`. Stay out of the game process.
