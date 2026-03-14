mod config;
mod db;
mod mp3;
mod scan;

use axum::{
    Router,
    Json,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::{AtomicU32, AtomicU64, Ordering}},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{fs, net::TcpListener, sync::broadcast, time};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

pub const PREBUFFER_CHUNKS: usize = 6;
pub const CHUNK_MS: u64 = 500;
const ICY_METAINT: usize = 8192;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(about = "servito — live MP3 HTTP stream server")]
struct Args {
    /// Path to config file [default: ~/.config/streamer/config.toml]
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the track library
    Library {
        #[command(subcommand)]
        command: LibraryCommand,
    },
    /// Start the HTTP stream server
    Stream,
    /// Show what is currently playing on a running stream
    NowPlaying,
}

#[derive(Subcommand)]
enum LibraryCommand {
    /// Scan configured paths and add new/changed files to the library
    Scan,
    /// List all tracks in the library
    List,
    /// Remove one or more tracks by ID
    Remove {
        /// Track IDs to remove
        #[arg(required = true)]
        ids: Vec<i64>,
    },
}

fn resolve_config(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p);
    }
    let home = std::env::var("HOME").map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("$HOME is not set"))?;
    Ok(home.join(".config/servito/config.toml"))
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Stats {
    clients_active: Arc<AtomicU32>,
    clients_total:  Arc<AtomicU64>,
    bytes_read:     Arc<AtomicU64>,
    bytes_streamed: Arc<AtomicU64>,
}

impl Stats {
    fn new() -> Self {
        Self {
            clients_active: Arc::new(AtomicU32::new(0)),
            clients_total:  Arc::new(AtomicU64::new(0)),
            bytes_read:     Arc::new(AtomicU64::new(0)),
            bytes_streamed: Arc::new(AtomicU64::new(0)),
        }
    }

    fn print(&self, label: &str) {
        let active   = self.clients_active.load(Ordering::Relaxed);
        let total    = self.clients_total.load(Ordering::Relaxed);
        let read     = self.bytes_read.load(Ordering::Relaxed);
        let streamed = self.bytes_streamed.load(Ordering::Relaxed);
        println!(
            "{} [{label}] clients={active}  total={total}  read={}  streamed={}",
            ts(),
            fmt_bytes(read),
            fmt_bytes(streamed),
        );
    }
}

fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn fmt_bytes(n: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if n >= GB      { format!("{:.2}GB", n as f64 / GB as f64) }
    else if n >= MB { format!("{:.1}MB", n as f64 / MB as f64) }
    else            { format!("{:.1}KB", n as f64 / 1024.0) }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct NowPlaying {
    path:           String,
    title:          String,
    duration_secs:  f64,
    position_secs:  f64,
}

#[derive(Clone)]
struct AppState {
    tx:               Arc<broadcast::Sender<Bytes>>,
    prebuffer:        Arc<Mutex<VecDeque<Bytes>>>,
    avg_bitrate_kbps: Arc<AtomicU32>,
    current_meta:     Arc<Mutex<String>>,
    now_playing:      Arc<Mutex<Option<NowPlaying>>>,
    stats:            Stats,
}

// ---------------------------------------------------------------------------
// Client lifecycle
// ---------------------------------------------------------------------------

struct ClientGuard {
    id:    u64,
    stats: Stats,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.stats.clients_active.fetch_sub(1, Ordering::Relaxed);
        self.stats.print(&format!("disconnect id={}", self.id));
    }
}

// ---------------------------------------------------------------------------
// ICY metadata encoding
// ---------------------------------------------------------------------------

/// Encode a metadata string into an ICY metadata block.
/// Format: 1 length byte (n = ceil(text.len()/16)), then n*16 bytes of text zero-padded.
fn icy_meta_block(meta: &str) -> Bytes {
    let text = meta.as_bytes();
    let n = (text.len() + 15) / 16;
    let mut block = vec![0u8; 1 + n * 16];
    block[0] = n as u8;
    block[1..1 + text.len()].copy_from_slice(text);
    Bytes::from(block)
}

/// Empty metadata block (one zero byte — length 0).
fn icy_empty_block() -> Bytes {
    Bytes::from_static(&[0u8])
}

// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------

async fn stream_handler(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    let wants_meta = req.headers()
        .get("icy-metadata")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    let id = state.stats.clients_total.fetch_add(1, Ordering::Relaxed) + 1;
    state.stats.clients_active.fetch_add(1, Ordering::Relaxed);

    let rx      = state.tx.subscribe();
    let prebuf: Vec<Bytes> = state.prebuffer.lock().unwrap().iter().cloned().collect();
    let avg_br  = state.avg_bitrate_kbps.load(Ordering::Relaxed);

    state.stats.print(&format!("connect id={id}"));

    let guard        = ClientGuard { id, stats: state.stats.clone() };
    let stats_stream = state.stats.clone();
    let meta_state   = state.current_meta.clone();

    let prebuf_stream = futures_util::stream::iter(prebuf)
        .map(Ok::<_, std::convert::Infallible>);

    let live_stream = BroadcastStream::new(rx).filter_map(move |r| async move {
        match r {
            Ok(bytes) => Some(bytes),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                println!("{} [client {id}] lagged, skipped {n} chunk(s)", ts());
                None
            }
        }
    });

    let combined = prebuf_stream.chain(live_stream.map(Ok::<_, std::convert::Infallible>));

    // State carried through unfold:
    //   inner        — the audio chunk stream
    //   guard        — disconnect logging
    //   remainder    — leftover audio bytes from the previous chunk that didn't fill a full metaint window
    //   until_meta   — bytes of audio still to emit before the next metadata block
    //   last_meta    — the last metadata string we injected (so we can send empty blocks when unchanged)
    //   first        — true until the very first metadata block has been sent
    #[allow(dead_code)]
    struct IcyState<S> {
        inner:      S,
        guard:      ClientGuard,
        remainder:  Bytes,
        until_meta: usize,
        last_meta:  String,
        first:      bool,
    }

    if wants_meta {
        let icy_state = IcyState {
            inner:      Box::pin(combined),
            guard,
            remainder:  Bytes::new(),
            until_meta: ICY_METAINT,
            last_meta:  String::new(), // force first block to be sent
            first:      true,
        };

        let body_stream = futures_util::stream::unfold(
            (icy_state, meta_state, stats_stream),
            move |(mut s, meta_state, stats)| async move {
                loop {
                    // If we have enough remainder to fill until the next meta point, emit audio.
                    if s.remainder.len() >= s.until_meta {
                        let audio_slice = s.remainder.slice(..s.until_meta);
                        s.remainder = s.remainder.slice(s.until_meta..);

                        // Build metadata block.
                        let current = meta_state.lock().unwrap().clone();
                        let meta_block = if current != s.last_meta || s.first {
                            s.last_meta = current.clone();
                            s.first = false;
                            icy_meta_block(&current)
                        } else {
                            icy_empty_block()
                        };
                        s.until_meta = ICY_METAINT;

                        let mut out = Vec::with_capacity(audio_slice.len() + meta_block.len());
                        out.extend_from_slice(&audio_slice);
                        out.extend_from_slice(&meta_block);
                        let out = Bytes::from(out);
                        stats.bytes_streamed.fetch_add(audio_slice.len() as u64, Ordering::Relaxed);
                        return Some((Ok::<_, std::convert::Infallible>(out), (s, meta_state, stats)));
                    }

                    // Need more audio. Pull next chunk from stream.
                    match s.inner.next().await {
                        None => return None,
                        Some(chunk) => {
                            let bytes = chunk.unwrap_or_else(|e| match e {});
                            // Append to remainder.
                            if s.remainder.is_empty() {
                                s.remainder = bytes;
                            } else {
                                let mut v = Vec::with_capacity(s.remainder.len() + bytes.len());
                                v.extend_from_slice(&s.remainder);
                                v.extend_from_slice(&bytes);
                                s.remainder = Bytes::from(v);
                            }
                        }
                    }
                }
            },
        );

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("Connection", "keep-alive")
            .header("Accept-Ranges", "none")
            .header("icy-br", avg_br.to_string())
            .header("icy-metaint", ICY_METAINT.to_string())
            .header("icy-name", "stream")
            .header("icy-pub", "0")
            .body(Body::from_stream(body_stream))
            .unwrap()
    } else {
        // No metadata requested — plain audio stream.
        let body_stream = futures_util::stream::unfold(
            (Box::pin(combined), guard),
            move |(mut inner, guard)| {
                let stats = stats_stream.clone();
                async move {
                    inner.next().await.map(|chunk| {
                        let bytes = chunk.unwrap_or_else(|e| match e {});
                        stats.bytes_streamed.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        (Ok::<_, std::convert::Infallible>(bytes), (inner, guard))
                    })
                }
            },
        );

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("Connection", "keep-alive")
            .header("Accept-Ranges", "none")
            .header("icy-br", avg_br.to_string())
            .header("icy-name", "stream")
            .header("icy-pub", "0")
            .body(Body::from_stream(body_stream))
            .unwrap()
    }
}

