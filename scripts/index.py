#!/usr/bin/env python3
"""Scan photos/ and upsert rows into .album/catalog.sqlite."""

from __future__ import annotations

import argparse
import hashlib
import sys
from datetime import datetime
from pathlib import Path

from catalog import (
    add_root_arg,
    album_paths,
    captured_at_iso,
    connect,
    read_exif,
    walk_photos,
)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def index_library(root: Path) -> int:
    _root, photos, db = album_paths(root)
    if not photos.is_dir():
        print(f"photos/ not found at {photos}", file=sys.stderr)
        return 1

    conn = connect(db, create=True)
    now = datetime.now().isoformat(timespec="seconds")
    seen: set[str] = set()
    added = skipped = updated = 0

    existing = {
        row["relpath"]: row
        for row in conn.execute("SELECT relpath, size, mtime FROM files")
    }

    for path in walk_photos(photos):
        relpath = path.relative_to(photos).as_posix()
        seen.add(relpath)
        stat = path.stat()
        prev = existing.get(relpath)
        if prev is not None and prev["size"] == stat.st_size and prev["mtime"] == stat.st_mtime:
            skipped += 1
            continue

        exif = read_exif(path)
        captured_at, year = captured_at_iso(exif, stat.st_mtime)
        camera = exif.get("camera")
        lat = exif.get("lat")
        lon = exif.get("lon")
        digest = sha256_file(path)

        conn.execute(
            """
            INSERT INTO files (
                relpath, filename, size, mtime, sha256,
                captured_at, year, lat, lon, camera, indexed_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(relpath) DO UPDATE SET
                filename=excluded.filename,
                size=excluded.size,
                mtime=excluded.mtime,
                sha256=excluded.sha256,
                captured_at=excluded.captured_at,
                year=excluded.year,
                lat=excluded.lat,
                lon=excluded.lon,
                camera=excluded.camera,
                indexed_at=excluded.indexed_at
            """,
            (
                relpath,
                path.name,
                stat.st_size,
                stat.st_mtime,
                digest,
                captured_at,
                year,
                lat,
                lon,
                camera,
                now,
            ),
        )
        if prev is None:
            added += 1
        else:
            updated += 1

    removed = 0
    for relpath in existing:
        if relpath not in seen:
            conn.execute("DELETE FROM files WHERE relpath = ?", (relpath,))
            removed += 1

    conn.commit()
    total = conn.execute("SELECT COUNT(*) FROM files").fetchone()[0]
    conn.close()
    print(
        f"indexed {total} files "
        f"(added {added}, updated {updated}, skipped {skipped}, removed {removed})"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Index photos/ into .album/catalog.sqlite")
    add_root_arg(parser)
    args = parser.parse_args()
    return index_library(args.root)


if __name__ == "__main__":
    raise SystemExit(main())
