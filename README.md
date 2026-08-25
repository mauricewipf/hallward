# Hallward

Terminal photo library: miller-style folders, a thumbnail grid for albums, and an **external** image viewer. Photos stay ordinary files on disk. Hallward only adds a `.album/` directory (SQLite catalog + JPEG thumbs).

v1 indexes **still images** only. Videos (including Live Photo `.MOV` companions) stay on disk and are ignored.

## Prerequisites

- **Ghostty**, **Kitty** or another graphic-supporting terminal. Other terminals fall back to coarse unicode blocks.
- `libheif` / `heif-convert` for HEIC thumbnails (Arch: `pacman -S libheif`)
- An image viewer: **imv** (preferred), or nsxiv, feh, swayimg

## Use

Point Hallward at a folder of albums (collections are folders that only contain other folders):

```text
~/Pictures/Library/
  .album/               # generated on init (catalog + thumbs)
  2025/                 # collection
    Rome/               # album (images)
  Samples/              # album
```

First run in that folder:

```bash
cd ~/Pictures/Library
hallward
```

You will be asked to initialize. That creates `.album/` and builds thumbs (HEIC decode can take a minute).

```bash
hallward init          # create .album/ and index
hallward index         # refresh after adding/removing files
hallward               # open the TUI
hallward --root PATH   # library is PATH instead of cwd
```

### Keyboard

| Key | Action |
|-----|--------|
| arrows | Miller columns; in an album grid, move the selection frame |
| Right on a collection | Open its subfolders |
| Right on an album | Focus the thumbnail grid |
| Left from the left edge of the grid | Back to the album column |
| letters / digits | Filter collection and album **names** (not filenames) |
| Tab | Jump to the filtered tree |
| Esc | Clear search and show the full tree |
| Enter | Open the **whole album** in the external viewer (imv starts at the selected photo; swayimg starts at the first) |
| r | Re-scan files and refresh thumbnails (when search is closed) |
| q | Quit (when search is closed) |

EXIF for the highlighted still is in the bottom-left pane.

## Install

From source (needs [Rust](https://rustup.rs/)):

```bash
git clone <this-repo>
cd hallward
cargo install --path .
```

Or without installing:

```bash
cargo build --release
# binary: target/release/hallward
```

## Dev (this repo)

```bash
cargo test
cargo run -- --root medialibrary init
cargo run -- --root medialibrary
```
