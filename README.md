# servito

`servito` is a Rust app that scans an MP3 library into SQLite and serves it as a continuous live HTTP stream.

## Features

- Scans one or more MP3 paths, directories, or glob patterns
- Stores track metadata in a local SQLite database
- Builds a randomized playback timeline from the scanned library
- Serves a live MP3 stream over HTTP with ICY metadata
- Exposes `/nowplaying`, `/status`, and a simple built-in web UI at `/ui`

## Requirements

- Rust toolchain (for local builds)
- A directory of `.mp3` files

## Build

```sh
cargo build --release
```

The binary will be available at `target/release/servito`.

## Configuration

By default, `servito` looks for its config at `~/.config/servito/config.toml`.
You can also pass a config file explicitly with `-c` or `--config`.

Example config:

```toml
db = "/data/servito.db"

[stream]
port = 8000
log_interval_secs = 10

[library]
paths = [
  "/music",
  "/music/**/*.mp3",
]
# Optional: add this under [library] to override the default.
# scan_concurrency = 16
```

`scan_concurrency` is optional under `[library]`. If you omit it, `servito` chooses a default based on available CPU parallelism.

## Usage

Show CLI help:

```sh
servito --help
```

Scan the library:

```sh
servito -c /path/to/config.toml library scan
```

List scanned tracks:

```sh
servito -c /path/to/config.toml library list
```

Remove tracks by database ID:

```sh
servito -c /path/to/config.toml library remove 1 2 3
```

Start the stream server:

```sh
servito -c /path/to/config.toml stream
```

Print the currently playing track from a running local server by querying `/nowplaying`:

```sh
servito -c /path/to/config.toml now-playing
```

## HTTP Endpoints

Once the server is running on port `8000`:

- `GET /` — live MP3 stream
- `GET /nowplaying` — current track as JSON
- `GET /status` — current track, listener count, and recent history
- `GET /ui` — built-in web player

## Docker

Build the image:

```sh
docker build -t servito .
```

When running in Docker, make sure the config mounted at `/config/config.toml` uses container paths such as `db = "/data/servito.db"` and library paths under `/music`, so the mounted volumes match the config.

Run the stream server:

```sh
docker run --rm \
  -p 8000:8000 \
  -v /path/to/config:/config \
  -v /path/to/music:/music:ro \
  -v /path/to/data:/data \
  servito
```

The container entrypoint is:

```sh
servito -c /config/config.toml
```

The default command is `stream`, so you can override it for one-off tasks:

```sh
docker run --rm \
  -v /path/to/config:/config \
  -v /path/to/music:/music:ro \
  -v /path/to/data:/data \
  servito library scan
```
