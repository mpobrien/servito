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
pub struct Channel {
    pub id:   i64,
    pub name: String,
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
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS tracks (
            id           INTEGER PRIMARY KEY,
            path         TEXT    NOT NULL UNIQUE,
            duration_secs REAL   NOT NULL,
            size_bytes   INTEGER NOT NULL,
            frame_count  INTEGER NOT NULL,
            sample_rate  INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS channels (
            id   INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS channel_tracks (
            channel_id INTEGER NOT NULL REFERENCES channels(id),
            track_id   INTEGER NOT NULL REFERENCES tracks(id),
            PRIMARY KEY (channel_id, track_id)
        );
    ")?;

    // Pre-multi-channel installs have a `timeline` table with no channel_id
    // column. Timeline rows are just a rolling schedule that gets
    // regenerated on demand, so it's safe to drop and rebuild rather than
    // migrate the data in place.
    let has_channel_id: bool = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('timeline') WHERE name = 'channel_id'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0;
    if !has_channel_id {
        conn.execute_batch("DROP TABLE IF EXISTS timeline;")?;
    }

    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS timeline (
            position           INTEGER PRIMARY KEY AUTOINCREMENT,
            channel_id         INTEGER NOT NULL REFERENCES channels(id),
            track_id           INTEGER NOT NULL REFERENCES tracks(id),
            virtual_start_secs REAL    NOT NULL
        );
    ")?;
    Ok(())
}

pub fn upsert_track(
    conn: &Connection,
    path: &str,
    duration_secs: f64,
    size_bytes: i64,
    frame_count: i64,
    sample_rate: i64,
) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO tracks (path, duration_secs, size_bytes, frame_count, sample_rate)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
             duration_secs = excluded.duration_secs,
             size_bytes    = excluded.size_bytes,
             frame_count   = excluded.frame_count,
             sample_rate   = excluded.sample_rate
         RETURNING id",
        params![path, duration_secs, size_bytes, frame_count, sample_rate],
        |r| r.get(0),
    )?;
    Ok(id)
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

/// Remove tracks by ID. Also removes any timeline entries and channel
/// memberships referencing them (past timeline rows are never read again —
/// only `history`, kept separately in memory, drives "recently played").
/// Returns the number of tracks actually deleted.
pub fn remove_tracks(conn: &Connection, ids: &[i64]) -> Result<usize> {
    let mut removed = 0;
    for &id in ids {
        conn.execute("DELETE FROM timeline WHERE track_id = ?1", params![id])?;
        conn.execute("DELETE FROM channel_tracks WHERE track_id = ?1", params![id])?;
        removed += conn.execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
    }
    Ok(removed)
}

/// Get or create a channel by name, returning its id.
pub fn upsert_channel(conn: &Connection, name: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO channels (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
        params![name],
    )?;
    let id = conn.query_row(
        "SELECT id FROM channels WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(id)
}

#[allow(dead_code)]
pub fn list_channels(conn: &Connection) -> Result<Vec<Channel>> {
    let mut stmt = conn.prepare("SELECT id, name FROM channels ORDER BY name")?;
    let channels = stmt.query_map([], |r| Ok(Channel { id: r.get(0)?, name: r.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(channels)
}

/// Replace the set of channels a track belongs to.
pub fn set_track_channels(conn: &Connection, track_id: i64, channel_ids: &[i64]) -> Result<()> {
    conn.execute("DELETE FROM channel_tracks WHERE track_id = ?1", params![track_id])?;
    for &channel_id in channel_ids {
        conn.execute(
            "INSERT INTO channel_tracks (channel_id, track_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![channel_id, track_id],
        )?;
    }
    Ok(())
}

pub fn count_channel_tracks(conn: &Connection, channel_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM channel_tracks WHERE channel_id = ?1",
        params![channel_id],
        |r| r.get(0),
    )?)
}

/// Return the last virtual_start_secs + duration_secs covered in `channel_id`'s
/// timeline, or `since` if that channel's timeline is empty.
fn timeline_end(conn: &Connection, channel_id: i64, since: f64) -> Result<f64> {
    let result: Option<f64> = conn.query_row(
        "SELECT t2.virtual_start_secs + tr.duration_secs
         FROM timeline t2
         JOIN tracks tr ON tr.id = t2.track_id
         WHERE t2.channel_id = ?1
         ORDER BY t2.position DESC LIMIT 1",
        params![channel_id],
        |r| r.get(0),
    ).optional()?;
    Ok(result.unwrap_or(since))
}

/// Extend `channel_id`'s timeline so it covers at least `until_secs`, starting
/// from `since_secs` if the timeline is empty or ends before `since_secs`.
/// Picks tracks at random from that channel's tracks.
pub fn ensure_timeline_covers(conn: &Connection, channel_id: i64, since_secs: f64, until_secs: f64) -> Result<()> {
    let mut end = timeline_end(conn, channel_id, since_secs)?;
    if end < since_secs { end = since_secs; }

    while end < until_secs {
        // Pick a random track belonging to this channel.
        let (track_id, duration): (i64, f64) = conn.query_row(
            "SELECT tr.id, tr.duration_secs
             FROM channel_tracks ct
             JOIN tracks tr ON tr.id = ct.track_id
             WHERE ct.channel_id = ?1
             ORDER BY RANDOM() LIMIT 1",
            params![channel_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).with_context(|| format!("channel {channel_id} has no tracks"))?;
        let start = end;
        conn.execute(
            "INSERT INTO timeline (channel_id, track_id, virtual_start_secs) VALUES (?1, ?2, ?3)",
            params![channel_id, track_id, start],
        )?;
        end = start + duration;
    }
    Ok(())
}

/// Get the timeline entry that is playing at `unix_secs` on `channel_id`.
/// Extends the timeline if needed.
pub fn get_entry_at(conn: &Connection, channel_id: i64, unix_secs: f64) -> Result<TimelineEntry> {
    ensure_timeline_covers(conn, channel_id, unix_secs, unix_secs + 3600.0)?;
    let entry = conn.query_row(
        "SELECT tl.position, tl.track_id, tr.path, tr.duration_secs,
                tr.frame_count, tr.sample_rate, tl.virtual_start_secs
         FROM timeline tl
         JOIN tracks tr ON tr.id = tl.track_id
         WHERE tl.channel_id = ?1
           AND tl.virtual_start_secs <= ?2
           AND tl.virtual_start_secs + tr.duration_secs > ?2
         ORDER BY tl.position DESC LIMIT 1",
        params![channel_id, unix_secs],
        row_to_entry,
    )?;
    Ok(entry)
}

/// Get the timeline entry immediately after `position` on `channel_id`.
pub fn get_entry_after(conn: &Connection, channel_id: i64, position: i64) -> Result<TimelineEntry> {
    let entry = conn.query_row(
        "SELECT tl.position, tl.track_id, tr.path, tr.duration_secs,
                tr.frame_count, tr.sample_rate, tl.virtual_start_secs
         FROM timeline tl
         JOIN tracks tr ON tr.id = tl.track_id
         WHERE tl.channel_id = ?1 AND tl.position > ?2
         ORDER BY tl.position ASC LIMIT 1",
        params![channel_id, position],
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
