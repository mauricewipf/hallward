---
name: index-photos
description: Build or refresh the SQLite photo catalog by running scripts/index.py. Use when the user adds, removes, or moves photos, says the library is out of date, or search reports a missing catalog.
---

# Index photos

## Python 3

This skill needs `python3`. If `python3` is not on `PATH`, stop, tell the user indexing/search needs Python 3, and ask whether they want it installed. Do not install Python or packages unless they explicitly say yes.

## Instructions

1. Run from the album root (the directory that contains `photos/` and `scripts/`):

```bash
python3 scripts/index.py
```

2. The script walks `photos/`, writes `.album/catalog.sqlite`, skips files whose size and mtime match, and deletes rows for files that are gone.
3. Do not invent a one-off scanner or write ad-hoc SQL to rebuild the catalog.
4. Do not move or rename media while indexing unless the user asked.

After a successful index, use `skills/search-photos/SKILL.md` to query.
