# AGENTS.md

## Project

`notnotsavebot` is a Rust 2024 Telegram bot.

1. Accepts social/media post URLs
2. Downloads media with `yt-dlp`
3. normalizes delivery media with `ffmpeg`, and sends the result back to Telegram.
- Persistent state is stored in SQLite.

## Stack

- Rust 2024, `src/main.rs`
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

## Useful Commands

```bash
cargo run -- config init
cargo run
cargo check
```
