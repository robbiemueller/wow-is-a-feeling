# WoW is a Feeling: UI behavior

How the app behaves. `DESIGN.md` covers the why and the engine. The skin (`skin.rs`, `theme.rs`, `widgets.rs`) already exists, and the look in `--demo` is the target.

## Idle means idle

- Never call `request_repaint()` unless a job is running or something is animating right now.
- Time-based things use `request_repaint_after(remaining)`, so the event loop sleeps until needed.
- All IO happens on worker threads. The UI thread only ever reads finished results from a channel. No file or network call in `ui()`.
- Worker threads are spawned per job and exit when done. Nothing polls.

## Startup

1. `--demo`: use the fixed demo catalog and a fake install. No network and no writes, so screenshots are deterministic.
2. Otherwise: init logging, install a panic hook that writes to the log, load `state.json`.
3. Take the single-instance lock (`config_dir()/lock` with `File::try_lock`). If it's held, show a small popup saying the app is already running, with a Close button.
4. Paint immediately. On workers: find the install (saved path first, validated, else `detect_all`), run `install::recover`, scan AddOns, load the cached catalog, fetch a fresh one with the saved ETag.
5. One second after the first frame, check for an app update if the setting allows it. Failures are silent, logged only.
6. If `eframe::run_native` fails because OpenGL isn't available, tell the player plainly (a native message box on Windows, stderr elsewhere) rather than vanishing.

Default tab: My Addons if anything is installed, otherwise Browse.

## Layout

The frame stays as it is in `--demo`: portrait, title, chrome buttons, toolbar, list at left, parchment at right, casting bar strip, tabs at the bottom, status text at bottom right. Resizable with a minimum of 820x560. Window size and position are saved in `state.settings` and restored when still on screen.

**Toolbar:** search box, sort dropdown (Browse only: Most popular, Recently updated, A to Z), and **Update all (n)**, disabled at zero.

**Status text, bottom right:** "Forever 1.60.1 | 18 addons | updated 2 hours ago", with the real game version, count and catalog age. Offline is stated plainly: "Offline, showing the last catalog".

## Lists

Row height stays constant so `show_rows` can virtualize. Rows are a flat `Vec<Row>` rebuilt only when inputs change (catalog, search text, sort, collapse state, install state), never every frame.

