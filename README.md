# Photo library

This folder is a **file-manager photo library**: pictures live in `photos/` as ordinary files. A SQLite catalog in `.album/` makes agent search fast. There is no gallery app and no tool to install.

## Workflows

1. **Browse** — open `photos/` in any file manager and view files with the system image/video app.
2. **Ask an agent** — open this folder as the workspace and describe what you want. The agent should run `scripts/search.py`, not walk every image.

## Layout

- `photos/` — dated folders of media (`YYYY/YYYY-MM-DD/…`)
- `.album/catalog.sqlite` — generated index (not committed)
- `scripts/index.py` / `scripts/search.py` — catalog tools (Python 3 stdlib)
- `skills/` — portable agent skills (`index-photos`, `search-photos`)
- `AGENTS.md` — rules for any coding agent

## Python

Indexing and search need **Python 3** on `PATH` (`python3`). Browsing files does not.

If `python3` is missing, **ask the user whether to install it**. Do not install Python or packages unless they say yes.

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
