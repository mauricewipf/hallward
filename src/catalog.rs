use std::path::{Path, PathBuf};
use std::time::Duration;

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
    let empty_catalog = db.metadata().is_ok_and(|meta| meta.len() == 0);
    if !create && (!db.exists() || empty_catalog) {
        anyhow::bail!(
            "no usable catalog at {} — run `hallward init`",
            db.display()
        );
    }
    // A previous failed initialization can leave a zero-byte database. Remove
    // it before SQLite opens it: SMB/CIFS servers may reject the lock SQLite
    // needs to initialize an existing empty file.
    if create && empty_catalog {
        std::fs::remove_file(&db).with_context(|| format!("remove empty {}", db.display()))?;
    }
    let conn = Connection::open(&db).with_context(|| format!("open {}", db.display()))?;
    // Use SQLite's default rollback journal rather than WAL. WAL relies on
    // shared-memory locking and is unreliable on SMB/CIFS mounts. The timeout
    // handles a short-lived lock from another Hallward process.
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
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

pub fn captured_at(conn: &Connection, relpath: &str) -> Result<Option<String>> {
    let row = conn
        .query_row(
            "SELECT captured_at FROM photos WHERE relpath = ?1",
            [relpath],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.flatten())
}

/// Atomically apply all catalog mutations after thumbnail generation.
pub fn apply_index_changes(
    conn: &mut Connection,
    keep: &[String],
    photos: &[Photo],
) -> Result<usize> {
    let tx = conn.transaction()?;
    let existing: Vec<String> = {
        let mut stmt = tx.prepare("SELECT relpath FROM photos")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let keep: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
    let mut removed = 0;
    for rel in existing {
        if !keep.contains(rel.as_str()) {
            tx.execute("DELETE FROM photos WHERE relpath = ?1", [&rel])?;
            removed += 1;
        }
    }
    for photo in photos {
        tx.execute(
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
    }
    tx.commit()?;
    Ok(removed)
}

pub fn upsert_photo(conn: &Connection, photo: &Photo) -> Result<()> {
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

pub fn delete_photo(conn: &Connection, relpath: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM photos WHERE relpath = ?1", [relpath])?)
}

pub fn delete_under_prefix(conn: &Connection, dir_rel: &str) -> Result<usize> {
    let escaped = dir_rel
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let like = format!("{escaped}/%");
    Ok(conn.execute(
        "DELETE FROM photos WHERE relpath = ?1 OR relpath LIKE ?2 ESCAPE '\\'",
        rusqlite::params![dir_rel, like],
    )?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_catalog_requires_initialization_and_init_recovers_it() {
        let dir = tempfile::tempdir().unwrap();
        let (album, db) = album_paths(dir.path());
        std::fs::create_dir_all(album).unwrap();
        std::fs::File::create(&db).unwrap();

        let err = open(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("no usable catalog"));

        drop(open(dir.path(), true).unwrap());
        assert!(db.metadata().unwrap().len() > 0);
    }

    #[test]
    fn delete_photo_and_prefix_do_not_overmatch() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path(), true).unwrap();
        for rel in ["Rome/a.jpg", "Rome/sub/b.jpg", "Rome-2/a.jpg", "Paris/c.jpg"] {
            upsert_photo(
                &conn,
                &Photo {
                    relpath: rel.into(),
                    album: album_relpath(rel),
                    filename: "f".into(),
                    mtime: 0,
                    size: 0,
                    captured_at: None,
                    camera: None,
                    width: None,
                    height: None,
                },
            )
            .unwrap();
        }
        assert_eq!(delete_photo(&conn, "Paris/c.jpg").unwrap(), 1);
        assert_eq!(count(&conn).unwrap(), 3);
        assert_eq!(delete_under_prefix(&conn, "Rome").unwrap(), 2);
        assert_eq!(count(&conn).unwrap(), 1);
        // "Rome-2/a.jpg" must survive a "Rome" prefix delete.
        let remaining = photos_in_album(&conn, "Rome-2").unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn delete_prefix_escapes_like_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path(), true).unwrap();
        for rel in ["a%b/c.jpg", "aXb/c.jpg"] {
            upsert_photo(
                &conn,
                &Photo {
                    relpath: rel.into(),
                    album: album_relpath(rel),
                    filename: "f".into(),
                    mtime: 0,
                    size: 0,
                    captured_at: None,
                    camera: None,
                    width: None,
                    height: None,
                },
            )
            .unwrap();
        }
        assert_eq!(delete_under_prefix(&conn, "a%b").unwrap(), 1);
        assert_eq!(count(&conn).unwrap(), 1);
    }
}