async fn nowplaying_handler(State(state): State<AppState>) -> Response {
    match state.now_playing.lock().unwrap().clone() {
        Some(np) => Json(np).into_response(),
        None => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("stream not yet started"))
            .unwrap(),
    }
}

// ---------------------------------------------------------------------------
// Virtual playhead broadcaster
// ---------------------------------------------------------------------------

struct LoadedAudio {
    data:      Vec<u8>,
    frames:    Vec<mp3::Frame>,
    frame_idx: usize,
    fpc:       usize,
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn ts() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}


fn update_track_state(
    entry: &db::TimelineEntry,
    current_meta: &Mutex<String>,
    now_playing: &Mutex<Option<NowPlaying>>,
    now: f64,
) {
    let stem = std::path::Path::new(&entry.path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.path.clone());
    *current_meta.lock().unwrap() = format!("StreamTitle='{stem}';");
    *now_playing.lock().unwrap() = Some(NowPlaying {
        path:          entry.path.clone(),
        title:         stem,
        duration_secs: entry.duration_secs,
        position_secs: (now - entry.virtual_start_secs).max(0.0),
    });
}

async fn broadcaster_task(
    db_path:      std::path::PathBuf,
    tx:           Arc<broadcast::Sender<Bytes>>,
    prebuffer:    Arc<Mutex<VecDeque<Bytes>>>,
    avg_bitrate_kbps: Arc<AtomicU32>,
    current_meta: Arc<Mutex<String>>,
    now_playing:  Arc<Mutex<Option<NowPlaying>>>,
    stats:        Stats,
) {
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => { eprintln!("{} [broadcaster] failed to open DB: {e}", ts()); return; }
    };

    let now = unix_now();

    // Get the first timeline entry covering right now.
    let mut entry = match db::get_entry_at(&conn, now) {
        Ok(e) => e,
        Err(e) => { eprintln!("{} [broadcaster] no timeline entry at startup: {e}", ts()); return; }
    };
    update_track_state(&entry, &current_meta, &now_playing, now);

    let mut audio: Option<LoadedAudio> = None;

    let mut ticker = time::interval(Duration::from_millis(CHUNK_MS));
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let now = unix_now();

        // Advance to correct timeline entry.
        loop {
            let track_end = entry.virtual_start_secs + entry.duration_secs;
            if now < track_end { break; }
            // Current track ended in virtual time. Advance.
            match db::get_entry_after(&conn, entry.position) {
                Ok(next) => {
                    if entry.track_id != next.track_id {
                        println!("{} [track] {}", ts(), next.path);
                        update_track_state(&next, &current_meta, &now_playing, now);
                    }
                    entry = next;
                    audio = None; // force reload
                }
                Err(e) => {
                    // Extend timeline and retry
                    let _ = db::ensure_timeline_covers(&conn, now, now + 3600.0);
                    match db::get_entry_after(&conn, entry.position) {
                        Ok(next) => {
                            update_track_state(&next, &current_meta, &now_playing, now);
                            entry = next;
                            audio = None;
                        }
                        Err(e2) => { eprintln!("{} [broadcaster] timeline gap: {e} / {e2}", ts()); break; }
                    }
                }
            }
        }

        // Keep position_secs current every tick.
        if let Some(np) = now_playing.lock().unwrap().as_mut() {
            np.position_secs = (now - entry.virtual_start_secs).max(0.0);
        }

        let receiver_count = tx.receiver_count();
        let prebuf_full = prebuffer.lock().unwrap().len() >= PREBUFFER_CHUNKS;

        if receiver_count == 0 && prebuf_full {
            // No clients and prebuffer is full — drop audio, keep virtual position.
            audio = None;
            continue;
        }

        // We have clients. Load audio if not loaded.
        if audio.is_none() {
            let data = match fs::read(&entry.path).await {
                Ok(d) => d,
                Err(e) => { eprintln!("{} [broadcaster] failed to read {}: {e}", ts(), entry.path); continue; }
            };
            stats.bytes_read.fetch_add(data.len() as u64, Ordering::Relaxed);
            let (frames, sample_rate) = mp3::parse_frames(&data);
            if frames.is_empty() {
                eprintln!("{} [broadcaster] no frames in {}", ts(), entry.path);
                continue;
            }

            // Compute avg bitrate for icy-br header
            let total_audio_bytes: usize = frames.iter().map(|f| f.end - f.start).sum();
            let duration_secs = frames.len() as f64 * 1152.0 / sample_rate as f64;
            let avg_br = (total_audio_bytes as f64 * 8.0 / duration_secs / 1000.0).round() as u32;
            avg_bitrate_kbps.store(avg_br, Ordering::Relaxed);

            let fpc = mp3::frames_per_chunk_for(sample_rate);

            // Seek to the correct frame based on virtual time elapsed.
            let elapsed = (unix_now() - entry.virtual_start_secs).max(0.0);
            let frames_elapsed = (elapsed * sample_rate as f64 / 1152.0) as usize;
            let frame_idx = frames_elapsed.min(frames.len().saturating_sub(1));

            println!("{} [broadcaster] loading {} @ frame {}/{}", ts(), entry.path, frame_idx, frames.len());

            audio = Some(LoadedAudio { data, frames, frame_idx, fpc });
        }

        // Build a chunk.
        let loaded = audio.as_mut().unwrap();
        let mut buf       = Vec::new();
        let mut remaining = loaded.fpc;

        while remaining > 0 {
            if loaded.frame_idx >= loaded.frames.len() {
                // Track exhausted — advance to next entry.
                match db::get_entry_after(&conn, entry.position) {
                    Ok(next) => {
                        println!("{} [track] {}", ts(), next.path);
                        update_track_state(&next, &current_meta, &now_playing, unix_now());
                        entry = next;
                        audio = None;
                        break; // emit partial chunk and reload next tick
                    }
                    Err(_) => {
                        let _ = db::ensure_timeline_covers(&conn, unix_now(), unix_now() + 3600.0);
                        break;
                    }
                }
            }

            let f = &loaded.frames[loaded.frame_idx];
            buf.extend_from_slice(&loaded.data[f.start..f.end]);
            loaded.frame_idx += 1;
            remaining -= 1;
        }

        if buf.is_empty() { continue; }

        let chunk = Bytes::from(buf);

        {
            let mut pb = prebuffer.lock().unwrap();
            pb.push_back(chunk.clone());
            if pb.len() > PREBUFFER_CHUNKS { pb.pop_front(); }
        }

        let _ = tx.send(chunk);
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config_path = resolve_config(args.config)?;

    match args.command {
        Command::Library { command } => {
            let cfg = config::load(&config_path)?;
            match command {
                LibraryCommand::Scan => scan::run(&cfg).await?,
                LibraryCommand::List => {
                    let conn = db::open(&cfg.db)?;
                    let tracks = db::list_tracks(&conn)?;
                    if tracks.is_empty() {
                        println!("Library is empty.");
                    } else {
                        println!("{:>6}  {:>8}  {}", "ID", "Duration", "Path");
                        println!("{}", "-".repeat(60));
                        for t in tracks {
                            println!("{:>6}  {:>7.1}s  {}", t.id, t.duration_secs, t.path);
                        }
                    }
                }
                LibraryCommand::Remove { ids } => {
                    let conn = db::open(&cfg.db)?;
                    let removed = db::remove_tracks(&conn, &ids)?;
                    println!("Removed {} track(s).", removed);
                }
            }
        }
        Command::Stream => {
            let cfg = config::load(&config_path)?;
            stream(cfg).await?;
        }
        Command::NowPlaying => {
            let cfg = config::load(&config_path)?;
            let url = format!("http://127.0.0.1:{}/nowplaying", cfg.stream.port);
            let np: NowPlaying = ureq::get(&url)
                .call()
                .map_err(|e| anyhow::anyhow!("could not reach stream: {e}"))?
                .into_json()?;
            let pos = np.position_secs as u64;
            let dur = np.duration_secs as u64;
            println!("{}", np.title);
            println!("{} / {}", fmt_duration(pos), fmt_duration(dur));
            println!("{}", np.path);
        }
    }

    Ok(())
}

