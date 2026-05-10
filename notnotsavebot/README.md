# notnotsavebot

Rust Telegram bot for fetching social post media using `yt-dlp`.

## Features

- Accepts URL input and downloads media/post metadata.
- Prefers mobile-compatible output (`mp4` container, h264/h265 video via source, AAC/m4a audio).
- Returns post description + media (single video or grouped images in Telegram).
- Stores users and media metadata in SQLite.
- Seeds user roles (`admin|user|none`) from config at startup.
- Only users seeded with role `admin` or `user` are allowed to use the bot; everyone else is rejected by default.
- Uses the shared `download_media()` path directly instead of exposing an HTTP API.
- Supports Telegram slash commands including `/start`, `/help`, `/id`, and `/download <url>`.

## Requirements

- Linux
- `yt-dlp` available on `$PATH`
- `ffmpeg` available on `$PATH`

## Run

```bash
cargo run -- config init
# edit config.conl
cargo run -- run
```

Default config path is `config.conl`. Override it with `-c/--config`.

Config format is auto-detected as JSON or CONL

## Config CLI

```bash
cargo run -- config init
cargo run -- config schema
cargo run -- config check
cargo run -- config json
cargo run -- config list
cargo run -- config get telegram.enabled
cargo run -- config get --all
cargo run -- config set telegram.enabled true
```
