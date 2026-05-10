use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Instant,
};

use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use teloxide::{
    prelude::*,
    types::{InputFile, InputMedia, InputMediaPhoto, LinkPreviewOptions, ParseMode},
    utils::command::BotCommands,
};
use tokio::{process::Command as TokioCommand, task};
use tracing::{error, info};
use url::Url;
use uuid::Uuid;
use which::which;

#[derive(Debug, Parser)]
#[command(name = "mine-bot")]
struct Cli {
    #[arg(short = 'c', long = "config", default_value = "config.conl")]
    config: Utf8PathBuf,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    Run,
    Config {
        #[command(subcommand)]
        command: ConfigSubcommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
    Init,
    Schema,
    Check,
    Json,
    Get {
        #[arg(long)]
        all: bool,
        key: Option<String>,
    },
    Set {
        key: String,
        value: String,
    },
    List,
}

#[derive(Debug, Clone, Copy)]
enum ConfigFormat {
    Json,
    Conl,
}

#[derive(Debug, Clone)]
struct LoadedConfig {
    format: ConfigFormat,
    config: Config,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct Config {
    #[serde(default = "default_database_path")]
    #[schemars(with = "String")]
    database_path: Utf8PathBuf,
    #[serde(default = "default_download_dir")]
    #[schemars(with = "String")]
    download_dir: Utf8PathBuf,
    #[serde(default)]
    telegram: TelegramConfig,
    #[serde(default)]
    seed_users: Vec<SeedUser>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_path: default_database_path(),
            download_dir: default_download_dir(),
            telegram: TelegramConfig::default(),
            seed_users: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct TelegramConfig {
    enabled: bool,
    bot_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SeedUser {
    platform: String,
    platform_user_id: String,
    role: UserRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum UserRole {
    Admin,
    User,
    None,
}

impl UserRole {
    fn as_str(&self) -> &'static str {
        match self {
            UserRole::Admin => "admin",
            UserRole::User => "user",
            UserRole::None => "none",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "admin" => Ok(UserRole::Admin),
            "user" => Ok(UserRole::User),
            "none" => Ok(UserRole::None),
            _ => bail!("invalid stored user role: {value}"),
        }
    }

    fn is_allowed(self) -> bool {
        matches!(self, UserRole::Admin | UserRole::User)
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct DownloadResponse {
    media_id: i64,
    normalized_url: String,
    post_text: String,
    files: Vec<String>,
}

#[derive(Debug, Clone)]
struct DeliveryFilenameMetadata {
    username: String,
    date: String,
    title: String,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Available commands:")]
enum TelegramCommand {
    #[command(description = "show a short introduction")]
    Start,
    #[command(description = "show this help message")]
    Help,
    #[command(description = "show your Telegram user id")]
    Id,
    #[command(description = "download media from a URL")]
    Download(String),
}

#[derive(Debug, Clone)]
enum PathSegment {
    Key(String),
    Index(usize),
}

const CONFIG_TEMPLATE: &str = include_str!("../templates/config.conl");

fn default_database_path() -> Utf8PathBuf {
    Utf8PathBuf::from("./mine-bot.sqlite")
}

fn default_download_dir() -> Utf8PathBuf {
    Utf8PathBuf::from("./downloads")
}

const MAX_DELIVERY_FILENAME_LEN: usize = 79;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Run => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .init();
            run_bot(cli.config).await
        }
        CliCommand::Config { command } => run_config_command(&cli.config, command),
    }
}

async fn run_bot(config_path: Utf8PathBuf) -> Result<()> {
    let mut config = load_config(&config_path)?.config;
    apply_env_overrides(&mut config);
    ensure_runtime_tools()?;

    fs::create_dir_all(&config.download_dir)?;
    init_db(&config)?;

    if !config.telegram.enabled {
        bail!("no runtime adapters enabled; set telegram.enabled = true");
    }

    let config = Arc::new(config);
    info!("mine-bot running");
    run_telegram(config).await
}

fn apply_env_overrides(config: &mut Config) {
    if let Ok(bot_token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        config.telegram.bot_token = bot_token;
    }
}

fn ensure_runtime_tools() -> Result<()> {
    let yt_dlp = which("yt-dlp")
        .context("required tool `yt-dlp` was not found on PATH; install it or fix the bot service PATH")?;
    info!("found yt-dlp at {}", yt_dlp.display());

    let ffmpeg = which("ffmpeg")
        .context("required tool `ffmpeg` was not found on PATH; install it or fix the bot service PATH")?;
    info!("found ffmpeg at {}", ffmpeg.display());

    Ok(())
}

fn run_config_command(config_path: &Utf8PathBuf, command: ConfigSubcommand) -> Result<()> {
    match command {
        ConfigSubcommand::Init => {
            init_config(config_path)?;
            println!("created {}", config_path);
            Ok(())
        }
        ConfigSubcommand::Schema => {
            let schema = schema_for!(Config);
            println!("{}", serde_json::to_string_pretty(&schema)?);
            Ok(())
        }
        ConfigSubcommand::Check => {
            load_config(config_path)?;
            println!("ok");
            Ok(())
        }
        ConfigSubcommand::Json => {
            let loaded = load_config(config_path)?;
            println!("{}", serde_json::to_string_pretty(&loaded.config)?);
            Ok(())
        }
        ConfigSubcommand::Get { all, key } => {
            let loaded = load_config(config_path)?;
            let value = serde_json::to_value(&loaded.config)?;

            match (all, key) {
                (true, Some(key)) => print_flattened_values_for_key(&value, &key),
                (true, None) => print_flattened_values(&value, None),
                (false, Some(key)) => {
                    let path = parse_config_path(&key)?;
                    let found = get_value_at_path(&value, &path)
                        .with_context(|| format!("config key not found: {key}"))?;
                    print_json_value(found)
                }
                (false, None) => bail!("config get requires KEY or --all"),
            }
        }
        ConfigSubcommand::Set { key, value } => {
            let mut loaded = load_or_default_config(config_path)?;
            let mut document = serde_json::to_value(&loaded.config)?;
            let path = parse_config_path(&key)?;
            let new_value = parse_cli_value(&value);
            set_value_at_path(&mut document, &path, new_value)?;
            loaded.config = serde_json::from_value(document)
                .with_context(|| format!("updated config is invalid after setting {key}"))?;
            save_config(config_path, loaded.format, &loaded.config)?;
            println!("updated {key}");
            Ok(())
        }
        ConfigSubcommand::List => {
            let loaded = load_config(config_path)?;
            let value = serde_json::to_value(&loaded.config)?;
            let mut paths = flatten_config_paths(&value, None);
            paths.sort();
            for path in paths {
                println!("{path}");
            }
            Ok(())
        }
    }
}

fn init_config(path: &Utf8PathBuf) -> Result<()> {
    if path.exists() {
        bail!("refusing to overwrite existing config at {path}");
    }

    if let Some(parent) = path.parent() {
        if !parent.as_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    fs::write(path, CONFIG_TEMPLATE)
        .with_context(|| format!("unable to write config at {path}"))?;
    Ok(())
}

fn load_or_default_config(path: &Utf8PathBuf) -> Result<LoadedConfig> {
    if path.exists() {
        return load_config(path);
    }

    Ok(LoadedConfig {
        format: default_format_for_path(path),
        config: Config::default(),
    })
}

fn load_config(path: &Utf8PathBuf) -> Result<LoadedConfig> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("unable to read config at {path}"))?;
    let format = detect_config_format(&raw);
    let config = match format {
        ConfigFormat::Json => serde_json::from_str(&strip_jsonc_comment_lines(&raw))
            .with_context(|| format!("unable to parse JSON config at {path}"))?,
        ConfigFormat::Conl => serde_conl::from_str(&raw)
            .with_context(|| format!("unable to parse CONL config at {path}"))?,
    };

    Ok(LoadedConfig { format, config })
}

fn save_config(path: &Utf8PathBuf, format: ConfigFormat, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let rendered = match format {
        ConfigFormat::Json => format!("{}\n", serde_json::to_string_pretty(config)?),
        ConfigFormat::Conl => {
            let mut text = serde_conl::to_string(config)?;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text
        }
    };

    fs::write(path, rendered).with_context(|| format!("unable to write config at {path}"))?;
    Ok(())
}

fn default_format_for_path(path: &Utf8PathBuf) -> ConfigFormat {
    match path.extension() {
        Some("json") => ConfigFormat::Json,
        _ => ConfigFormat::Conl,
    }
}

fn detect_config_format(text: &str) -> ConfigFormat {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with("//") {
            continue;
        }
        return if trimmed.starts_with('{') {
            ConfigFormat::Json
        } else {
            ConfigFormat::Conl
        };
    }
    ConfigFormat::Conl
}

fn strip_jsonc_comment_lines(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn print_json_value(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_flattened_values_for_key(root: &Value, key: &str) -> Result<()> {
    let path = parse_config_path(key)?;
    let value =
        get_value_at_path(root, &path).with_context(|| format!("config key not found: {key}"))?;
    print_flattened_values(value, Some(key))
}

fn print_flattened_values(root: &Value, prefix: Option<&str>) -> Result<()> {
    let mut entries = flatten_config_entries(root, prefix);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, value) in entries {
        println!("{path} = {}", serde_json::to_string(&value)?);
    }
    Ok(())
}

fn flatten_config_paths(root: &Value, prefix: Option<&str>) -> Vec<String> {
    flatten_config_entries(root, prefix)
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

fn flatten_config_entries(root: &Value, prefix: Option<&str>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    collect_config_entries(root, prefix, &mut out);
    out
}

fn collect_config_entries(value: &Value, prefix: Option<&str>, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let next = match prefix {
                    Some(prefix) if !prefix.is_empty() => format!("{prefix}.{key}"),
                    _ => key.clone(),
                };
                collect_config_entries(child, Some(&next), out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (index, child) in items.iter().enumerate() {
                let next = match prefix {
                    Some(prefix) if !prefix.is_empty() => format!("{prefix}[{index}]"),
                    _ => format!("[{index}]"),
                };
                collect_config_entries(child, Some(&next), out);
            }
        }
        _ => out.push((prefix.unwrap_or("$").to_string(), value.clone())),
    }
}

fn parse_cli_value(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_string()))
}

fn parse_config_path(path: &str) -> Result<Vec<PathSegment>> {
    if path.is_empty() {
        bail!("config key cannot be empty");
    }

    let bytes = path.as_bytes();
    let mut current = String::new();
    let mut segments = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                if current.is_empty() {
                    bail!("invalid config key: {path}");
                }
                segments.push(PathSegment::Key(std::mem::take(&mut current)));
                i += 1;
            }
            b'[' => {
                if !current.is_empty() {
                    segments.push(PathSegment::Key(std::mem::take(&mut current)));
                }
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if start == i || i >= bytes.len() || bytes[i] != b']' {
                    bail!("invalid config key: {path}");
                }
                let index = std::str::from_utf8(&bytes[start..i])?.parse::<usize>()?;
                segments.push(PathSegment::Index(index));
                i += 1;
                if i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                    bail!("invalid config key: {path}");
                }
            }
            byte => {
                current.push(byte as char);
                i += 1;
            }
        }
    }

    if !current.is_empty() {
        segments.push(PathSegment::Key(current));
    }

    if segments.is_empty() {
        bail!("invalid config key: {path}");
    }

    Ok(segments)
}