async fn stream(cfg: config::Config) -> anyhow::Result<()> {
    let conn = db::open(&cfg.db)?;
    let track_count = db::count_tracks(&conn)?;
    if track_count == 0 {
        anyhow::bail!("Library is empty — run 'streamer scan <config>' first");
    }
    println!("{} Library: {} tracks", ts(), track_count);

    // Seed the timeline starting now.
    let now = unix_now();
    db::ensure_timeline_covers(&conn, now, now + 7200.0)?;
    drop(conn);

    let stats = Stats::new();

    let (tx, _)  = broadcast::channel::<Bytes>(256);
    let tx       = Arc::new(tx);
    let prebuffer: Arc<Mutex<VecDeque<Bytes>>> = Arc::new(Mutex::new(VecDeque::new()));
    let avg_bitrate_kbps: Arc<AtomicU32> = Arc::new(AtomicU32::new(128));
    let current_meta: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let now_playing: Arc<Mutex<Option<NowPlaying>>> = Arc::new(Mutex::new(None));

    // Broadcaster task.
    {
        let tx_bg        = tx.clone();
        let prebuffer_bg = prebuffer.clone();
        let avg_br_bg    = avg_bitrate_kbps.clone();
        let meta_bg      = current_meta.clone();
        let np_bg        = now_playing.clone();
        let stats_bg     = stats.clone();
        let db_path      = cfg.db.clone();

        tokio::spawn(async move {
            broadcaster_task(db_path, tx_bg, prebuffer_bg, avg_br_bg, meta_bg, np_bg, stats_bg).await;
        });
    }

    println!("{} Ready.", ts());

    // Periodic stats logger.
    if cfg.stream.log_interval_secs > 0 {
        let stats_log = stats.clone();
        let np_log    = now_playing.clone();
        let interval  = cfg.stream.log_interval_secs;
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(interval));
            ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let track_info = np_log.lock().unwrap().as_ref().map(|np| {
                    let title = std::path::Path::new(&np.path)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| np.path.clone());
                    let pos = np.position_secs as u64;
                    let rem = (np.duration_secs - np.position_secs).max(0.0) as u64;
                    format!("  track=\"{}\"  pos={}  rem={}", title, fmt_duration(pos), fmt_duration(rem))
                }).unwrap_or_default();
                let active   = stats_log.clients_active.load(Ordering::Relaxed);
                let total    = stats_log.clients_total.load(Ordering::Relaxed);
                let read     = stats_log.bytes_read.load(Ordering::Relaxed);
                let streamed = stats_log.bytes_streamed.load(Ordering::Relaxed);
                println!(
                    "{} [stats] clients={active}  total={total}  read={}  streamed={}{}",
                    ts(), fmt_bytes(read), fmt_bytes(streamed), track_info,
                );
            }
        });
    }

    let state = AppState {
        tx,
        prebuffer,
        avg_bitrate_kbps,
        current_meta,
        now_playing,
        stats,
    };

    let app      = Router::new()
        .route("/", get(stream_handler))
        .route("/nowplaying", get(nowplaying_handler))
        .with_state(state);
    let listener = TcpListener::bind(format!("0.0.0.0:{}", cfg.stream.port)).await?;
    println!("{} Streaming → http://0.0.0.0:{}/", ts(), cfg.stream.port);
    axum::serve(listener, app).await?;
    Ok(())
}
