"""Shared album root, SQLite catalog, and EXIF helpers. Stdlib only."""

from __future__ import annotations

import argparse
import os
import sqlite3
import struct
import sys
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = """
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    relpath TEXT NOT NULL UNIQUE,
    filename TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    sha256 TEXT,
    captured_at TEXT,
    year INTEGER,
    lat REAL,
    lon REAL,
    camera TEXT,
    indexed_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_files_captured ON files(captured_at);
CREATE INDEX IF NOT EXISTS idx_files_year ON files(year);
CREATE INDEX IF NOT EXISTS idx_files_filename ON files(filename);
"""

MEDIA_SUFFIXES = {
    ".jpg",
    ".jpeg",
    ".png",
    ".heic",
    ".heif",
    ".webp",
    ".tif",
    ".tiff",
    ".gif",
    ".bmp",
    ".dng",
    ".cr2",
    ".cr3",
    ".nef",
    ".arw",
    ".raf",
    ".orf",
    ".rw2",
}


def default_root() -> Path:
    return Path(__file__).resolve().parent.parent


def add_root_arg(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--root",
        type=Path,
        default=default_root(),
        help="Album root (directory that contains photos/ and .album/)",
    )


def album_paths(root: Path) -> tuple[Path, Path, Path]:
    root = root.resolve()
    photos = root / "photos"
    album_dir = root / ".album"
    db = album_dir / "catalog.sqlite"
    return root, photos, db


def connect(db_path: Path, *, create: bool) -> sqlite3.Connection:
    if create:
        db_path.parent.mkdir(parents=True, exist_ok=True)
    elif not db_path.is_file():
        print(
            f"Catalog not found: {db_path}\nRun: python3 scripts/index.py",
            file=sys.stderr,
        )
        raise SystemExit(1)
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA foreign_keys=ON")
    if create:
        conn.executescript(SCHEMA)
        conn.commit()
    return conn


def is_media(path: Path) -> bool:
    return path.suffix.lower() in MEDIA_SUFFIXES


def walk_photos(photos_root: Path) -> list[Path]:
    if not photos_root.is_dir():
        return []
    files: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(photos_root):
        dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        base = Path(dirpath)
        for name in filenames:
            if name.startswith(".") or name == ".gitkeep":
                continue
            path = base / name
            if is_media(path):
                files.append(path)
    return files


def _read_ascii(buf: bytes) -> str:
    return buf.split(b"\x00", 1)[0].decode("utf-8", errors="replace").strip()


def _rational(buf: bytes, endian: str) -> float | None:
    if len(buf) < 8:
        return None
    num, den = struct.unpack(endian + "II", buf[:8])
    if den == 0:
        return None
    return num / den


def _gps_coord(values: bytes, ref: str | None, endian: str) -> float | None:
    if len(values) < 24:
        return None
    parts = []
    for i in range(3):
        r = _rational(values[i * 8 : (i + 1) * 8], endian)
        if r is None:
            return None
        parts.append(r)
    deg, minutes, seconds = parts
    coord = deg + minutes / 60.0 + seconds / 3600.0
    if ref in ("S", "W"):
        coord = -coord
    return coord


