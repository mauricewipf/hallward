# Photo library

This workspace is a folder of photos plus a SQLite catalog. It is not a typical code repo. Load `skills/*/SKILL.md` when indexing or searching.

## Do

- Find photos with `python3 scripts/search.py` (see `skills/search-photos/SKILL.md`).
- Refresh the catalog with `python3 scripts/index.py` after files change (see `skills/index-photos/SKILL.md`).
- Return paths relative to `photos/`. Open results with the system viewer (`xdg-open`, `open`, …) when the user wants to look.
- Ask before moving, renaming, or deleting media.

## Do not

- Recursively list or glob `photos/` (`**/*.jpg`, etc.) to answer search questions.
- Invent SQL against `.album/catalog.sqlite`; use `scripts/search.py`.
- Commit files under `photos/` or `.album/*.sqlite*`.
- Install Python or packages unless the user explicitly agrees.

## Python 3

Index and search require `python3` on `PATH`. If it is missing:

1. Stop.
2. Tell the user that indexing/search needs Python 3.
3. Ask whether they want it installed.
4. Install only after they say yes.

Browsing `photos/` in a file manager does not need Python.

## Layout

- `photos/` — media in `YYYY/YYYY-MM-DD/` folders
- `.album/catalog.sqlite` — catalog (generated)
- `scripts/index.py`, `scripts/search.py`
- `skills/` — agent instructions (`SKILL.md`); not Cursor-specific
