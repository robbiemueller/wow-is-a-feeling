# Changelog

The section for each release becomes the notes the app shows before it updates itself, so write for players.

## 0.1.3

- Spoken's page now lists its voice packs and says which ones you have. Installing Spoken from its page asks whether to get the quest voices it needs too, and says how big they are.
- Sizes of 1 GB and up now read in GB.
- When a catalog entry starts pointing at a different addon, your copy is no longer updated into it. Its page says what happened and how to switch. An addon whose folder was renamed keeps updating when the catalog knows it's the same addon.
- A pasted or shared addon list now shows each addon's download size, what would come along or be updated with them, and what it all adds up to. It also says which addons it would leave alone and what won't come along, and why.
- An addon you've set not to update is no longer updated when another addon you install or update needs its newer version. The app tells you instead.
- Very big downloads, such as voice packs, are no longer kept for rolling back, so they don't push out the copies other addons keep. Copies the app kept before this update still work, and an addon that has just grown that big can still go back to its last smaller version.

## 0.1.2

- When WoWInterface is holding an addon's newest upload until it's approved, which blocks every download of that addon, the app now says so and tells you to try again later. It used to call the download corrupt.
- A failed install or update now says which addon it was, and the reason stays on that addon's page until you try again, instead of fading after a few seconds.

## 0.1.1

- The update prompt now says a version is available, since nothing downloads until you choose Update now.
- More kinds of program and script files are left out of addon zips, such as AutoHotkey and Perl scripts.

## 0.1.0

The first release.

- Browse over 1,300 WoW Forever addons, with readmes, screenshots and changelogs, without opening a website.
- Install, update and roll back with one click. Removing asks you first. Addons an addon needs usually come along with it, and the app tells you about any it can't add.
- Addons you installed with another tool are recognized, and one click lets the app keep them updated.
- Drop a downloaded zip on the window to install it the same safe way.
- Your WTF settings get backed up before updates, once a day, and you can restore them.
- The app asks before it updates itself, and checks every update's signature.
- It finds WoW Forever on its own. The macOS and Linux builds pass every test, but haven't been tried with the game yet, so tell us how they go.
