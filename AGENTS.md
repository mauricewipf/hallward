# Hallward

This is a **Rust TUI photo library** (`hallward`). Photos stay ordinary files on disk; `.album/` holds SQLite and thumbnails.

## Do

- Browse with `cargo run -- --root photos` (after `hallward init` if `.album/` is missing).
- Index with `cargo run -- --root photos init` or `index`.
- Keep media as normal files under the library root. Never commit `photos/` or `.album/`.
- Ask before moving, renaming, or deleting media.

## Do not

- Recursively glob `photos/` to answer “what’s in the library”; use the catalog (`.album/catalog.sqlite`) or `hallward index`.
- Index videos in v1 (`.mov` / `.mp4` stay on disk, ignored).
- Add Python catalog scripts back.

## Layout

- `src/` — Rust crate (`hallward` binary)
- `photos/` — local test library (gitignored)
- `.album/` — generated catalog + thumbs (inside the library root)

## Commands

```bash
cargo test
cargo run -- --root photos init
cargo run -- --root photos
```