fn get_value_at_path<'a>(value: &'a Value, path: &[PathSegment]) -> Option<&'a Value> {
    let mut current = value;
    for segment in path {
        current = match segment {
            PathSegment::Key(key) => current.get(key)?,
            PathSegment::Index(index) => current.get(*index)?,
        };
    }
    Some(current)
}

fn set_value_at_path(value: &mut Value, path: &[PathSegment], new_value: Value) -> Result<()> {
    if path.is_empty() {
        *value = new_value;
        return Ok(());
    }

    match &path[0] {
        PathSegment::Key(key) => {
            let object = value
                .as_object_mut()
                .ok_or_else(|| anyhow!("config path traverses a non-object at key {key}"))?;

            if path.len() == 1 {
                object.insert(key.clone(), new_value);
                return Ok(());
            }

            let next = object
                .entry(key.clone())
                .or_insert_with(|| container_for_segment(&path[1]));
            if next.is_null() {
                *next = container_for_segment(&path[1]);
            }
            set_value_at_path(next, &path[1..], new_value)
        }
        PathSegment::Index(index) => {
            let array = value
                .as_array_mut()
                .ok_or_else(|| anyhow!("config path traverses a non-array at index {index}"))?;

            if array.len() <= *index {
                let fill = if path.len() == 1 {
                    Value::Null
                } else {
                    container_for_segment(&path[1])
                };
                array.resize(*index + 1, fill);
            }

            if path.len() == 1 {
                array[*index] = new_value;
                return Ok(());
            }

            if array[*index].is_null() {
                array[*index] = container_for_segment(&path[1]);
            }
            set_value_at_path(&mut array[*index], &path[1..], new_value)
        }
    }
}

