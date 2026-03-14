use crate::{config::Config, db, mp3};
use anyhow::Result;
use rusqlite::OptionalExtension;
use std::path::PathBuf;

pub async fn run(config: &Config) -> Result<()> {
    let conn = db::open(&config.db)?;

    let candidates = expand_paths(&config.library.paths);
    println!("{} Found {} files to scan.", crate::ts(), candidates.len());

    let mut added = 0u64;
    let mut updated = 0u64;
    let mut skipped = 0u64;

    for path in &candidates {
        let path_str = path.to_string_lossy().to_string();
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => { eprintln!("{}   skip {path_str}: {e}", crate::ts()); skipped += 1; continue; }
        };
        let size_bytes = meta.len() as i64;

        // Check if already in DB with same size
        let existing: Option<(i64, i64)> = conn.query_row(
            "SELECT id, size_bytes FROM tracks WHERE path = ?1",
            rusqlite::params![path_str],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;

        if let Some((_, db_size)) = existing {
            if db_size == size_bytes {
                skipped += 1;
                continue;
            }
        }

        // Need to parse
        let data = match tokio::fs::read(path).await {
            Ok(d) => d,
            Err(e) => { eprintln!("{}   skip {path_str}: {e}", crate::ts()); skipped += 1; continue; }
        };
        let (frames, sample_rate) = mp3::parse_frames(&data);
        if frames.is_empty() {
            eprintln!("{}   skip {path_str}: no MPEG frames", crate::ts());
            skipped += 1;
            continue;
        }

        let frame_count  = frames.len() as i64;
        let duration_secs = frame_count as f64 * 1152.0 / sample_rate as f64;

        let is_new = existing.is_none();
        db::upsert_track(&conn, &path_str, duration_secs, size_bytes, frame_count, sample_rate as i64)?;

        if is_new {
            println!("{}  + {path_str}  ({:.1}s  {} frames  {}Hz)", crate::ts(), duration_secs, frame_count, sample_rate);
            added += 1;
        } else {
            println!("{}  ~ {path_str}  (updated)", crate::ts());
            updated += 1;
        }
    }

    println!("{} Scan complete: {} added, {} updated, {} unchanged.", crate::ts(), added, updated, skipped);
    println!("{} Library total: {} tracks.", crate::ts(), db::count_tracks(&conn)?);
    Ok(())
}

fn expand_paths(patterns: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pattern in patterns {
        let p = std::path::Path::new(pattern);
        if p.is_file() {
            out.push(p.to_path_buf());
            continue;
        }
        if p.is_dir() {
            if let Ok(entries) = std::fs::read_dir(p) {
                let mut files: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && is_mp3(p))
                    .collect();
                files.sort();
                out.extend(files);
            }
            continue;
        }
        // Try as glob
        match glob::glob(pattern) {
            Ok(paths) => {
                let mut files: Vec<PathBuf> = paths
                    .flatten()
                    .filter(|p| p.is_file() && is_mp3(p))
                    .collect();
                files.sort();
                out.extend(files);
            }
            Err(e) => eprintln!("{} Invalid glob pattern {pattern}: {e}", crate::ts()),
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_mp3(p: &std::path::Path) -> bool {
    p.extension().map(|e| e.eq_ignore_ascii_case("mp3")).unwrap_or(false)
}
