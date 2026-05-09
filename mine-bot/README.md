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
cargo run -- config init
# edit config.conl
cargo run -- api
```

Default config path is `config.conl`. Override it with `-c/--config`.

Config format is auto-detected as JSON when the first non-empty line that is not a CONL `;` comment or JSONC `//` comment starts with `{`. Otherwise it is parsed as CONL. JSON inputs support whole-line `//` comments.

Default bind address is `127.0.0.1:53211`.

API port selection order is:
1. `-p/--port`
2. `PORT`
3. `../conf/ports.conl` entry for `mine-bot`
4. built-in fallback `53211`

`default` in `../conf/ports.conl` is documentation only and is not read at runtime.

API host selection order is:
1. `--host`
2. `HOST`
3. built-in fallback `127.0.0.1`

Telegram token selection order is:
1. `TELEGRAM_BOT_TOKEN`
2. `telegram.bot_token` in config

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

## API
`POST /download`

```json
{ "url": "https://example.com/post" }
```

Returns media DB id, normalized URL, post text, and produced files.