fn container_for_segment(segment: &PathSegment) -> Value {
    match segment {
        PathSegment::Key(_) => Value::Object(Map::new()),
        PathSegment::Index(_) => Value::Array(Vec::new()),
    }
}

async fn run_telegram(config: Arc<Config>) -> Result<()> {
    let bot = Bot::new(config.telegram.bot_token.clone());
    let me = bot.get_me().await?;
    let bot_name = me.user.username.unwrap_or_else(|| "mine-bot".to_string());
    bot.set_my_commands(TelegramCommand::bot_commands()).await?;

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let config = config.clone();
        let bot_name = bot_name.clone();
        async move {
            if let Some(text) = msg.text() {
                let user_id = msg
                    .from
                    .as_ref()
                    .map(|u| u.id.0.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let role = match authorize_user(config.as_ref(), "telegram", &user_id) {
                    Ok(role) if role.is_allowed() => role,
                    Ok(_) => {
                        bot.send_message(
                            msg.chat.id,
                            format!(
                                "You are not authorized to use this bot. Your Telegram user id is {user_id}."
                            ),
                        )
                        .await?;
                        return respond(());
                    }
                    Err(e) => {
                        bot.send_message(
                            msg.chat.id,
                            format!("Authorization check failed: {e:#}"),
                        )
                        .await?;
                        return respond(());
                    }
                };
                if let Ok(command) = TelegramCommand::parse(text, &bot_name) {
                    handle_telegram_command(&bot, &msg, config.as_ref(), &user_id, role, command)
                        .await?;
                } else if text.trim_start().starts_with('/') {
                    bot.send_message(
                        msg.chat.id,
                        "Unknown command. Use /help to see supported commands.",
                    )
                    .await?;
                } else {
                    handle_download_request(&bot, &msg, config.as_ref(), text).await?;
                }
            }
            respond(())
        }
    })
    .await;
    Ok(())
}

