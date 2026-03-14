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
    path_str:      String,
    is_new:        bool,
    size_bytes:    i64,
    duration_secs: f64,
    frame_count:   i64,
    sample_rate:   u32,
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

    // Pass 2: parallel parse + immediate DB write on each result.
    let total       = pending.len();
    let mut done    = 0usize;
    let mut added   = 0u64;
    let mut updated = 0u64;

    let mut stream = futures_util::stream::iter(pending)
        .map(|pf| async move {
            use std::io::SeekFrom;
            use tokio::io::{AsyncReadExt, AsyncSeekExt};

            let mut file = tokio::fs::File::open(&pf.path_str).await
                .map_err(|e| (pf.path_str.clone(), e.to_string()))?;

            // Read the first 10 bytes to detect an ID3 header.
            let mut lead = [0u8; 10];
            file.read_exact(&mut lead).await
                .map_err(|e| (pf.path_str.clone(), e.to_string()))?;

            let id3_size = if &lead[0..3] == b"ID3" {
                let sz = ((lead[6] as usize) << 21) | ((lead[7] as usize) << 14)
                       | ((lead[8] as usize) <<  7) |  lead[9] as usize;
                sz + 10
            } else {
                0
            };

            // Build a small probe buffer starting at the first audio byte.
            // If no ID3, reuse the 10 bytes already read; otherwise seek past the tag.
            let mut probe = vec![0u8; 2048];
            let n = if id3_size == 0 {
                probe[..10].copy_from_slice(&lead);
                let rest = file.read(&mut probe[10..]).await
                    .map_err(|e| (pf.path_str.clone(), e.to_string()))?;
                10 + rest
            } else {
                file.seek(SeekFrom::Start(id3_size as u64)).await
                    .map_err(|e| (pf.path_str.clone(), e.to_string()))?;
                file.read(&mut probe).await
                    .map_err(|e| (pf.path_str.clone(), e.to_string()))?
            };
            probe.truncate(n);

            let (frame_count, sample_rate) = {
                let path = pf.path_str.clone();
                let probe_result = tokio::task::spawn_blocking(move || mp3::probe_duration(&probe))
                    .await
                    .unwrap();

                match probe_result {
                    mp3::ProbeResult::Known(c, sr) => (c as i64, sr),
                    mp3::ProbeResult::Invalid => return Err((pf.path_str, "no MPEG frames".to_string())),
                    mp3::ProbeResult::NeedsFullScan => {
                        let data = tokio::fs::read(&path).await
                            .map_err(|e| (pf.path_str.clone(), e.to_string()))?;
                        match tokio::task::spawn_blocking(move || mp3::count_frames(&data)).await.unwrap() {
                            Some((c, sr)) => (c as i64, sr),
                            None => return Err((pf.path_str, "no MPEG frames".to_string())),
                        }
                    }
                }
            };

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
                db::upsert_track(&conn, &pf.path_str, pf.duration_secs, pf.size_bytes, pf.frame_count, pf.sample_rate as i64)?;
                done += 1;
                let size = crate::fmt_bytes(pf.size_bytes as u64);
                let dur  = crate::fmt_duration(pf.duration_secs as u64);
                if pf.is_new {
                    println!("{}  + {}  ({size}  {dur})  (scanned {done}/{total})", crate::ts(), pf.path_str);
                    added += 1;
                } else {
                    println!("{}  ~ {}  ({size}  {dur})  (scanned {done}/{total})", crate::ts(), pf.path_str);
                    updated += 1;
                }
            }
            Err((path_str, e)) => {
                done += 1;
                eprintln!("{}   skip {path_str}: {e}  (scanned {done}/{total})", crate::ts());
                skipped += 1;
            }
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
