use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

#[allow(dead_code)]
pub struct Track {
    pub id:           i64,
    pub path:         String,
    pub duration_secs: f64,
    pub size_bytes:   i64,
    pub frame_count:  i64,
    pub sample_rate:  i64,
}

#[allow(dead_code)]
pub struct TimelineEntry {
    pub position:          i64,
    pub track_id:          i64,
    pub path:              String,
    pub duration_secs:     f64,
    pub frame_count:       i64,
    pub sample_rate:       i64,
    pub virtual_start_secs: f64,
}

pub fn open(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database: {}", db_path.display()))?;
    conn.execute_batch("
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS tracks (
            id           INTEGER PRIMARY KEY,
            path         TEXT    NOT NULL UNIQUE,
            duration_secs REAL   NOT NULL,
            size_bytes   INTEGER NOT NULL,
            frame_count  INTEGER NOT NULL,
            sample_rate  INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS timeline (
            position           INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id           INTEGER NOT NULL REFERENCES tracks(id),
            virtual_start_secs REAL    NOT NULL
        );
    ")?;
    Ok(conn)
}

pub fn upsert_track(
    conn: &Connection,
    path: &str,
    duration_secs: f64,
    size_bytes: i64,
    frame_count: i64,
    sample_rate: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tracks (path, duration_secs, size_bytes, frame_count, sample_rate)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
             duration_secs = excluded.duration_secs,
             size_bytes    = excluded.size_bytes,
             frame_count   = excluded.frame_count,
             sample_rate   = excluded.sample_rate",
        params![path, duration_secs, size_bytes, frame_count, sample_rate],
    )?;
    Ok(())
}

pub fn count_tracks(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?)
}

pub fn list_tracks(conn: &Connection) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, duration_secs, size_bytes, frame_count, sample_rate
         FROM tracks ORDER BY path"
    )?;
    let tracks = stmt.query_map([], |r| Ok(Track {
        id:           r.get(0)?,
        path:         r.get(1)?,
        duration_secs: r.get(2)?,
        size_bytes:   r.get(3)?,
        frame_count:  r.get(4)?,
        sample_rate:  r.get(5)?,
    }))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tracks)
}

/// Remove tracks by ID. Also removes any future timeline entries referencing them.
/// Returns the number of tracks actually deleted.
pub fn remove_tracks(conn: &Connection, ids: &[i64]) -> Result<usize> {
    let mut removed = 0;
    for &id in ids {
        conn.execute("DELETE FROM timeline WHERE track_id = ?1 AND virtual_start_secs > unixepoch()", params![id])?;
        removed += conn.execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
    }
    Ok(removed)
}

/// Return the last virtual_start_secs + duration_secs covered in the timeline,
/// or `since` if timeline is empty.
fn timeline_end(conn: &Connection, since: f64) -> Result<f64> {
    let result: Option<f64> = conn.query_row(
        "SELECT t2.virtual_start_secs + tr.duration_secs
         FROM timeline t2
         JOIN tracks tr ON tr.id = t2.track_id
         ORDER BY t2.position DESC LIMIT 1",
        [],
        |r| r.get(0),
    ).optional()?;
    Ok(result.unwrap_or(since))
}

/// Extend the timeline so it covers at least `until_secs`, starting from
/// `since_secs` if the timeline is empty or ends before `since_secs`.
/// Picks tracks at random from the tracks table.
pub fn ensure_timeline_covers(conn: &Connection, since_secs: f64, until_secs: f64) -> Result<()> {
    let mut end = timeline_end(conn, since_secs)?;
    if end < since_secs { end = since_secs; }

    while end < until_secs {
        // Pick a random track
        let (track_id, duration): (i64, f64) = conn.query_row(
            "SELECT id, duration_secs FROM tracks ORDER BY RANDOM() LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let start = end;
        conn.execute(
            "INSERT INTO timeline (track_id, virtual_start_secs) VALUES (?1, ?2)",
            params![track_id, start],
        )?;
        end = start + duration;
    }
    Ok(())
}

/// Get the timeline entry that is playing at `unix_secs`.
/// Extends the timeline if needed.
pub fn get_entry_at(conn: &Connection, unix_secs: f64) -> Result<TimelineEntry> {
    ensure_timeline_covers(conn, unix_secs, unix_secs + 3600.0)?;
    let entry = conn.query_row(
        "SELECT tl.position, tl.track_id, tr.path, tr.duration_secs,
                tr.frame_count, tr.sample_rate, tl.virtual_start_secs
         FROM timeline tl
         JOIN tracks tr ON tr.id = tl.track_id
         WHERE tl.virtual_start_secs <= ?1
           AND tl.virtual_start_secs + tr.duration_secs > ?1
         ORDER BY tl.position DESC LIMIT 1",
        params![unix_secs],
        row_to_entry,
    )?;
    Ok(entry)
}

/// Get the timeline entry immediately after `position`.
pub fn get_entry_after(conn: &Connection, position: i64) -> Result<TimelineEntry> {
    let entry = conn.query_row(
        "SELECT tl.position, tl.track_id, tr.path, tr.duration_secs,
                tr.frame_count, tr.sample_rate, tl.virtual_start_secs
         FROM timeline tl
         JOIN tracks tr ON tr.id = tl.track_id
         WHERE tl.position > ?1
         ORDER BY tl.position ASC LIMIT 1",
        params![position],
        row_to_entry,
    )?;
    Ok(entry)
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<TimelineEntry> {
    Ok(TimelineEntry {
        position:           r.get(0)?,
        track_id:           r.get(1)?,
        path:               r.get(2)?,
        duration_secs:      r.get(3)?,
        frame_count:        r.get(4)?,
        sample_rate:        r.get(5)?,
        virtual_start_secs: r.get(6)?,
    })
}