async fn handle_telegram_command(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    user_id: &str,
    _role: UserRole,
    command: TelegramCommand,
) -> ResponseResult<()> {
    match command {
        TelegramCommand::Start => {
            let text = concat!(
                "Send me a post URL and I will fetch the media for you.\n",
                "You can also use /download <url> explicitly.\n",
                "Use /help to see the available commands."
            );
            bot.send_message(msg.chat.id, text).await?;
        }
        TelegramCommand::Help => {
            bot.send_message(msg.chat.id, TelegramCommand::descriptions().to_string())
                .await?;
        }
        TelegramCommand::Id => {
            bot.send_message(msg.chat.id, format!("Your Telegram user id is {user_id}."))
                .await?;
        }
        TelegramCommand::Download(url) => {
            handle_download_request(bot, msg, config, &url).await?;
        }
    }

    Ok(())
}

async fn handle_download_request(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    input: &str,
) -> ResponseResult<()> {
    let download = download_media(config, input).await;
    match download {
        Ok(media) => send_download_result(bot, msg, media).await?,
        Err(e) => {
            error!(
                "download request failed for chat {} input {:?}: {e:#}",
                msg.chat.id.0,
                input
            );
            bot.send_message(msg.chat.id, format!("Download failed: {e:#}"))
                .await?;
        }
    }

    Ok(())
}

