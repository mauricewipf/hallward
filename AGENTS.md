# Hallward

This is a **Rust TUI photo library** (`hallward`). Photos stay ordinary files on disk; `.album/` holds SQLite and thumbnails.

## Do

- Browse with `cargo run -- --root medialibrary` (after `hallward init` if `.album/` is missing).
- Index with `cargo run -- --root medialibrary init` or `index`. Video thumbs need `ffmpeg` on PATH; Live Photo detection uses `ffprobe`.
- Keep media as normal files under the library root. Never commit `medialibrary/` or `.album/`.
- Ask before moving, renaming, or deleting media.
- Shipping a version: use the `release` skill (`.cursor/skills/release/`). Do not tag by hand.

## Do not

- Recursively glob `medialibrary/` to answer “what’s in the library”; use the catalog (`.album/catalog.sqlite`) or `hallward index`.
- Index iPhone Live Photo `.MOV` companions (they are ignored; only the HEIC still is cataloged).
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
