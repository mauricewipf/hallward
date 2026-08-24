use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::library::album_paths;
use crate::meta::PhotoMeta;

#[derive(Debug, Clone)]
pub struct Photo {
    pub relpath: String,
    pub album: String,
    pub filename: String,
    pub mtime: i64,
    pub size: i64,
    pub captured_at: Option<String>,
    pub camera: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

pub fn open(root: &Path, create: bool) -> Result<Connection> {
    let (album, db) = album_paths(root);
    if create {
        std::fs::create_dir_all(&album).with_context(|| format!("create {}", album.display()))?;
        std::fs::create_dir_all(album.join("thumbs"))?;
    }
    if !create && !db.exists() {
        anyhow::bail!("no catalog at {} — run `hallward init`", db.display());
    }
    let conn = Connection::open(&db).with_context(|| format!("open {}", db.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA synchronous=NORMAL;",
    )?;
    if create {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS photos (
                relpath TEXT PRIMARY KEY,
                album TEXT NOT NULL,
                filename TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL,
                captured_at TEXT,
                camera TEXT,
                width INTEGER,
                height INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_photos_album ON photos(album);
            CREATE INDEX IF NOT EXISTS idx_photos_captured ON photos(captured_at);",
        )?;
    }
    Ok(conn)
}

pub fn get_mtime_size(conn: &Connection, relpath: &str) -> Result<Option<(i64, i64)>> {
    let row = conn
        .query_row(
            "SELECT mtime, size FROM photos WHERE relpath = ?1",
            [relpath],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row)
}

pub fn upsert(conn: &Connection, photo: &Photo) -> Result<()> {
    conn.execute(
        "INSERT INTO photos (relpath, album, filename, mtime, size, captured_at, camera, width, height)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(relpath) DO UPDATE SET
            album=excluded.album,
            filename=excluded.filename,
            mtime=excluded.mtime,
            size=excluded.size,
            captured_at=excluded.captured_at,
            camera=excluded.camera,
            width=excluded.width,
            height=excluded.height",
        params![
            photo.relpath,
            photo.album,
            photo.filename,
            photo.mtime,
            photo.size,
            photo.captured_at,
            photo.camera,
            photo.width,
            photo.height,
        ],
    )?;
    Ok(())
}

pub fn delete_missing(conn: &Connection, keep: &[String]) -> Result<usize> {
    let existing: Vec<String> = {
        let mut stmt = conn.prepare("SELECT relpath FROM photos")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let keep: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
    let mut n = 0;
    for rel in existing {
        if !keep.contains(rel.as_str()) {
            conn.execute("DELETE FROM photos WHERE relpath = ?1", [&rel])?;
            n += 1;
        }
    }
    Ok(n)
}

pub fn photos_in_album(conn: &Connection, album: &str) -> Result<Vec<Photo>> {
    let mut stmt = conn.prepare(
        "SELECT relpath, album, filename, mtime, size, captured_at, camera, width, height
         FROM photos WHERE album = ?1
         ORDER BY COALESCE(captured_at, '') ASC, filename ASC",
    )?;
    let rows = stmt.query_map([album], |r| {
        Ok(Photo {
            relpath: r.get(0)?,
            album: r.get(1)?,
            filename: r.get(2)?,
            mtime: r.get(3)?,
            size: r.get(4)?,
            captured_at: r.get(5)?,
            camera: r.get(6)?,
            width: r.get(7)?,
            height: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?)
}

#[allow(dead_code)]
pub fn count_in_album(conn: &Connection, album: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE album = ?1",
        [album],
        |r| r.get(0),
    )?)
}

pub fn album_relpath(relpath: &str) -> String {
    Path::new(relpath)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into())
}

pub fn photo_from_file(
    relpath: String,
    filename: String,
    album: String,
    mtime: i64,
    size: i64,
    meta: PhotoMeta,
) -> Photo {
    Photo {
        relpath,
        album,
        filename,
        mtime,
        size,
        captured_at: meta.captured_at,
        camera: meta.camera,
        width: meta.width.map(|w| w as i64),
        height: meta.height.map(|h| h as i64),
    }
}

#[allow(dead_code)]
pub fn abs_path(root: &Path, relpath: &str) -> PathBuf {
    root.join(relpath)
}