async fn send_download_result(
    bot: &Bot,
    msg: &Message,
    media: DownloadResponse,
) -> ResponseResult<()> {
    bot.send_message(msg.chat.id, format_post_text_html(&media.post_text))
        .parse_mode(ParseMode::Html)
        .link_preview_options(LinkPreviewOptions {
            is_disabled: true,
            url: None,
            prefer_small_media: false,
            prefer_large_media: false,
            show_above_text: false,
        })
        .await?;
    if media.files.len() == 1 {
        let file = &media.files[0];
        if is_image_path(file) {
            bot.send_photo(msg.chat.id, InputFile::file(PathBuf::from(file)))
                .await?;
        } else {
            bot.send_video(msg.chat.id, InputFile::file(PathBuf::from(file)))
                .await?;
        }
    } else {
        let photos: Vec<InputMedia> = media
            .files
            .iter()
            .map(|f| InputMedia::Photo(InputMediaPhoto::new(InputFile::file(PathBuf::from(f)))))
            .collect();
        if !photos.is_empty() {
            bot.send_media_group(msg.chat.id, photos).await?;
        }
    }

    Ok(())
}

fn format_post_text_html(text: &str) -> String {
    format!("<blockquote>{}</blockquote>", escape_html(text))
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

async fn download_media(
    config: &Config,
    input_url: &str,
) -> Result<DownloadResponse> {
    let normalized = Url::parse(input_url)?.to_string();

    let started = Instant::now();
    let item_dir = config.download_dir.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&item_dir)?;
    let out_tmpl = item_dir.join("%(title).100B-%(id)s.%(ext)s");

    let output = TokioCommand::new("yt-dlp")
        .arg("-f")
        .arg("bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/b")
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("--write-info-json")
        .arg("--write-description")
        .arg("--convert-thumbnails")
        .arg("jpg")
        .arg("--output")
        .arg(out_tmpl.as_str())
        .arg(&normalized)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(anyhow!("yt-dlp failed with status: {}", output.status));
        }
        return Err(anyhow!(
            "yt-dlp failed with status {}: {}",
            output.status,
            stderr
        ));
    }
    let download_ms = started.elapsed().as_millis() as i64;

    let normalize_started = Instant::now();
    let filename_metadata = read_delivery_filename_metadata(&item_dir);
    let files = collect_delivery_files(&item_dir, &filename_metadata).await?;
    let convert_ms = normalize_started.elapsed().as_millis() as i64;

    let description = read_sidecar_text(&item_dir).unwrap_or_else(|| "(no description)".into());

    let media_id = insert_media_record(
        &config,
        &normalized,
        &files.join(","),
        &description,
        download_ms,
        convert_ms,
    )?;

    Ok(DownloadResponse {
        media_id,
        normalized_url: normalized,
        post_text: description,
        files,
    })
}

fn is_image_path(path: &str) -> bool {
    path.ends_with(".jpg") || path.ends_with(".jpeg") || path.ends_with(".png")
}

async fn collect_delivery_files(
    item_dir: &Utf8PathBuf,
    filename_metadata: &DeliveryFilenameMetadata,
) -> Result<Vec<String>> {
    let mut video_files = Vec::new();
    let mut audio_files = Vec::new();
    let mut image_files = Vec::new();

    for entry in fs::read_dir(item_dir)? {
        let path = entry?.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("mp4") => video_files.push(path),
            Some("m4a") => audio_files.push(path),
            Some("jpg" | "jpeg" | "png") => image_files.push(path),
            _ => {}
        }
    }

    video_files.sort();
    audio_files.sort();
    image_files.sort();

    if let Some(video_path) = video_files.first() {
        let normalized = normalize_video_for_delivery(video_path, audio_files.first()).await?;
        let renamed = rename_delivery_file(&normalized, filename_metadata, None)?;
        return Ok(vec![renamed.to_string_lossy().to_string()]);
    }

    let total_images = image_files.len();
    let mut files = Vec::with_capacity(total_images);
    for (index, path) in image_files.into_iter().enumerate() {
        let suffix = (total_images > 1).then_some(index + 1);
        let renamed = rename_delivery_file(&path, filename_metadata, suffix)?;
        files.push(renamed.to_string_lossy().to_string());
    }
    if files.is_empty() {
        bail!("yt-dlp produced no deliverable media files");
    }
    Ok(files)
}

