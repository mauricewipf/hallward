---
name: search-photos
description: Search this folder photo library by date, year, or filename using scripts/search.py. Use when the user wants to find, list, or locate photos or asks in natural language which pictures match a time, folder, or name.
---

# Search photos

## Python 3

This skill needs `python3`. If `python3` is not on `PATH`, stop, tell the user indexing/search needs Python 3, and ask whether they want it installed. Do not install Python or packages unless they explicitly say yes.

## Instructions

1. Do not glob or recursively list `photos/` to answer the question.
2. Translate the request into `python3 scripts/search.py` flags. Run from the album root (the directory that contains `photos/` and `scripts/`).
3. Read TSV stdout (`relpath`, `captured_at`, `filename`). Paths are relative to `photos/`.
4. Reply with those paths. Open files with the system viewer only if the user wants to look at them.

## Command

```bash
python3 scripts/search.py [--from YYYY-MM-DD] [--until YYYY-MM-DD] [--year YYYY] [--text SUBSTRING] [--limit N]
```

`--from` / `--until` are inclusive dates. `--text` matches relative path or filename. Default `--limit` is 50.

## Examples

User: "photos from 2024"

```bash
python3 scripts/search.py --year 2024
```

User: "pictures between June 2020 and December 2024 named IMG"

```bash
python3 scripts/search.py --from 2020-06-01 --until 2024-12-31 --text IMG
```

If the catalog is missing, tell the user to run `python3 scripts/index.py` (or follow `skills/index-photos/SKILL.md`).
