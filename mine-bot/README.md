# mine-bot

Rust Dropshot service + Telegram adapter for fetching social post media using `yt-dlp`.

## Features
- Accepts URL input and downloads media/post metadata.
- Prefers mobile-compatible output (`mp4` container, h264/h265 video via source, AAC/m4a audio).
- Returns post description + media (single video or grouped images in Telegram).
- Stores users and media metadata in SQLite.
- Seeds user roles (`admin|user|none`) from config at startup.

## Requirements
- Linux
- `yt-dlp` available on `$PATH`
- Optional `ffmpeg` for remux/merge

## Run
```bash
cp config.example.toml config.toml
# edit config.toml
cargo run
```

Config is loaded from `MINE_BOT_CONFIG` (default: `config.toml`).

Default bind address is `0.0.0.0:53211`.

## API
`POST /download`

```json
{ "url": "https://example.com/post" }
```

Returns media DB id, normalized URL, post text, and produced files.