async fn normalize_video_for_delivery(video_path: &PathBuf, audio_path: Option<&PathBuf>) -> Result<PathBuf> {
    let output_path = video_path.with_file_name("delivery.mp4");
    let mut cmd = TokioCommand::new("ffmpeg");
    cmd.arg("-y")
        .arg("-nostdin")
        .arg("-i")
        .arg(video_path);

    if let Some(audio_path) = audio_path {
        cmd.arg("-i")
            .arg(audio_path)
            .arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("1:a:0")
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg("128k");
    } else {
        cmd.arg("-map").arg("0:v:0").arg("-an");
    }

    let output = cmd
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&output_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("ffmpeg failed with status {}", output.status);
        }
        bail!("ffmpeg failed with status {}: {}", output.status, stderr);
    }

    Ok(output_path)
}

fn read_sidecar_text(dir: &Utf8PathBuf) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "description"))
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
}

fn read_delivery_filename_metadata(dir: &Utf8PathBuf) -> DeliveryFilenameMetadata {
    let info = read_info_json(dir).unwrap_or(Value::Null);
    DeliveryFilenameMetadata {
        username: read_string_field(
            &info,
            &[
                "uploader",
                "channel",
                "creator",
                "artist",
                "uploader_id",
                "channel_id",
            ],
        )
        .unwrap_or_else(|| "unknown".into()),
        date: read_upload_date(&info).unwrap_or_else(current_delivery_date),
        title: read_string_field(&info, &["title", "fulltitle", "alt_title"])
            .unwrap_or_else(|| "untitled".into()),
    }
}

fn read_info_json(dir: &Utf8PathBuf) -> Option<Value> {
    let info_path = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json") && path.to_string_lossy().ends_with(".info.json"))?;
    let raw = fs::read_to_string(info_path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn read_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key)?.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn read_upload_date(value: &Value) -> Option<String> {
    for key in ["upload_date", "release_date"] {
        if let Some(date) = value.get(key).and_then(Value::as_str) {
            if let Some(formatted) = format_compact_date(date) {
                return Some(formatted);
            }
        }
    }

    if let Some(timestamp) = value.get("timestamp").and_then(Value::as_i64) {
        if let Some(date) = DateTime::<Utc>::from_timestamp(timestamp, 0) {
            return Some(date.format("%Y %m %d").to_string());
        }
    }

    None
}

fn format_compact_date(input: &str) -> Option<String> {
    if input.len() != 8 || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    Some(format!(
        "{} {} {}",
        &input[0..4],
        &input[4..6],
        &input[6..8]
    ))
}

fn current_delivery_date() -> String {
    Utc::now().format("%Y %m %d").to_string()
}

fn rename_delivery_file(
    path: &Path,
    metadata: &DeliveryFilenameMetadata,
    sequence: Option<usize>,
) -> Result<PathBuf> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .ok_or_else(|| anyhow!("downloaded media file has no extension: {}", path.display()))?;

    let suffix = sequence.map(|index| format!("_{index:02}")).unwrap_or_default();
    let max_base_len = MAX_DELIVERY_FILENAME_LEN
        .saturating_sub(extension.chars().count() + 1)
        .saturating_sub(suffix.chars().count());
    let base = build_delivery_basename(metadata, max_base_len);
    let filename = format!("{base}{suffix}.{extension}");
    let renamed = path.with_file_name(filename);

    if renamed != path {
        fs::rename(path, &renamed)?;
    }

    Ok(renamed)
}

fn build_delivery_basename(metadata: &DeliveryFilenameMetadata, max_len: usize) -> String {
    let username = sanitize_filename_part(&metadata.username, "unknown");
    let date = sanitize_filename_part(&metadata.date, "unknown");
    let title = sanitize_filename_part(&metadata.title, "untitled");

    let prefix = truncate_prefix_with_date(&username, &date, max_len);
    if prefix.chars().count() >= max_len || title.is_empty() {
        return prefix;
    }

    let title_budget = max_len.saturating_sub(prefix.chars().count() + 1);
    let title = truncate_to_chars(&title, title_budget);
    if title.is_empty() {
        prefix
    } else {
        format!("{prefix}_{title}")
    }
}

