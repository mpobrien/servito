use crate::{config::Config, db, mp3};
use anyhow::Result;
use futures_util::StreamExt;
use rusqlite::OptionalExtension;
use std::path::PathBuf;

struct PendingFile {
    path_str: String,
    is_new:   bool,
    size_bytes: i64,
}

struct ParsedFile {
    path_str:     String,
    is_new:       bool,
    size_bytes:   i64,
    duration_secs: f64,
    frame_count:  i64,
    sample_rate:  u32,
}

pub async fn run(config: &Config) -> Result<()> {
    let conn = db::open(&config.db)?;

    let candidates = expand_paths(&config.library.paths);
    println!("{} Found {} files to scan.", crate::ts(), candidates.len());

    // Pass 1: sequential metadata + DB check to decide what needs parsing.
    let mut pending: Vec<PendingFile> = Vec::new();
    let mut skipped = 0u64;

    for path in &candidates {
        let path_str = path.to_string_lossy().to_string();
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => { eprintln!("{}   skip {path_str}: {e}", crate::ts()); skipped += 1; continue; }
        };
        let size_bytes = meta.len() as i64;

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

        pending.push(PendingFile { path_str, is_new: existing.is_none(), size_bytes });
    }

    let concurrency = std::thread::available_parallelism().map_or(4, |n| n.get());
    println!("{} {} file(s) need parsing (concurrency={concurrency}).", crate::ts(), pending.len());

    // Pass 2: parallel file reads + frame parsing, capped at `concurrency`.
    let mut parsed: Vec<ParsedFile> = Vec::new();

    let mut stream = futures_util::stream::iter(pending)
        .map(|pf| async move {
            let data = tokio::fs::read(&pf.path_str).await
                .map_err(|e| (pf.path_str.clone(), e.to_string()))?;

            let (frames, sample_rate) = tokio::task::spawn_blocking(move || mp3::parse_frames(&data))
                .await
                .unwrap();

            if frames.is_empty() {
                return Err((pf.path_str, "no MPEG frames".to_string()));
            }

            let frame_count   = frames.len() as i64;
            let duration_secs = frame_count as f64 * 1152.0 / sample_rate as f64;

            Ok(ParsedFile {
                path_str: pf.path_str,
                is_new:   pf.is_new,
                size_bytes: pf.size_bytes,
                duration_secs,
                frame_count,
                sample_rate,
            })
        })
        .buffer_unordered(concurrency);

    while let Some(res) = stream.next().await {
        match res {
            Ok(pf) => {
                println!("{}  parsed {}  ({:.1}s  {} frames  {}Hz)",
                    crate::ts(), pf.path_str, pf.duration_secs, pf.frame_count, pf.sample_rate);
                parsed.push(pf);
            }
            Err((path_str, e)) => { eprintln!("{}   skip {path_str}: {e}", crate::ts()); skipped += 1; }
        }
    }

    // Pass 3: sequential DB upserts.
    let mut added   = 0u64;
    let mut updated = 0u64;

    for pf in parsed {
        db::upsert_track(&conn, &pf.path_str, pf.duration_secs, pf.size_bytes, pf.frame_count, pf.sample_rate as i64)?;
        if pf.is_new {
            println!("{}  + {}  ({:.1}s  {} frames  {}Hz)", crate::ts(), pf.path_str, pf.duration_secs, pf.frame_count, pf.sample_rate);
            added += 1;
        } else {
            println!("{}  ~ {}  (updated)", crate::ts(), pf.path_str);
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
