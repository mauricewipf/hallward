# Hallward

This is a **Rust TUI photo library** (`hallward`). Photos stay ordinary files on disk; `.album/` holds SQLite and thumbnails.

## Do

- Browse with `cargo run -- --root medialibrary` (after `hallward init` if `.album/` is missing).
- Index with `cargo run -- --root medialibrary init` or `index`. Video thumbs need `ffmpeg` on PATH; Live Photo detection uses `ffprobe`. On macOS, stills open in **Preview**; video playback needs **mpv** (`brew install mpv`); do not treat `ffplay` as the viewer.
- Verify Ask AI and image editing with `cargo test --test ask_ai`, which drives full requests against a stub OpenRouter HTTP transport. Extend that suite instead of testing by hand. Q&A and edits both call OpenRouter; the first request may prompt for an API key via the TUI overlay (`OPENROUTER_API_KEY` also works; legacy `GEMINI_API_KEY` is still read).
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
