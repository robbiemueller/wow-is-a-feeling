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
- Big zips: Questie and OpenMap are around 90 MB.
- `/releases/latest` can point at another flavor (VuhDo's latest is a vanilla build), so scan recent releases.
- Tags with slashes and `+` (`bundle/v12.6.2+v1.0.11`).
- Same-named repos that are different projects (`ForeverPlus`, `ForeverUI`). Link sources by TOC IDs, never by name alone.
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
| `extract` | Safe zip extraction: no traversal, no bombs, no executables |
| `install` | Staging, journaled atomic swap, rollback, crash recovery |
| `backup` | WTF settings backup and restore |
| `selfupdate` | Signed app updates through GitHub Releases |
| `details` | On-demand README and changelog text for the parchment panel |

### Install transaction

Staging lives in `<_forever_>/Interface/.wiaf/`, which is on the same volume as `AddOns` and outside it, so the game never scans it.

1. Download to `.wiaf/downloads/<txn>.zip` and verify size and digest when known.
2. Extract into `.wiaf/staging/<txn>/` through the safe extractor.
3. Require at least one folder with a TOC. Warn, don't fail, when no folder targets Forever.
4. Write `.wiaf/journal.json` listing every folder to swap, flushed to disk.
5. For each folder: rename the old one into `.wiaf/backup/<txn>/`, then rename the new one into `AddOns`. Retry renames with backoff on transient locks from antivirus and cloud-sync tools.
6. Mark the journal committed, delete the backup and staging, delete the journal.
7. On any failure, undo in reverse. On startup, a leftover uncommitted journal rolls back; a committed one finishes cleanup.

Folders we own are recorded in `state`. Removal deletes only folders no other managed addon claims. Unmanaged folders are never touched, except when a new install contains a folder of the same name, which goes through the same backup-and-swap and is logged. Two addons may claim one folder (shared libraries).

### Safe extraction

Never call `ZipArchive::extract`. Hand-roll it on top of `zip`:

- Use `enclosed_name()`; reject symlinks, `:`, Windows device names, trailing dots or spaces, and case-only duplicates.
- Never trust declared sizes. Wrap readers in `take(limit)`, with per-file, per-archive, entry-count and ratio caps.
- Skip junk (`__MACOSX`, `.git`, `.github`, `.DS_Store`, `Thumbs.db`).
- Never write executables or scripts (`exe dll scr com msi bat cmd ps1 sh jar vbs app dylib so`). Skip them, log them, and report them in the result. Dev scripts appear in real source archives, so skipping beats failing.
- Only keep top-level folders that contain a TOC.

### Self-update

Checked once at launch, in the background, after the first frame. Never silent: the player sees the version and notes and picks **Update now**, **Later** or **Skip this version**.

- `https://github.com/robbiemueller/wow-is-a-feeling/releases/latest/download/latest.json` plus `latest.json.minisig`. That URL is a plain redirect, so it doesn't touch GitHub's REST rate limit.
- Verify the signature over `latest.json` with the public key embedded in the app, using `minisign-verify`, then compare versions, download the asset, and check its SHA-256 from the signed file.
- Windows and Linux tarball: swap with `self-replace`. AppImage: replace `$APPIMAGE`. macOS: replace the whole `.app`.

`latest.json`:

```json
{
  "version": "0.2.0",
  "notes": "What changed, in plain words.",
  "date": 1794000000,
  "assets": {
    "windows-x64": { "url": "...", "sha256": "...", "size": 123, "kind": "exe" },
    "linux-x64-appimage": { "url": "...", "sha256": "...", "size": 123, "kind": "appimage" },
    "linux-x64-tar": { "url": "...", "sha256": "...", "size": 123, "kind": "tar.gz" },
    "macos-universal": { "url": "...", "sha256": "...", "size": 123, "kind": "app.tar.gz" }
  }
}
```

## Catalog

`crates/core/src/catalog.rs` is the schema. CI builds one `catalog.json` and force-pushes it to the orphan `catalog` branch, so clones stay small. The app fetches it from `raw.githubusercontent.com` with `If-None-Match`, caches it, and parses it on a worker thread.

Update checks compare installed versions against the catalog: one request, not one per addon.

### Sources

- **GitHub** is the backbone. Seeds: the public-domain strongbox catalogue, topic and code search with a token, and `catalog/seeds.toml`. Detection order per release, newest first, because each step exists for a real addon:
  1. `release.json` metadata with flavor `forever`/`camelot` or a 16xxx interface.
  2. A zip asset whose name contains `forever` or `camelot`.
  3. Read the TOC inside candidate zips with HTTP range requests.
- **WoWInterface** has no Forever tag in its metadata, so scan zips: filter `api.mmoui.com/v3/game/WOW/filelist.json` to recently updated addons, download each zip once per version, and read its TOCs. Cache by id and version. Go slowly (one at a time), send an honest User-Agent with the project URL, and never mirror anything. The app downloads straight from their CDN, so their download counts stay intact.
- **Curated source installs** (`catalog/curated.toml`): addons whose authors publish tags but no packaged zips, such as ElvUI and Details. The app installs GitHub's source archive using `Release.moves`.
- **Not used, on purpose:** CurseForge and Wago (their terms need approval or a key), the Tukui API (asks for staff permission; ElvUI comes from its public GitHub tags instead), and any scraping of websites. Well-known gaps appear in `Catalog.gaps` with alternatives.

Merge entries from different sources by TOC IDs (`X-WoWI-ID`) or identical folder sets, never by name alone. Prefer GitHub when both exist.

Tier is a popularity color only: top 10 legendary, next 5% epic, next 15% rare, next 30% uncommon, rest common. Grey is reserved for addons that no longer target Forever.

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
