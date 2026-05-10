# AGENTS.md

## Project
- `notnotsavebot` is a Rust 2024 Telegram bot.
- It accepts social/media post URLs, downloads media with `yt-dlp`, normalizes delivery media with `ffmpeg`, and sends the result back to Telegram.
- Persistent state is stored in SQLite.

## Stack
- Rust edition: `2024`
- Main crate entrypoint: `src/main.rs`
- Telegram library: `teloxide`
- Async runtime: `tokio`

## Runtime Requirements
- `yt-dlp` on `PATH`
- `ffmpeg` on `PATH`
- Telegram bot token via `TELEGRAM_BOT_TOKEN` or `telegram.bot_token` in `config.conl`

## Important Files
- [src/main.rs](src/main.rs): bot runtime, Telegram handlers, download/normalization flow, config CLI
- [Cargo.toml](Cargo.toml): crate metadata and dependencies
- [README.md](README.md): basic usage and config notes
- [templates/config.conl](templates/config.conl): config template

## Main Flow
- Telegram messages are handled in `run_telegram()`.
- URL downloads go through `handle_download_request()` -> `download_media()`.
- Video normalization happens in `normalize_video_for_delivery()`.
- Telegram delivery formatting happens in `send_download_result()` and `format_post_text_html()`.

## Useful Commands
```bash
cargo run -- config init
cargo run -- run
cargo check
```
