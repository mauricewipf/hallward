#!/usr/bin/env python3
"""Query .album/catalog.sqlite and print TSV paths for an agent or human."""

from __future__ import annotations

import argparse
import sys
from datetime import datetime

from catalog import add_root_arg, album_paths, connect


def parse_day(value: str, *, end: bool) -> str:
    try:
        dt = datetime.strptime(value, "%Y-%m-%d")
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected YYYY-MM-DD, got {value!r}") from None
    if end:
        return dt.strftime("%Y-%m-%d") + "T23:59:59"
    return dt.strftime("%Y-%m-%d") + "T00:00:00"


def main() -> int:
    parser = argparse.ArgumentParser(description="Search the photo catalog")
    add_root_arg(parser)
    parser.add_argument("--from", dest="from_date", metavar="YYYY-MM-DD")
    parser.add_argument("--until", dest="until_date", metavar="YYYY-MM-DD")
    parser.add_argument("--year", type=int)
    parser.add_argument("--text", help="Substring match on relative path or filename")
    parser.add_argument("--limit", type=int, default=50)
    args = parser.parse_args()

    _root, _photos, db = album_paths(args.root)
    conn = connect(db, create=False)

    clauses = ["1=1"]
    params: list[object] = []

    if args.from_date:
        clauses.append("captured_at >= ?")
        params.append(parse_day(args.from_date, end=False))
    if args.until_date:
        clauses.append("captured_at <= ?")
        params.append(parse_day(args.until_date, end=True))
    if args.year is not None:
        clauses.append("year = ?")
        params.append(args.year)
    if args.text:
        like = f"%{args.text}%"
        clauses.append("(relpath LIKE ? COLLATE NOCASE OR filename LIKE ? COLLATE NOCASE)")
        params.extend([like, like])

    sql = (
        "SELECT relpath, captured_at, filename FROM files WHERE "
        + " AND ".join(clauses)
        + " ORDER BY captured_at ASC, relpath ASC"
    )
    if args.limit is not None:
        if args.limit < 0:
            parser.error("--limit must be >= 0")
        sql += " LIMIT ?"
        params.append(args.limit)

    rows = conn.execute(sql, params).fetchall()
    conn.close()

    print("relpath\tcaptured_at\tfilename")
    for row in rows:
        print(f"{row['relpath']}\t{row['captured_at'] or ''}\t{row['filename']}")
    sys.stdout.flush()
    print(f"# {len(rows)} result(s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