**Browse, no search text:** "Starter kits" (one row per kit), "Recommended" (the catalog's `featured`, open), then each category in `CATEGORIES` order, closed by default with a count. Collapse state lives in memory only.

**Browse, with search text:** a flat result list from `SearchIndex` with no headers, followed by matching `gaps` under a "Not available here" header. Search runs on every keystroke; it's under 10 ms. Among equal matches trusted entries come first, because an untrusted entry's downloads count for nothing in a search.

**My Addons:** "Updates available" (open when non-empty), "Installed", and "Other folders" for AddOns folders not managed by this app, grouped with `scan::group_unmanaged`. Search filters by name.

Row content: name colored by tier with a shadow, indented under its header, and at the right the version, or gold "update", or red "out of date". Installed addons in Browse show a small check or the word "installed" instead of the version. Hovering shows the item-style tooltip.

Keyboard: Up and Down move the selection and skip headers, Enter activates the main button, `/` or Ctrl+F focuses search, Escape clears search or closes a popup.

## The parchment page

**Catalog addon:**
- Name in the display face, "by author", the hand-drawn rule.
- Description (summary), then Details: Version, Updated, Size, Source (who it really comes from, "GitHub: owner/repo" or "WoWInterface", linking there), Folders, Requires (required addons by name, the same pick an install would make; click to select). One an install would leave out, because nobody has reviewed it, says "(install it yourself)". Display names aren't unique, so the repo is the only real signal.
- An entry the catalog doesn't mark `trusted` adds one note under Details: "New in the catalog. Nobody has reviewed it, so check who made it before you install."
- Then Screenshots (up to 3, decoded on a worker with a 5 MB and 4096 px limit, scaled to at most 640 px wide, textures freed when evicted), then About (README text, fetched on demand after a 300 ms pause on the selection, cached for the last 64 addons), then Changelog (collapsed). A README can run long, so the pictures come first.
- Fetch failures just leave those sections out and say so in small text. The page must stay usable offline.
- Buttons along the bottom, always visible: **Install** or **Update** or **Reinstall** (main), **Remove**, **Open page**, **Share**. **Roll back to x** appears when a previous version is kept. Two small checkboxes: "Use beta versions" and "Don't update this addon" (the pin).

**Other folder:** title and notes from its TOC, its folders, whether it targets Forever, and "This addon wasn't installed by this app." When a catalog match exists, say which entry and its source, and offer **Manage with this app**. Only a trusted entry can match, and a match by TOC id also needs the entry to list the folder. Managing takes only folders the disk ties together: one group here, or TOCs carrying the same id. Another folder the entry lists stays the player's, and the first update asks about it. Otherwise it just stays here. **Remove** always asks first.

**Kit:** its description, a list of its addons with their state, **Install all**.

**Gap:** "Not available here" with its reason, alternatives as clickable rows, **Open its page**, and a line saying a downloaded zip can be dropped on the window.

**Empty My Addons:** "Your quest log is empty." with a **Browse addons** button and the starter kits.

**No game found:** "Couldn't find WoW Forever." with what was checked, **Choose folder**, and **Look again**. Browse still works, but installing asks for a folder first.

## Installing

- Up to 3 operations run at once. Two operations whose folders overlap never run together.
- Before an install: `deps::resolve`. Required addons install first and get a yellow message. Only a trusted entry is picked, and downloads never rank a stranger over it. The pick installs by itself only when it's also reviewed (hand-picked), because any addon can ship a library's folder name long before anything needs it. A pick nobody reviewed stays out with a yellow line naming it and the page whose Requires line links to it, so the player can see who made it and install it themselves. A required folder nothing trusted provides gets a yellow warning telling the player to look for it themselves. Either way the install goes ahead.
- Before queuing a catalog install or update: compare its folders with the disk. See Replacing folders.
- Before an update batch: if `backup_wtf` is on and `should_backup` says so, back up WTF first. The casting bar shows it.
- If the game is running: a yellow message, not a block: "WoW is running. Type /reload in game to load updates. New addons appear after restarting the game." WTF restore is blocked while it's running.
- The casting bar shows one operation's name and fraction, or "Updating 3 of 7" and overall fraction. A small close button beside it cancels what hasn't finished.
- Success: update `state`, save, rescan, and put a line in the status strip: "You receive addon: Questie." Failure: red message with the error's text, and nothing else changes.
- Drag a zip onto the window to install it through the same safe path, tracked as `local:<main folder>` with no updates. A subtle overlay appears while hovering.
- `install::recover` results, if any, are told to the player: "An interrupted install was undone."

## Replacing folders

Who a folder belongs to goes by what's on disk and in the records, never by what a catalog entry claims. The last scan and the records are all it takes, so nothing here touches the disk from the UI thread. A request made before the first scan is back waits for it.

- **A conflict** is a folder the entry lists that's on disk, or in another addon's record, or about to arrive with another queued op, and isn't in this addon's own record. A folder new to the disk needs no question, and nor does a copy of the player's own they already let this addon replace when its zip didn't bring it.
- **One Install, Update or Reinstall click** with conflicts opens a popup first: which folders, who they belong to if known, the addon's source, **Replace them** and **Cancel**.
- **Update all, kits and pasted lists** never ask. An addon that would replace something stays out, with a yellow message saying to do it from its own page. The same goes for a required addon, and the addon that required it goes ahead.
- **Every catalog install and update** may replace this addon's record folders, including ones it shares, plus what the player agreed to. Anything else its zip brings that's already on disk is refused with a red message and nothing changes, which catches a zip that doesn't match the catalog. Folders a whole batch brings new to the disk may be shared within it. After a refusal the app reads AddOns again, so the next try asks instead of being refused the same way. A refused folder the catalog does list gets the words of a batch that left it out: it wasn't installed because it would replace that folder, so do it from its own page.
- **After saying yes,** a folder the zip brought is in the addon's record like any other, so the next update doesn't ask. One the zip didn't bring stays in the record if another addon claims it too, and that addon keeps its own record, a dropped zip's included. Otherwise it stays the player's, because a later version without it would delete a folder that was never replaced. The record remembers the yes, so later updates, Update all included, don't ask about it again. If a later zip does bring it, that update is refused, and the next one asks.
- **A dropped zip and a rollback** are the player's own choice, so they don't ask. A yellow message says which folders they replaced and how to get another addon's copy back.

Dominos ships `Masque_Dominos`, which is another entry's main folder. Installing one over the other asks once. After that both records claim the folder, updates of either replace it without a word, and removing either leaves it for the other.

## Removing

A popup confirms: "Remove Questie? Its folders are deleted. Your saved settings in WTF stay." It lists dependents when other installed addons require it.

Which folders a removal deletes, and which old folders an update or rollback drops, is worked out again when it starts, from the records then. One waiting behind an install that has come to share a folder leaves that folder to it.

## Popups

Vanilla-style static popups via `egui::Modal`: dark fill, gold border, centered near the top, yellow text, red buttons. One at a time; Escape cancels. The update prompt is one of these:

> **Version 0.2.0 is ready.** (You have 0.1.0.) What changed, scrollable. **Update now**, **Later**, **Skip this version**.

Update now shows progress on the casting bar, then relaunches. Failure is a red message and the app keeps running on the old version.

## Settings tab

Quest log layout again, with sections at left and a parchment page at right.

- **Game folder:** path, version, how it was found, **Choose folder** (a built-in folder browser, no native dialog dependency: path line, Up, drive list on Windows, subfolder list, validates with `detect::validate` as you browse), **Look again**. A failed write test explains the fix.
- **Backups:** toggle for automatic backups, **Back up now**, a list of backups with **Restore** (confirms, refused while the game runs), toggle for keeping previous addon versions.
- **Addon lists:** **Copy my list** to the clipboard. Paste a list and **Install these** shows what will be added first.
- **Display:** UI scale slider, 80% to 200%, using egui's zoom factor, saved.
- **App updates:** toggle for checking at launch, **Check now**, current version.
- **About:** the logo large, name, version, the credit below, links, and **Copy diagnostics**.

Credit text: "Named after the song 'World of Warcraft is a Feeling' from *Pure Pwnage*." with a **Watch the video** link. Then: "World of Warcraft is a trademark of Blizzard Entertainment, Inc. This project is not affiliated with or endorsed by Blizzard Entertainment." Then the GPL-3.0 note, fonts under the SIL Open Font License, and a link to the source.

Diagnostics text: app version, OS, install path and how found, game version, counts, and the last 50 log lines.

## Messages

At top center over the frame, or just below a popup when one is open: at most 3, red for errors and yellow for system messages, with a shadow. They hold for 5 seconds and fade over 1. While one is alive, schedule repaints only for the fade.

## Tooltips

Item style via `visuals.window_*`: title in tier color, "Binds when installed", "Requires Interface 16001", "Size: 93 gold 60 silver", green "Use: <summary>", yellow flavor line from `easter::flavor_line` in quotes. Delay about 250 ms. Keep them narrow.

## Easter eggs

From `easter.rs`, all opt-in and inert otherwise.

- Search box: Enter on a line starting with `/` runs `parse_slash` and clears the box. Don't treat a leading `/` as a search while typing.
  - `Slash::Say`: yellow message.
  - `Reload`: rescan.
  - `Dance`: portrait bobs for 3 seconds.
  - `Ding`: gold flash across the frame, about 1 second, and a yellow "Ding!".
  - `Pwnage`: the word PWNAGE in big block letters over the frame for 3 seconds, with **Watch the video** in a yellow message.
  - `OpenAbout`: go to Settings, About.
- Portrait clicks feed `MonitorGag`: when off, draw `logo_off` and show `CLEANING_MESSAGE`. Wake with `wake_in`.
- `MOM_MESSAGE` after `MOM_CALLS_AFTER` open, once, scheduled with `request_repaint_after`.
- `platypus_visits(launches)`: draw `PLATYPUS` at 2x sitting on the portrait. Hovering shows a tooltip: "A pink plush platypus. It has seen things." Colors: p pink, k black, o orange bill, w white.
- Update `stats.seconds_open` when closing.

## Custom cursor

`assets/skin/cursor.png` via `ctx.set_cursor_image`, hot spot (1,1), scaled 2x with nearest-neighbor when `pixels_per_point` is 1.5 or more. Set it only when egui's cursor icon is Default or PointingHand; otherwise clear it so text and resize cursors still work. Reuse one `Arc` per scale so nothing is re-uploaded.

## Accessibility

Every custom widget reports `widget_info`. AccessKit stays on. Color is never the only signal: updates say "update", out of date says "out of date".
