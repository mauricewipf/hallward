# Photo library

This folder is a **file-manager photo library**: pictures live in `photos/` as ordinary files. A SQLite catalog in `.album/` makes agent search fast. There is no gallery app and no tool to install.

It is **agent-agnostic**. Point any coding agent at this folder — Claude, Cursor, Codex, Gemini, or similar — and ask it to find photos. Instructions in `AGENTS.md` and `skills/` are portable; they are not tied to one product.

## Prerequisites

- **Python 3** on `PATH` (`python3`) — needed to index and search. Browsing files does not require it.
- **Any agent** — Claude, Cursor, Codex, Gemini, or similar. Instructions in `AGENTS.md` and `skills/` are portable.

## How to start

1. **Add photos** — put pictures in `photos/`. Each folder is an album.
2. **Index** — open this project folder as your agent’s workspace and ask it to index the library.
3. **Search** — ask the agent to find photos.
4. **Browse** — open `photos/` in a file manager and view files as ordinary files; no AI required.

## Layout

- `photos/` — dated folders of media (`YYYY/YYYY-MM-DD/…`)
- `.album/catalog.sqlite` — generated index (not committed)
- `scripts/index.py` / `scripts/search.py` — catalog tools (Python 3 stdlib)
- `skills/` — portable agent skills (`index-photos`, `search-photos`)
- `AGENTS.md` — rules for any coding agent

## Index

```bash
python3 scripts/index.py
```

Re-run after adding, removing, or moving files under `photos/`. Unchanged files are skipped.

## Search

```bash
python3 scripts/search.py --year 2024
python3 scripts/search.py --from 2020-01-01 --until 2024-12-31
python3 scripts/search.py --text IMG_ --limit 20
```

Output is TSV: `relpath`, `captured_at`, `filename`. Paths are relative to `photos/`.

## Who is Hallward?

Basil Hallward is the painter in Oscar Wilde’s *The Picture of Dorian Gray*. He makes Dorian’s portrait and then refuses to exhibit it — he wants the picture kept, not hung in a gallery. This library is named after him: photos stay as ordinary files, and you ask for them rather than putting them on display.