fn truncate_prefix_with_date(username: &str, date: &str, max_len: usize) -> String {
    let full_prefix = format!("{username}_{date}");
    if full_prefix.chars().count() <= max_len {
        return full_prefix;
    }

    let reserved_for_date = date.chars().count() + 1;
    if reserved_for_date < max_len {
        let username_budget = max_len - reserved_for_date;
        let username = truncate_to_chars(username, username_budget);
        if username.is_empty() {
            return truncate_to_chars(date, max_len);
        }
        return format!("{username}_{date}");
    }

    truncate_to_chars(date, max_len)
}

fn sanitize_filename_part(input: &str, fallback: &str) -> String {
    let normalized: String = input
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == ' ' {
                ch
            } else {
                ' '
            }
        })
        .collect();

    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join("_");
    if collapsed.is_empty() {
        fallback.to_string()
    } else {
        collapsed
    }
}

fn truncate_to_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

fn db_conn(config: &Config) -> Result<Connection> {
    Ok(Connection::open(&config.database_path)?)
}

fn init_db(config: &Config) -> Result<()> {
    let conn = db_conn(config)?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
          id INTEGER PRIMARY KEY,
          platform TEXT NOT NULL,
          platform_user_id TEXT NOT NULL,
          role TEXT NOT NULL CHECK(role IN ('admin','user','none')),
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          UNIQUE(platform, platform_user_id)
        );
        CREATE TABLE IF NOT EXISTS media (
          id INTEGER PRIMARY KEY,
          url TEXT NOT NULL,
          name TEXT NOT NULL,
          post_text TEXT,
          created_at TEXT NOT NULL,
          download_duration_ms INTEGER NOT NULL,
          convert_duration_ms INTEGER NOT NULL
        );
        ",
    )?;

    for u in &config.seed_users {
        upsert_user(config, &u.platform, &u.platform_user_id, u.role.clone())?;
    }
    Ok(())
}

fn upsert_user(
    config: &Config,
    platform: &str,
    platform_user_id: &str,
    role: UserRole,
) -> Result<()> {
    let conn = db_conn(config)?;
    let now: DateTime<Utc> = Utc::now();
    conn.execute(
        "INSERT INTO users(platform, platform_user_id, role, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(platform, platform_user_id)
         DO UPDATE SET role = excluded.role, updated_at = excluded.updated_at",
        params![platform, platform_user_id, role.as_str(), now.to_rfc3339()],
    )?;
    Ok(())
}

fn authorize_user(config: &Config, platform: &str, platform_user_id: &str) -> Result<UserRole> {
    let conn = db_conn(config)?;
    let now: DateTime<Utc> = Utc::now();
    conn.execute(
        "INSERT INTO users(platform, platform_user_id, role, created_at, updated_at)
         VALUES (?1, ?2, 'none', ?3, ?3)
         ON CONFLICT(platform, platform_user_id)
         DO UPDATE SET updated_at = excluded.updated_at",
        params![platform, platform_user_id, now.to_rfc3339()],
    )?;

    let role: String = conn.query_row(
        "SELECT role FROM users WHERE platform = ?1 AND platform_user_id = ?2",
        params![platform, platform_user_id],
        |row| row.get(0),
    )?;
    UserRole::from_str(&role)
}

fn insert_media_record(
    config: &Config,
    url: &str,
    name: &str,
    post_text: &str,
    download_ms: i64,
    convert_ms: i64,
) -> Result<i64> {
    let conn = db_conn(config)?;
    let now: DateTime<Utc> = Utc::now();
    task::block_in_place(|| {
        conn.execute(
            "INSERT INTO media(url, name, post_text, created_at, download_duration_ms, convert_duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![url, name, post_text, now.to_rfc3339(), download_ms, convert_ms],
        )
        .map_err(anyhow::Error::from)?;
        Ok(conn.last_insert_rowid())
    })
}
