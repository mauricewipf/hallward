# Hallward

This is a **Rust TUI photo library** (`hallward`). Photos stay ordinary files on disk; `.album/` holds SQLite and thumbnails.

## Do

- Browse with `cargo run -- --root medialibrary` (after `hallward init` if `.album/` is missing).
- Index with `cargo run -- --root medialibrary init` or `index`.
- Keep media as normal files under the library root. Never commit `medialibrary/` or `.album/`.
- Ask before moving, renaming, or deleting media.

## Do not

- Recursively glob `medialibrary/` to answer “what’s in the library”; use the catalog (`.album/catalog.sqlite`) or `hallward index`.
- Index videos in v1 (`.mov` / `.mp4` stay on disk, ignored).
- Add Python catalog scripts back.

## Layout

- `src/` — Rust crate (`hallward` binary)
- `medialibrary/` — local test library (gitignored)
- `.album/` — generated catalog + thumbs (inside the library root)

## Commands

```bash
cargo test
cargo run -- --root medialibrary init
cargo run -- --root medialibrary
```
