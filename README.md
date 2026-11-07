<p align="center"><img src="assets/icon/png/icon-256.png" width="128" alt="WoW is a Feeling logo: a beige CRT monitor wearing a black bandana, with a stone W and a pink heart on screen"></p>

<h1 align="center">WoW is a Feeling</h1>

<p align="center"><b>A free, lightweight addon manager for World of Warcraft: Forever.</b><br>Browse, install and update addons without opening a website. Windows, macOS and Linux.</p>

<p align="center">
  <a href="https://github.com/robbiemueller/wow-is-a-feeling/releases/latest/download/wow-is-a-feeling-windows-x64.exe"><b>Download for Windows</b></a>
  &nbsp;|&nbsp;
  <a href="https://github.com/robbiemueller/wow-is-a-feeling/releases/latest/download/wow-is-a-feeling-macos-universal.dmg"><b>macOS</b></a>
  &nbsp;|&nbsp;
  <a href="https://github.com/robbiemueller/wow-is-a-feeling/releases/latest/download/wow-is-a-feeling-linux-x64.AppImage"><b>Linux</b></a>
</p>

<p align="center"><img src="site/assets/screenshot.png" width="780" alt="The app's main window: a list of addons grouped by category on the left, and a parchment page with details and an Install button on the right."></p>

It's one small program. There are no ads, no account and no Overwolf, and when you close it, nothing is left running. It looks like the quest log you remember.

## What it does

- **Browse without a browser.** Over 1,300 Forever addons, with their readmes, screenshots and changelogs, right in the app.
- **One click** to install, update, roll back or remove. Addons an addon needs come along with it.
- **Adopts what you have.** Addons you installed with another tool get picked up and kept updated.
- **Takes zips.** Drop a downloaded zip on the window and it installs the same safe way.
- **Looks after your settings.** It backs up your WTF folder before updating, and can restore it.
- **Updates itself, after asking.** You never have to come back here for a new version, and every update is signed.

## How small it is

| | |
|---|---|
| Download | 10 MB, one file, nothing to install (5 MB zipped) |
| Memory once idle | About 35 MB |
| Memory while starting up | About 100 MB at most |
| CPU while idle | Next to none. It draws nothing until something changes. |
| Window opens in | About 0.2 seconds |
| Searching 8,000 addons | Under 1 millisecond |
| Running after you close it | Nothing |

Measured on Windows 11 with the release build: memory with the real addon list, search with a made-up list of 8,000. For comparison, a popular Electron-based addon manager takes 289 MB on disk on the same machine.

## The first time you open it

This app is free and isn't paying for code signing yet, so your system will warn you once.

- **Windows:** if SmartScreen appears, choose **More info**, then **Run anyway**.
- **macOS:** open it once, then go to **System Settings**, **Privacy & Security**, and choose **Open Anyway**.
- **Linux:** make the AppImage executable, then run it. It finds installs under Wine, Lutris, Bottles, Proton and Faugus.

Each release publishes its checksums and build attestations, and you can read every line of the source here.

## What it connects to

| Site | Why |
|---|---|
| GitHub | The addon list, app updates and most addon downloads |
| WoWInterface | Addons published there, and their screenshots |
| foreverdb.net | Only if you install ForeverDB, which is published there |
| Imgur and Postimages | Screenshots addon authors keep there |

Nothing else, and it sends no telemetry. A link on an addon's page can't make the app contact any other site, so it can't carry a tracking pixel either.

## Why some addons are missing

Addons published only on CurseForge, Wago or Tukui can't be listed, because those sites don't allow it without approval. That includes Leatrix Plus, Bagnon, Auctionator, AtlasLoot and ElvUI. When you search for one, the app says so and suggests alternatives, and you can still drop a downloaded zip on the window.

Know of an addon that should be here? [Tell us](https://github.com/robbiemueller/wow-is-a-feeling/issues/new?template=add_addon.yml).

## How it keeps you safe

Addons are written by strangers, so the app treats them that way.

- Nothing it downloads is ever run. Addon code only runs inside the game, and programs and scripts found in a zip are left out.
- Zips are checked before anything is written: no paths outside the folder, no links, no zip bombs.
- An install either finishes or leaves your addons exactly as they were, even if your computer crashes halfway.
- App updates are checked against a signature built into the app before they're used.

## Platforms

| | Status |
|---|---|
| Windows 10 and 11 | Tested against a real WoW Forever install |
| Linux (Wine, Lutris, Bottles, Proton, Faugus) | Runs and passes all tests on Ubuntu. Finding a real install under Wine hasn't been tried yet. |
| macOS 12 or later | Runs and passes all tests in CI, but hasn't been tried on a real Mac with the game yet. |

If you try it on a Mac or under Wine, [let us know how it went](https://github.com/robbiemueller/wow-is-a-feeling/issues/new?template=bug_report.yml).

## Building it yourself

You need [Rust](https://rustup.rs). On Linux, also `libxkbcommon-dev`, `libgl1-mesa-dev` and `libwayland-dev`.

```bash
cargo build --release -p wow-is-a-feeling
```

| Folder | What's in it |
|---|---|
| `crates/app` | The interface |
| `crates/core` | Finding the game, installing, updating, and everything else that isn't the interface |
| `crates/catalog-builder` | Builds the addon list every 3 hours, in CI |
| `crates/xtask` | Release chores |
| `docs` | [How it's designed](docs/DESIGN.md) and [how it behaves](docs/UI-SPEC.md) |

## Credits

Named after the song "World of Warcraft is a Feeling" from [*Pure Pwnage*](https://www.youtube.com/watch?v=SpC3lZdk2HM). If you've seen it, you might go looking for a few things in here. Try `/help` in the search box.

All art is original. The fonts are Almendra, Marcellus and Fira Sans Condensed, under the SIL Open Font License.

World of Warcraft is a trademark of Blizzard Entertainment, Inc. This project is not affiliated with or endorsed by Blizzard Entertainment.

Free software under the [GPL-3.0](LICENSE).