def _parse_tiff_exif(blob: bytes) -> dict:
    info: dict = {}
    if len(blob) < 8:
        return info
    endian_flag = blob[0:2]
    if endian_flag == b"MM":
        endian = ">"
    elif endian_flag == b"II":
        endian = "<"
    else:
        return info
    magic = struct.unpack(endian + "H", blob[2:4])[0]
    if magic != 42:
        return info

    def read_ifd(offset: int) -> dict[int, tuple[int, bytes]]:
        entries: dict[int, tuple[int, bytes]] = {}
        if offset < 0 or offset + 2 > len(blob):
            return entries
        count = struct.unpack(endian + "H", blob[offset : offset + 2])[0]
        pos = offset + 2
        type_size = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 7: 1, 9: 4, 10: 8}
        for _ in range(count):
            if pos + 12 > len(blob):
                break
            tag, typ, cnt = struct.unpack(endian + "HHI", blob[pos : pos + 8])
            val_off = blob[pos + 8 : pos + 12]
            pos += 12
            unit = type_size.get(typ, 1)
            nbytes = unit * cnt
            if nbytes <= 4:
                data = val_off[:nbytes]
            else:
                (off,) = struct.unpack(endian + "I", val_off)
                data = blob[off : off + nbytes]
            entries[tag] = (typ, data)
        return entries

    (ifd0_off,) = struct.unpack(endian + "I", blob[4:8])
    ifd0 = read_ifd(ifd0_off)

    make = model = None
    if 0x010F in ifd0:
        make = _read_ascii(ifd0[0x010F][1])
    if 0x0110 in ifd0:
        model = _read_ascii(ifd0[0x0110][1])
    camera_parts = [p for p in (make, model) if p]
    if camera_parts:
        info["camera"] = " ".join(camera_parts)

    exif = {}
    if 0x8769 in ifd0:
        (exif_off,) = struct.unpack(endian + "I", ifd0[0x8769][1].ljust(4, b"\x00")[:4])
        exif = read_ifd(exif_off)

    dt_raw = None
    for tag in (0x9003, 0x9004, 0x0132):
        src = exif if tag in exif else ifd0
        if tag in src:
            dt_raw = _read_ascii(src[tag][1])
            if dt_raw:
                break
    if dt_raw:
        info["datetime_original"] = dt_raw

    gps = {}
    if 0x8825 in ifd0:
        (gps_off,) = struct.unpack(endian + "I", ifd0[0x8825][1].ljust(4, b"\x00")[:4])
        gps = read_ifd(gps_off)
    lat_ref = _read_ascii(gps[1][1]) if 1 in gps else None
    lon_ref = _read_ascii(gps[3][1]) if 3 in gps else None
    if 2 in gps:
        lat = _gps_coord(gps[2][1], lat_ref, endian)
        if lat is not None:
            info["lat"] = lat
    if 4 in gps:
        lon = _gps_coord(gps[4][1], lon_ref, endian)
        if lon is not None:
            info["lon"] = lon
    return info


def _jpeg_exif_blob(data: bytes) -> bytes | None:
    if len(data) < 4 or data[0:2] != b"\xff\xd8":
        return None
    i = 2
    while i + 4 <= len(data):
        if data[i] != 0xFF:
            break
        marker = data[i + 1]
        i += 2
        if marker in (0xD8, 0xD9) or marker == 0x00:
            continue
        if marker >= 0xD0 and marker <= 0xD7:
            continue
        if marker == 0xDA:
            break
        if i + 2 > len(data):
            break
        (seglen,) = struct.unpack(">H", data[i : i + 2])
        if seglen < 2:
            break
        payload = data[i + 2 : i + seglen]
        i += seglen
        if marker == 0xE1 and payload.startswith(b"Exif\x00\x00"):
            return payload[6:]
    return None


def _heic_exif_blob(data: bytes) -> bytes | None:
    needle = b"Exif\x00\x00"
    idx = data.find(needle)
    if idx < 0:
        return None
    blob = data[idx + 6 :]
    if blob[:2] in (b"MM", b"II"):
        return blob
    return None


def read_exif(path: Path) -> dict:
    try:
        with path.open("rb") as fh:
            data = fh.read(256 * 1024)
    except OSError:
        return {}
    blob = _jpeg_exif_blob(data) or _heic_exif_blob(data)
    if not blob:
        return {}
    try:
        return _parse_tiff_exif(blob)
    except (struct.error, ValueError, IndexError):
        return {}


def captured_at_iso(exif: dict, mtime: float) -> tuple[str, int]:
    raw = exif.get("datetime_original")
    if isinstance(raw, str):
        for fmt in ("%Y:%m:%d %H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y:%m:%d"):
            try:
                dt = datetime.strptime(raw[:19], fmt)
                return dt.isoformat(timespec="seconds"), dt.year
            except ValueError:
                continue
    dt = datetime.fromtimestamp(mtime, tz=timezone.utc).astimezone()
    naive = dt.replace(tzinfo=None)
    return naive.isoformat(timespec="seconds"), naive.year
