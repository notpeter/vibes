use std::{
    fs,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OptionalExtension, params};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use teloxide::{
    ApiError, RequestError,
    prelude::*,
    types::{
        BotCommand, BotCommandScope, ChatId, InputFile, InputMedia, InputMediaPhoto,
        LinkPreviewOptions, MessageId, ParseMode,
    },
    utils::command::BotCommands,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command as TokioCommand,
    task,
};
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;
use which::which;

#[derive(Debug, Parser)]
#[command(name = "notnotsavebot")]
struct Cli {
    #[arg(short = 'c', long = "config", default_value = "config.conl")]
    config: Utf8PathBuf,

    #[command(subcommand)]
    command: Option<CliCommand>,
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

#[derive(Debug, Clone, Copy)]
struct TelegramVideoMetadata {
    width: u32,
    height: u32,
    duration_seconds: u32,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetAudioPlan {
    copy: bool,
    reserved_bitrate_bps: u64,
}

#[derive(Debug, Clone, Copy)]
struct VideoReencodeInfo {
    duration_seconds: f64,
    audio: TargetAudioPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StatusPhase {
    FetchingLink,
    FetchFailed,
    DownloadingMedia,
    DownloadFailed,
    ConvertingMedia,
    ConvertingFailed,
    CompressingMediaPass1,
    CompressingMediaPass2,
    CompressingFailed,
    UploadingMedia,
    UploadFailed,
    DownloadComplete,
}

impl StatusPhase {
    fn label(self) -> &'static str {
        match self {
            Self::FetchingLink => "Fetching link...",
            Self::FetchFailed => "Fetch failed.",
            Self::DownloadingMedia => "Downloading media...",
            Self::DownloadFailed => "Download failed.",
            Self::ConvertingMedia => "Converting media...",
            Self::ConvertingFailed => "Conversion failed.",
            Self::CompressingMediaPass1 => "Compressing media... Pass 1/2",
            Self::CompressingMediaPass2 => "Compressing media... Pass 2/2",
            Self::CompressingFailed => "Compression failed.",
            Self::UploadingMedia => "Uploading media...",
            Self::UploadFailed => "Upload failed.",
            Self::DownloadComplete => "Download complete.",
        }
    }

    fn is_failure(self) -> bool {
        matches!(
            self,
            Self::FetchFailed
                | Self::DownloadFailed
                | Self::ConvertingFailed
                | Self::CompressingFailed
                | Self::UploadFailed
        )
    }

    fn progress_text(self, bucket: u8, display_percent: u8) -> String {
        format!(
            "{}\n<code>{}</code>",
            self.label(),
            progress_bar(bucket, display_percent)
        )
    }
}

struct ProgressThrottle {
    phase: StatusPhase,
    last_update: Option<Instant>,
    last_display_percent: Option<u8>,
    pending_display_percent: Option<u8>,
}

impl ProgressThrottle {
    fn after_sent(phase: StatusPhase, display_percent: u8) -> Self {
        Self {
            phase,
            last_update: Some(Instant::now()),
            last_display_percent: Some(display_percent),
            pending_display_percent: None,
        }
    }

    async fn update(&mut self, progress: Option<&DownloadProgress<'_>>, percent: f64) {
        let Some(progress) = progress else {
            return;
        };
        let Some(display_percent) = progress_display_percent(percent) else {
            return;
        };
        if self.last_display_percent == Some(display_percent) {
            return;
        }
        if self
            .last_update
            .is_some_and(|last_update| last_update.elapsed() < PROGRESS_UPDATE_INTERVAL)
        {
            self.pending_display_percent = Some(display_percent);
            return;
        }

        self.send_percent(progress, display_percent).await;
    }

    async fn flush(&mut self, progress: Option<&DownloadProgress<'_>>) {
        let Some(progress) = progress else {
            return;
        };
        if let Some(display_percent) = self.pending_display_percent.take()
            && self.last_display_percent != Some(display_percent)
        {
            self.send_percent(progress, display_percent).await;
        }
    }

    async fn send_percent(&mut self, progress: &DownloadProgress<'_>, display_percent: u8) {
        self.last_update = Some(Instant::now());
        self.last_display_percent = Some(display_percent);
        self.pending_display_percent = None;
        progress.set_phase_bar(self.phase, display_percent).await;
    }
}

struct DownloadProgress<'a> {
    bot: &'a Bot,
    chat_id: ChatId,
    message_id: MessageId,
    phase: Mutex<Option<StatusPhase>>,
}

impl<'a> DownloadProgress<'a> {
    fn new(bot: &'a Bot, message: &Message) -> Self {
        Self {
            bot,
            chat_id: message.chat.id,
            message_id: message.id,
            phase: Mutex::new(None),
        }
    }

    async fn set_phase(&self, phase: StatusPhase) {
        self.record_phase(phase);
        self.set(phase.label()).await;
    }

    async fn set_phase_bar(&self, phase: StatusPhase, display_percent: u8) {
        self.record_phase(phase);
        let bucket = progress_bucket_from_display_percent(display_percent);
        self.set(&phase.progress_text(bucket, display_percent))
            .await;
    }

    fn has_failure_phase(&self) -> bool {
        self.phase
            .lock()
            .ok()
            .and_then(|phase| *phase)
            .is_some_and(StatusPhase::is_failure)
    }

    fn record_phase(&self, phase: StatusPhase) {
        if let Ok(mut current) = self.phase.lock() {
            *current = Some(phase);
        }
    }

    async fn set(&self, text: &str) {
        if let Err(err) = self
            .bot
            .edit_message_text(self.chat_id, self.message_id, text)
            .parse_mode(ParseMode::Html)
            .await
        {
            warn!(
                "failed to update Telegram status message chat_id={} message_id={}: {}",
                self.chat_id.0, self.message_id.0, err
            );
        }
    }

    async fn delete(&self) {
        if let Err(err) = self.bot.delete_message(self.chat_id, self.message_id).await {
            warn!(
                "failed to delete Telegram status message chat_id={} message_id={}: {}",
                self.chat_id.0, self.message_id.0, err
            );
        }
    }
}

async fn set_download_progress(progress: Option<&DownloadProgress<'_>>, phase: StatusPhase) {
    if let Some(progress) = progress {
        progress.set_phase(phase).await;
    }
}

async fn set_download_progress_bar(
    progress: Option<&DownloadProgress<'_>>,
    phase: StatusPhase,
    display_percent: u8,
) {
    if let Some(progress) = progress {
        progress.set_phase_bar(phase, display_percent).await;
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct DownloadResponse {
    media_id: i64,
    normalized_url: String,
    post_platform: String,
    post_title: Option<String>,
    post_source: String,
    post_date: String,
    post_body: String,
    post_text: String,
    files: Vec<String>,
}

#[derive(Debug, Clone)]
struct DeliveryFilenameMetadata {
    filename_username: String,
    platform_name: String,
    source_name: String,
    source_handle: Option<String>,
    date: String,
    title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TwitterPostMetadata {
    source_name: String,
    source_handle: Option<String>,
    timestamp: Option<i64>,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstagramPostMetadata {
    source_name: String,
    source_handle: Option<String>,
    timestamp: Option<i64>,
    title: String,
    caption: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlueskyPostRef {
    handle: String,
    rkey: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlueskyPostMetadata {
    source_name: String,
    source_handle: Option<String>,
    timestamp: Option<i64>,
    title: String,
    text: String,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Available commands:")]
enum AdminTelegramCommand {
    #[command(description = "show a short introduction")]
    Start,
    #[command(description = "show this help message")]
    Help,
    #[command(description = "show your Telegram user id")]
    Id,
    #[command(description = "download media from a URL")]
    Download(String),
    #[command(description = "authorize a Telegram user id or username as a user")]
    Authorize(String),
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Available commands:")]
enum UserTelegramCommand {
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
    Utf8PathBuf::from("./notnotsavebot.sqlite")
}

fn default_download_dir() -> Utf8PathBuf {
    Utf8PathBuf::from("./downloads")
}

const MAX_DELIVERY_FILENAME_LEN: usize = 79;
const TELEGRAM_UPLOAD_LIMIT_BYTES: u64 = 50_000_000;
const TELEGRAM_REENCODE_TARGET_BYTES: u64 = 48_000_000;
const TELEGRAM_REENCODE_AUDIO_BITRATE_BPS: u64 = 128_000;
const TELEGRAM_COPY_AUDIO_MAX_BITRATE_BPS: u64 = 131_000;
const TELEGRAM_REENCODE_CONTAINER_OVERHEAD_BPS: u64 = 64_000;
const TELEGRAM_REENCODE_MIN_VIDEO_BITRATE_BPS: u64 = 150_000;
const TELEGRAM_UPLOAD_LIMIT_LABEL: &str = "50 MB";
const PROGRESS_BAR_WIDTH: usize = 20;
const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
enum AuthorizationTarget {
    UserId(String),
    Username(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(CliCommand::Run) {
        CliCommand::Run => {
            let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stdout)
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
    run_telegram(config).await
}

fn apply_env_overrides(config: &mut Config) {
    if let Ok(bot_token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        config.telegram.bot_token = bot_token;
    }
}

fn ensure_runtime_tools() -> Result<()> {
    let yt_dlp = which("yt-dlp").context(
        "required tool `yt-dlp` was not found on PATH; install it or fix the bot service PATH",
    )?;
    info!("found yt-dlp at {}", yt_dlp.display());

    let ffmpeg = which("ffmpeg").context(
        "required tool `ffmpeg` was not found on PATH; install it or fix the bot service PATH",
    )?;
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
    let bot_name = me
        .user
        .username
        .unwrap_or_else(|| "notnotsavebot".to_string());
    bot.set_my_commands(Vec::<BotCommand>::new()).await?;

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let config = config.clone();
        let bot_name = bot_name.clone();
        async move {
            if let Some(text) = msg.text() {
                let Some(from) = msg.from.as_ref() else {
                    return respond(());
                };
                let user_id = from.id.0.to_string();
                let username = normalize_telegram_username(from.username.as_deref());
                info!(
                    "telegram message received chat_id={} message_id={} user_id={} username={} text={:?}",
                    msg.chat.id.0,
                    msg.id.0,
                    user_id,
                    username.as_deref().unwrap_or("-"),
                    preview_log_text(text)
                );
                let role = match authorize_user(
                    config.as_ref(),
                    "telegram",
                    &user_id,
                    username.as_deref(),
                ) {
                    Ok(role) => role,
                    Err(e) => {
                        bot.send_message(msg.chat.id, format!("Authorization check failed: {e:#}"))
                            .await?;
                        return respond(());
                    }
                };
                sync_commands_for_message(&bot, &msg, role).await?;

                match role {
                    Some(UserRole::Admin) => {
                        if is_bare_command_invocation(text, "authorize", &bot_name) {
                            send_authorize_prompt(&bot, &msg).await?;
                        } else if is_bare_command_invocation(text, "download", &bot_name) {
                            send_download_prompt(&bot, &msg).await?;
                        } else if let Ok(command) = AdminTelegramCommand::parse(text, &bot_name) {
                            handle_admin_telegram_command(
                                &bot,
                                &msg,
                                config.as_ref(),
                                &user_id,
                                command,
                            )
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
                    Some(UserRole::User) => {
                        if is_bare_command_invocation(text, "download", &bot_name) {
                            send_download_prompt(&bot, &msg).await?;
                        } else if let Ok(command) = UserTelegramCommand::parse(text, &bot_name) {
                            handle_user_telegram_command(
                                &bot,
                                &msg,
                                config.as_ref(),
                                &user_id,
                                command,
                            )
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
                    Some(UserRole::None) | None => {}
                }
            }
            respond(())
        }
    })
    .await;
    Ok(())
}

async fn sync_commands_for_message(
    bot: &Bot,
    msg: &Message,
    role: Option<UserRole>,
) -> ResponseResult<()> {
    let commands = command_set_for_role(role);

    if let Some(scope) = command_scope_for_message(msg) {
        bot.set_my_commands(commands).scope(scope).await?;
    }

    Ok(())
}

fn command_set_for_role(role: Option<UserRole>) -> Vec<BotCommand> {
    match role {
        Some(UserRole::Admin) => AdminTelegramCommand::bot_commands(),
        Some(UserRole::User) => UserTelegramCommand::bot_commands(),
        Some(UserRole::None) | None => Vec::new(),
    }
}

fn command_scope_for_message(msg: &Message) -> Option<BotCommandScope> {
    if msg.chat.is_private() {
        return Some(BotCommandScope::Chat {
            chat_id: msg.chat.id.into(),
        });
    }

    let user_id = msg.from.as_ref()?.id;
    Some(BotCommandScope::ChatMember {
        chat_id: msg.chat.id.into(),
        user_id,
    })
}

async fn handle_admin_telegram_command(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    user_id: &str,
    command: AdminTelegramCommand,
) -> ResponseResult<()> {
    match command {
        AdminTelegramCommand::Start => {
            let text = concat!(
                "Send me a post URL and I will fetch the media for you.\n",
                "You can also use /download <url> explicitly.\n",
                "Use /help to see the available commands."
            );
            bot.send_message(msg.chat.id, text).await?;
        }
        AdminTelegramCommand::Help => {
            bot.send_message(
                msg.chat.id,
                AdminTelegramCommand::descriptions().to_string(),
            )
            .await?;
        }
        AdminTelegramCommand::Id => {
            bot.send_message(msg.chat.id, format!("Your Telegram user id is {user_id}."))
                .await?;
        }
        AdminTelegramCommand::Download(url) => {
            handle_download_request(bot, msg, config, &url).await?;
        }
        AdminTelegramCommand::Authorize(target) => {
            handle_authorize_request(bot, msg, config, &target).await?;
        }
    }

    Ok(())
}

async fn handle_user_telegram_command(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    user_id: &str,
    command: UserTelegramCommand,
) -> ResponseResult<()> {
    match command {
        UserTelegramCommand::Start => {
            let text = concat!(
                "Send me a post URL and I will fetch the media for you.\n",
                "You can also use /download <url> explicitly.\n",
                "Use /help to see the available commands."
            );
            bot.send_message(msg.chat.id, text).await?;
        }
        UserTelegramCommand::Help => {
            bot.send_message(msg.chat.id, UserTelegramCommand::descriptions().to_string())
                .await?;
        }
        UserTelegramCommand::Id => {
            bot.send_message(msg.chat.id, format!("Your Telegram user id is {user_id}."))
                .await?;
        }
        UserTelegramCommand::Download(url) => {
            handle_download_request(bot, msg, config, &url).await?;
        }
    }

    Ok(())
}

async fn send_authorize_prompt(bot: &Bot, msg: &Message) -> ResponseResult<()> {
    let text = concat!(
        "Authorize a Telegram user as role=user.\n",
        "Use /authorize 123456789 to authorize by numeric Telegram user id.\n",
        "Use /authorize @username to authorize by username before they message the bot."
    );
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

async fn send_download_prompt(bot: &Bot, msg: &Message) -> ResponseResult<()> {
    bot.send_message(
        msg.chat.id,
        "Send /download <url> or paste a post URL directly.",
    )
    .await?;
    Ok(())
}

async fn handle_authorize_request(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    target: &str,
) -> ResponseResult<()> {
    let target = match parse_authorization_target(target) {
        Ok(target) => target,
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Invalid /authorize target: {e:#}"))
                .await?;
            return Ok(());
        }
    };

    match authorize_telegram_target(config, &target) {
        Ok(summary) => {
            bot.send_message(msg.chat.id, summary).await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Authorize failed: {e:#}"))
                .await?;
        }
    }

    Ok(())
}

fn is_bare_command_invocation(text: &str, command_name: &str, bot_name: &str) -> bool {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.splitn(2, char::is_whitespace);
    let token = parts.next().unwrap_or_default();
    let tail = parts.next().unwrap_or_default().trim();
    if !tail.is_empty() {
        return false;
    }

    token.eq_ignore_ascii_case(command_name)
        || token.eq_ignore_ascii_case(&format!("{command_name}@{bot_name}"))
}

fn parse_authorization_target(input: &str) -> Result<AuthorizationTarget> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("missing authorization target");
    }

    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(AuthorizationTarget::UserId(trimmed.to_string()));
    }

    let username = normalize_telegram_username(Some(trimmed))
        .ok_or_else(|| anyhow!("target must be a Telegram numeric user id or username"))?;
    Ok(AuthorizationTarget::Username(username))
}

fn normalize_telegram_username(input: Option<&str>) -> Option<String> {
    let trimmed = input?.trim();
    let trimmed = trimmed.strip_prefix('@').unwrap_or(trimmed).trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

fn authorize_telegram_target(config: &Config, target: &AuthorizationTarget) -> Result<String> {
    match target {
        AuthorizationTarget::UserId(user_id) => {
            upsert_user(config, "telegram", Some(user_id), None, UserRole::User)?;
            Ok(format!("Authorized Telegram user id {user_id} as user."))
        }
        AuthorizationTarget::Username(username) => {
            upsert_user(config, "telegram", None, Some(username), UserRole::User)?;
            Ok(format!("Authorized Telegram username @{username} as user."))
        }
    }
}

async fn handle_download_request(
    bot: &Bot,
    msg: &Message,
    config: &Config,
    input: &str,
) -> ResponseResult<()> {
    let status = bot
        .send_message(msg.chat.id, StatusPhase::FetchingLink.label())
        .await?;
    let progress = DownloadProgress::new(bot, &status);

    let download = download_media(config, input, Some(&progress)).await;
    match download {
        Ok(media) => {
            progress.set_phase(StatusPhase::DownloadComplete).await;
            if let Err(e) = send_download_result(bot, msg, media, Some(&progress)).await {
                error!(
                    "download delivery failed for chat {} input {:?}: {e:#}",
                    msg.chat.id.0, input
                );
                progress.set_phase(StatusPhase::UploadFailed).await;
                if let Err(notify_error) = send_delivery_error_notice(bot, msg, &e).await {
                    error!(
                        "failed to notify chat {} after delivery error: {notify_error:#}",
                        msg.chat.id.0
                    );
                    return Err(notify_error);
                }
            }
            progress.delete().await;
        }
        Err(e) => {
            error!(
                "download request failed for chat {} input {:?}: {e:#}",
                msg.chat.id.0, input
            );
            if !progress.has_failure_phase() {
                progress.set_phase(StatusPhase::DownloadFailed).await;
            }
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
    progress: Option<&DownloadProgress<'_>>,
) -> ResponseResult<()> {
    bot.send_message(
        msg.chat.id,
        format_post_text_html(
            &media.post_platform,
            media.post_title.as_deref(),
            &media.post_source,
            &media.post_date,
            &media.post_body,
        ),
    )
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
        if let Some(size) = oversized_telegram_file_size(file)? {
            warn!(
                "skipping Telegram upload larger than {} chat_id={} file={} size_bytes={}",
                TELEGRAM_UPLOAD_LIMIT_LABEL, msg.chat.id.0, file, size
            );
            send_file_too_large_notice(bot, msg, size).await?;
            return Ok(());
        }

        if is_image_path(file) {
            set_download_progress(progress, StatusPhase::UploadingMedia).await;
            bot.send_photo(msg.chat.id, InputFile::file(PathBuf::from(file)))
                .await?;
        } else {
            set_download_progress(progress, StatusPhase::UploadingMedia).await;
            let mut request = bot.send_video(msg.chat.id, InputFile::file(PathBuf::from(file)));
            request = request.supports_streaming(true);
            if let Some(metadata) = probe_telegram_video_metadata(Path::new(file)).await {
                request = request
                    .width(metadata.width)
                    .height(metadata.height)
                    .duration(metadata.duration_seconds);
            }
            request.await?;
            info!(
                "telegram processed video sent chat_id={} message_id={} file={}",
                msg.chat.id.0, msg.id.0, file
            );
        }
    } else {
        let mut deliverable_files = Vec::new();
        let mut skipped_files = 0;
        let mut largest_skipped_size = 0;
        for file in &media.files {
            if let Some(size) = oversized_telegram_file_size(file)? {
                skipped_files += 1;
                largest_skipped_size = largest_skipped_size.max(size);
                warn!(
                    "skipping Telegram upload larger than {} chat_id={} file={} size_bytes={}",
                    TELEGRAM_UPLOAD_LIMIT_LABEL, msg.chat.id.0, file, size
                );
                continue;
            }

            deliverable_files.push(file.as_str());
        }

        match deliverable_files.as_slice() {
            [] => {}
            [photo] => {
                set_download_progress(progress, StatusPhase::UploadingMedia).await;
                bot.send_photo(msg.chat.id, InputFile::file(PathBuf::from(*photo)))
                    .await?;
            }
            photos => {
                let photos: Vec<InputMedia> = photos
                    .iter()
                    .map(|file| {
                        InputMedia::Photo(InputMediaPhoto::new(InputFile::file(PathBuf::from(
                            *file,
                        ))))
                    })
                    .collect();
                set_download_progress(progress, StatusPhase::UploadingMedia).await;
                bot.send_media_group(msg.chat.id, photos).await?;
            }
        }
        if skipped_files > 0 {
            send_files_too_large_notice(bot, msg, skipped_files, largest_skipped_size).await?;
        }
    }

    Ok(())
}

async fn send_delivery_error_notice(
    bot: &Bot,
    msg: &Message,
    error: &RequestError,
) -> ResponseResult<()> {
    let text = if is_request_entity_too_large(error) {
        format!(
            "Something went wrong while sending the file: Telegram rejected it as larger than the {TELEGRAM_UPLOAD_LIMIT_LABEL} bot upload limit."
        )
    } else {
        "Something went wrong while sending the downloaded media. Please try again later."
            .to_string()
    };
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

fn is_request_entity_too_large(error: &RequestError) -> bool {
    matches!(error, RequestError::Api(api) if *api == ApiError::RequestEntityTooLarge)
}

fn oversized_telegram_file_size(path: &str) -> std::io::Result<Option<u64>> {
    let size = fs::metadata(path)?.len();
    Ok((size > TELEGRAM_UPLOAD_LIMIT_BYTES).then_some(size))
}

async fn send_file_too_large_notice(bot: &Bot, msg: &Message, size: u64) -> ResponseResult<()> {
    bot.send_message(
        msg.chat.id,
        format!(
            "The downloaded file is {}, which is over Telegram's {TELEGRAM_UPLOAD_LIMIT_LABEL} bot upload limit, so I did not send it.",
            format_file_size(size)
        ),
    )
    .await?;
    Ok(())
}

async fn send_files_too_large_notice(
    bot: &Bot,
    msg: &Message,
    skipped_files: usize,
    largest_size: u64,
) -> ResponseResult<()> {
    let noun = if skipped_files == 1 {
        "file was"
    } else {
        "files were"
    };
    bot.send_message(
        msg.chat.id,
        format!(
            "{skipped_files} downloaded {noun} over Telegram's {TELEGRAM_UPLOAD_LIMIT_LABEL} bot upload limit and were not sent. The largest was {}.",
            format_file_size(largest_size)
        ),
    )
    .await?;
    Ok(())
}

fn format_file_size(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

fn preview_log_text(input: &str) -> String {
    const MAX_CHARS: usize = 120;
    let mut preview = String::new();
    for (index, ch) in input.chars().enumerate() {
        if index >= MAX_CHARS {
            preview.push_str("...");
            return preview;
        }
        preview.push(ch);
    }
    preview
}

fn format_post_text_html(
    platform: &str,
    title: Option<&str>,
    source: &str,
    date: &str,
    body: &str,
) -> String {
    let mut out = String::new();
    if !platform.trim().is_empty() {
        out.push('(');
        out.push_str(&escape_html(platform));
        out.push_str(")\n");
    }
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        out.push_str("<b>");
        out.push_str(&escape_html(title));
        out.push_str("</b>\n");
    }
    out.push_str(&escape_html(source));
    if !date.is_empty() {
        out.push('\n');
        out.push_str(&escape_html(date));
    }
    if !body.trim().is_empty() {
        out.push('\n');
        out.push_str("<blockquote expandable>");
        out.push_str(&escape_html(body));
        out.push_str("</blockquote>");
    }
    out
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

fn progress_display_percent(percent: f64) -> Option<u8> {
    if !percent.is_finite() {
        return None;
    }
    Some(percent.clamp(0.0, 100.0).floor() as u8)
}

fn progress_bucket_from_display_percent(display_percent: u8) -> u8 {
    ((display_percent.min(100) / 5) as u8).min(20)
}

fn progress_bar(bucket: u8, display_percent: u8) -> String {
    let filled = usize::from(bucket).min(PROGRESS_BAR_WIDTH);
    let empty = PROGRESS_BAR_WIDTH - filled;
    let display_percent = display_percent.min(100);
    format!(
        "[{}{}] {display_percent:>3}%",
        "#".repeat(filled),
        "-".repeat(empty)
    )
}

fn normalize_input_url(input_url: &str) -> Result<String> {
    let trimmed = input_url.trim();
    let mut url = Url::parse(trimmed).or_else(|err| {
        if trimmed.contains("://") {
            return Err(err);
        }
        Url::parse(&format!("https://{trimmed}"))
    })?;
    if let Some(host) = url.host_str() {
        let host = host.to_ascii_lowercase();
        if host == "x.com"
            || host == "www.x.com"
            || host == "twitter.com"
            || host == "www.twitter.com"
        {
            url.set_query(None);
        }
    }
    Ok(url.to_string())
}

async fn download_media(
    config: &Config,
    input_url: &str,
    progress: Option<&DownloadProgress<'_>>,
) -> Result<DownloadResponse> {
    let normalized = match normalize_input_url(input_url) {
        Ok(normalized) => normalized,
        Err(err) => {
            set_download_progress(progress, StatusPhase::FetchFailed).await;
            return Err(err);
        }
    };

    let started = Instant::now();
    let item_dir = config.download_dir.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&item_dir)?;
    let out_tmpl = item_dir.join("%(title).100B-%(id)s.%(ext)s");

    set_download_progress_bar(progress, StatusPhase::DownloadingMedia, 0).await;
    let output = match run_yt_dlp_download(&normalized, &out_tmpl, progress).await {
        Ok(output) => output,
        Err(err) => {
            set_download_progress(progress, StatusPhase::DownloadFailed).await;
            return Err(err);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if should_continue_with_instagram_embed_fallback(&normalized, &stderr) {
            warn!(
                "yt-dlp could not download Instagram post directly; trying embed image fallback: {}",
                stderr
            );
        } else if should_continue_with_twitter_image_fallback(&normalized, &stderr) {
            warn!(
                "yt-dlp could not download Twitter/X post directly; trying image fallback: {}",
                stderr
            );
        } else if should_continue_with_bluesky_image_fallback(&normalized, &stderr) {
            warn!(
                "yt-dlp could not download Bluesky post directly; trying image fallback: {}",
                stderr
            );
        } else if stderr.is_empty() {
            set_download_progress(progress, StatusPhase::DownloadFailed).await;
            return Err(anyhow!("yt-dlp failed with status: {}", output.status));
        } else {
            set_download_progress(progress, StatusPhase::DownloadFailed).await;
            return Err(anyhow!(
                "yt-dlp failed with status {}: {}",
                output.status,
                stderr
            ));
        }
    }
    let download_ms = started.elapsed().as_millis() as i64;

    let normalize_started = Instant::now();
    if let Err(err) = download_instagram_embed_images_if_needed(&normalized, &item_dir).await {
        set_download_progress(progress, StatusPhase::FetchFailed).await;
        return Err(err);
    }
    if let Err(err) = download_twitter_images_if_needed(&normalized, &item_dir).await {
        set_download_progress(progress, StatusPhase::FetchFailed).await;
        return Err(err);
    }
    if let Err(err) = download_bluesky_images_if_needed(&normalized, &item_dir).await {
        set_download_progress(progress, StatusPhase::FetchFailed).await;
        return Err(err);
    }
    let filename_metadata = read_delivery_filename_metadata(&item_dir);
    let files = match collect_delivery_files(&item_dir, &filename_metadata, progress).await {
        Ok(files) => files,
        Err(err) => {
            set_download_progress(progress, StatusPhase::ConvertingFailed).await;
            return Err(err);
        }
    };
    let convert_ms = normalize_started.elapsed().as_millis() as i64;

    let description = read_sidecar_text(&item_dir).unwrap_or_else(|| "(no description)".into());
    let post_platform = filename_metadata.platform_name.clone();
    let post_title = display_post_title(&filename_metadata);
    let post_source = format_post_source_line(&filename_metadata);
    let post_date = filename_metadata.date.clone();
    let post_text = compose_post_text(
        &post_platform,
        post_title.as_deref(),
        &post_source,
        &post_date,
        &description,
    );

    let media_id = insert_media_record(
        config,
        &normalized,
        &files.join(","),
        &post_text,
        download_ms,
        convert_ms,
    )?;

    Ok(DownloadResponse {
        media_id,
        normalized_url: normalized,
        post_platform,
        post_title,
        post_source,
        post_date,
        post_body: description,
        post_text,
        files,
    })
}

fn is_image_path(path: &str) -> bool {
    path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".png")
        || path.ends_with(".webp")
}

async fn download_instagram_embed_images_if_needed(
    normalized: &str,
    item_dir: &Utf8PathBuf,
) -> Result<()> {
    if item_dir_has_deliverable_files(item_dir)? {
        return Ok(());
    }

    let Some(shortcode) = instagram_shortcode_from_url(normalized)? else {
        return Ok(());
    };

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 notnotsavebot/0.1")
        .build()?;
    let graphql_media = fetch_instagram_graphql_media(&client, &shortcode).await?;
    if let Some(media) = &graphql_media
        && let Some(metadata) = extract_instagram_metadata(media)
    {
        write_instagram_metadata_sidecars(item_dir, &shortcode, normalized, &metadata)?;
    }
    let mut image_urls = graphql_media
        .as_ref()
        .map(extract_instagram_graphql_image_urls)
        .unwrap_or_default();

    if image_urls.is_empty()
        && let Some(embed_url) = instagram_embed_url(normalized)?
    {
        let html = client
            .get(embed_url.as_str())
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        image_urls = extract_instagram_sidecar_image_urls(&html);
    }

    if image_urls.is_empty() {
        bail!("Instagram post did not expose uncropped image URLs");
    }

    for (index, image_url) in image_urls.iter().enumerate() {
        let response = client
            .get(image_url)
            .header(reqwest::header::REFERER, normalized)
            .send()
            .await?
            .error_for_status()?;
        if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE)
            && !content_type
                .to_str()
                .unwrap_or_default()
                .starts_with("image/")
        {
            bail!(
                "Instagram image URL returned non-image content type: {}",
                content_type.to_str().unwrap_or("<invalid content type>")
            );
        }
        let extension = image_extension_from_url(image_url);
        let output_path = item_dir.join(format!("instagram-sidecar-{:02}.{extension}", index + 1));
        fs::write(output_path, response.bytes().await?)?;
    }

    Ok(())
}

fn should_continue_with_instagram_embed_fallback(normalized: &str, stderr: &str) -> bool {
    should_retry_without_video_format(stderr)
        && instagram_shortcode_from_url(normalized)
            .ok()
            .flatten()
            .is_some()
}

async fn download_twitter_images_if_needed(normalized: &str, item_dir: &Utf8PathBuf) -> Result<()> {
    if item_dir_has_deliverable_files(item_dir)? {
        return Ok(());
    }

    let Some(status_id) = twitter_status_id_from_url(normalized)? else {
        return Ok(());
    };

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 notnotsavebot/0.1")
        .build()?;
    let html = client
        .get(normalized)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    if let Some(metadata) = extract_twitter_metadata(&html, &status_id) {
        write_twitter_metadata_sidecars(item_dir, &status_id, normalized, &metadata)?;
    }

    let image_urls = extract_twitter_image_urls(&html);
    if image_urls.is_empty() {
        bail!("Twitter/X post did not expose image URLs");
    }

    for (index, image_url) in image_urls.iter().enumerate() {
        let response = client
            .get(image_url)
            .header(reqwest::header::REFERER, normalized)
            .send()
            .await?
            .error_for_status()?;
        if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE)
            && !content_type
                .to_str()
                .unwrap_or_default()
                .starts_with("image/")
        {
            bail!(
                "Twitter/X image URL returned non-image content type: {}",
                content_type.to_str().unwrap_or("<invalid content type>")
            );
        }
        let extension = image_extension_from_url(image_url);
        let output_path = item_dir.join(format!("twitter-image-{:02}.{extension}", index + 1));
        fs::write(output_path, response.bytes().await?)?;
    }

    Ok(())
}

fn should_continue_with_twitter_image_fallback(normalized: &str, stderr: &str) -> bool {
    stderr.contains("No video could be found in this tweet")
        && twitter_status_id_from_url(normalized)
            .ok()
            .flatten()
            .is_some()
}

async fn download_bluesky_images_if_needed(normalized: &str, item_dir: &Utf8PathBuf) -> Result<()> {
    if item_dir_has_deliverable_files(item_dir)? {
        return Ok(());
    }

    let Some(post_ref) = bluesky_post_ref_from_url(normalized)? else {
        return Ok(());
    };

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 notnotsavebot/0.1")
        .build()?;
    let did = resolve_bluesky_handle(&client, &post_ref.handle).await?;
    let post_uri = format!("at://{did}/app.bsky.feed.post/{}", post_ref.rkey);
    let post = fetch_bluesky_post(&client, &post_uri).await?;

    if let Some(metadata) = extract_bluesky_metadata(&post) {
        write_bluesky_metadata_sidecars(item_dir, &post_ref.rkey, normalized, &metadata)?;
    }

    let image_urls = extract_bluesky_image_urls(&post);
    if image_urls.is_empty() {
        bail!("Bluesky post did not expose image URLs");
    }

    for (index, image_url) in image_urls.iter().enumerate() {
        let response = client
            .get(image_url)
            .header(reqwest::header::REFERER, normalized)
            .send()
            .await?
            .error_for_status()?;
        if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE)
            && !content_type
                .to_str()
                .unwrap_or_default()
                .starts_with("image/")
        {
            bail!(
                "Bluesky image URL returned non-image content type: {}",
                content_type.to_str().unwrap_or("<invalid content type>")
            );
        }
        let extension = image_extension_from_url(image_url);
        let output_path = item_dir.join(format!("bluesky-image-{:02}.{extension}", index + 1));
        fs::write(output_path, response.bytes().await?)?;
    }

    Ok(())
}

fn should_continue_with_bluesky_image_fallback(normalized: &str, stderr: &str) -> bool {
    stderr.contains("No video could be found in this post")
        && bluesky_post_ref_from_url(normalized)
            .ok()
            .flatten()
            .is_some()
}

fn bluesky_post_ref_from_url(normalized: &str) -> Result<Option<BlueskyPostRef>> {
    let url = Url::parse(normalized)?;
    let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) else {
        return Ok(None);
    };
    if host != "bsky.app" && host != "www.bsky.app" {
        return Ok(None);
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    match segments.as_slice() {
        ["profile", handle, "post", rkey, ..] if !handle.is_empty() && !rkey.is_empty() => {
            Ok(Some(BlueskyPostRef {
                handle: (*handle).to_string(),
                rkey: (*rkey).to_string(),
            }))
        }
        _ => Ok(None),
    }
}

async fn resolve_bluesky_handle(client: &reqwest::Client, handle: &str) -> Result<String> {
    let mut url =
        Url::parse("https://public.api.bsky.app/xrpc/com.atproto.identity.resolveHandle")?;
    url.query_pairs_mut().append_pair("handle", handle);

    let payload: Value = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    payload
        .get("did")
        .and_then(Value::as_str)
        .filter(|did| !did.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("Bluesky handle did not resolve to a DID: {handle}"))
}

async fn fetch_bluesky_post(client: &reqwest::Client, post_uri: &str) -> Result<Value> {
    let mut url = Url::parse("https://public.api.bsky.app/xrpc/app.bsky.feed.getPostThread")?;
    url.query_pairs_mut()
        .append_pair("uri", post_uri)
        .append_pair("depth", "0");

    let payload: Value = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    payload
        .pointer("/thread/post")
        .cloned()
        .ok_or_else(|| anyhow!("Bluesky post response did not include thread.post"))
}

fn extract_bluesky_image_urls(post: &Value) -> Vec<String> {
    let Some(images) = post.pointer("/embed/images").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut urls = Vec::new();
    for image in images {
        if let Some(url) = image.get("fullsize").and_then(Value::as_str)
            && is_bluesky_image_url(url)
            && !urls.iter().any(|existing| existing == url)
        {
            urls.push(url.to_string());
        }
    }
    urls
}

fn is_bluesky_image_url(url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|url| {
        url.host_str() == Some("cdn.bsky.app") && url.path().starts_with("/img/feed_fullsize/")
    })
}

fn extract_bluesky_metadata(post: &Value) -> Option<BlueskyPostMetadata> {
    let author = post.get("author");
    let handle = author
        .and_then(|author| author.get("handle"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|handle| !handle.is_empty());
    let source_name = author
        .and_then(|author| author.get("displayName"))
        .and_then(Value::as_str)
        .map(decode_twitter_text)
        .filter(|name| !name.is_empty())
        .or_else(|| handle.map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let source_handle = handle.map(|handle| format!("@{handle}"));
    let text = post
        .pointer("/record/text")
        .and_then(Value::as_str)
        .map(decode_twitter_text)
        .unwrap_or_default();
    let title = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "untitled".into());
    let timestamp = post
        .pointer("/record/createdAt")
        .and_then(Value::as_str)
        .and_then(|created_at| DateTime::parse_from_rfc3339(created_at).ok())
        .map(|created_at| created_at.timestamp());

    Some(BlueskyPostMetadata {
        source_name,
        source_handle,
        timestamp,
        title,
        text,
    })
}

fn write_bluesky_metadata_sidecars(
    item_dir: &Utf8PathBuf,
    rkey: &str,
    normalized: &str,
    metadata: &BlueskyPostMetadata,
) -> Result<()> {
    let mut info = Map::new();
    info.insert("id".into(), Value::String(rkey.to_string()));
    info.insert("extractor_key".into(), Value::String("Bluesky".into()));
    info.insert("extractor".into(), Value::String("Bluesky".into()));
    info.insert("webpage_url".into(), Value::String(normalized.to_string()));
    info.insert(
        "webpage_url_domain".into(),
        Value::String("bsky.app".into()),
    );
    info.insert(
        "channel".into(),
        Value::String(metadata.source_name.clone()),
    );
    info.insert("title".into(), Value::String(metadata.title.clone()));

    if let Some(handle) = &metadata.source_handle {
        info.insert("uploader".into(), Value::String(handle.clone()));
        info.insert("uploader_id".into(), Value::String(handle.clone()));
    } else {
        info.insert(
            "uploader".into(),
            Value::String(metadata.source_name.clone()),
        );
    }
    if let Some(timestamp) = metadata.timestamp {
        info.insert("timestamp".into(), Value::Number(timestamp.into()));
    }

    let info_path = item_dir.join(format!("bluesky-{rkey}.info.json"));
    fs::write(info_path, serde_json::to_vec_pretty(&Value::Object(info))?)?;

    let description_path = item_dir.join(format!("bluesky-{rkey}.description"));
    fs::write(description_path, metadata.text.as_bytes())?;

    Ok(())
}

async fn fetch_instagram_graphql_media(
    client: &reqwest::Client,
    shortcode: &str,
) -> Result<Option<Value>> {
    let mut url = Url::parse("https://www.instagram.com/graphql/query")?;
    url.query_pairs_mut()
        .append_pair("doc_id", "8845758582119845")
        .append_pair("variables", &format!(r#"{{"shortcode":"{shortcode}"}}"#));

    let payload: Value = client
        .get(url)
        .header("X-IG-App-ID", "936619743392459")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(payload.pointer("/data/xdt_shortcode_media").cloned())
}

fn write_instagram_metadata_sidecars(
    item_dir: &Utf8PathBuf,
    shortcode: &str,
    normalized: &str,
    metadata: &InstagramPostMetadata,
) -> Result<()> {
    let mut info = Map::new();
    info.insert("id".into(), Value::String(shortcode.to_string()));
    info.insert("extractor_key".into(), Value::String("Instagram".into()));
    info.insert("extractor".into(), Value::String("instagram".into()));
    info.insert("webpage_url".into(), Value::String(normalized.to_string()));
    info.insert(
        "webpage_url_domain".into(),
        Value::String("instagram.com".into()),
    );
    info.insert(
        "channel".into(),
        Value::String(metadata.source_name.clone()),
    );
    info.insert("title".into(), Value::String(metadata.title.clone()));

    if let Some(handle) = &metadata.source_handle {
        info.insert("uploader".into(), Value::String(handle.clone()));
        info.insert("uploader_id".into(), Value::String(handle.clone()));
    } else {
        info.insert(
            "uploader".into(),
            Value::String(metadata.source_name.clone()),
        );
    }
    if let Some(timestamp) = metadata.timestamp {
        info.insert("timestamp".into(), Value::Number(timestamp.into()));
    }

    let info_path = item_dir.join(format!("instagram-{shortcode}.info.json"));
    fs::write(info_path, serde_json::to_vec_pretty(&Value::Object(info))?)?;

    let description_path = item_dir.join(format!("instagram-{shortcode}.description"));
    fs::write(description_path, metadata.caption.as_bytes())?;

    Ok(())
}

fn extract_instagram_metadata(media: &Value) -> Option<InstagramPostMetadata> {
    let owner = media.get("owner");
    let username = owner
        .and_then(|owner| owner.get("username"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|username| !username.is_empty());
    let source_name = owner
        .and_then(|owner| owner.get("full_name"))
        .and_then(Value::as_str)
        .map(decode_twitter_text)
        .filter(|name| !name.is_empty())
        .or_else(|| username.map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let source_handle = username.map(|username| format!("@{username}"));
    let timestamp = media.get("taken_at_timestamp").and_then(Value::as_i64);
    let caption = instagram_caption_text(media).unwrap_or_default();
    let title = instagram_title(media, &caption);

    Some(InstagramPostMetadata {
        source_name,
        source_handle,
        timestamp,
        title,
        caption,
    })
}

fn instagram_caption_text(media: &Value) -> Option<String> {
    media
        .pointer("/edge_media_to_caption/edges")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|edge| edge.pointer("/node/text").and_then(Value::as_str))
        .map(decode_twitter_text)
        .find(|caption| !caption.is_empty())
}

fn instagram_title(media: &Value, caption: &str) -> String {
    caption
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            media
                .get("accessibility_caption")
                .and_then(Value::as_str)
                .map(decode_twitter_text)
                .filter(|title| !title.is_empty())
        })
        .unwrap_or_else(|| "untitled".into())
}

fn extract_instagram_graphql_image_urls(media: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(edges) = media
        .pointer("/edge_sidecar_to_children/edges")
        .and_then(Value::as_array)
    {
        for node in edges.iter().filter_map(|edge| edge.get("node")) {
            push_instagram_graphql_image_url(node, &mut urls);
        }
    } else {
        push_instagram_graphql_image_url(media, &mut urls);
    }
    urls
}

fn push_instagram_graphql_image_url(media: &Value, urls: &mut Vec<String>) {
    if media.get("is_video").and_then(Value::as_bool) == Some(true) {
        return;
    }

    let best_resource = media
        .get("display_resources")
        .and_then(Value::as_array)
        .and_then(|resources| {
            resources.iter().max_by_key(|resource| {
                let width = resource
                    .get("config_width")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let height = resource
                    .get("config_height")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                width.saturating_mul(height)
            })
        })
        .and_then(|resource| resource.get("src"))
        .and_then(Value::as_str);
    let url = best_resource.or_else(|| media.get("display_url").and_then(Value::as_str));
    if let Some(url) = url
        && is_instagram_media_image_url(url)
        && !urls.iter().any(|existing| existing == url)
    {
        urls.push(url.to_string());
    }
}

fn is_instagram_media_image_url(url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|url| {
        url.host_str()
            .is_some_and(|host| host.ends_with("cdninstagram.com") || host.ends_with("fbcdn.net"))
            && url.path().contains("/v/t51.")
    })
}

fn item_dir_has_deliverable_files(item_dir: &Utf8PathBuf) -> Result<bool> {
    for entry in fs::read_dir(item_dir)? {
        let path = entry?.path();
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("mp4" | "jpg" | "jpeg" | "png" | "webp")
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn instagram_embed_url(normalized: &str) -> Result<Option<Url>> {
    let Some(shortcode) = instagram_shortcode_from_url(normalized)? else {
        return Ok(None);
    };
    let segments = ["p", shortcode.as_str(), "embed"];
    let mut embed_url = Url::parse("https://www.instagram.com/")?;
    {
        let mut output_segments = embed_url
            .path_segments_mut()
            .map_err(|_| anyhow!("unable to build Instagram embed URL"))?;
        output_segments.clear();
        for segment in segments {
            output_segments.push(segment);
        }
    }
    Ok(Some(embed_url))
}

fn instagram_shortcode_from_url(normalized: &str) -> Result<Option<String>> {
    let url = Url::parse(normalized)?;
    let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) else {
        return Ok(None);
    };
    if host != "instagram.com" && host != "www.instagram.com" {
        return Ok(None);
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    match segments.as_slice() {
        ["p" | "reel" | "tv", shortcode, ..] if !shortcode.is_empty() => {
            Ok(Some((*shortcode).to_string()))
        }
        [_, "p" | "reel" | "tv", shortcode, ..] if !shortcode.is_empty() => {
            Ok(Some((*shortcode).to_string()))
        }
        _ => Ok(None),
    }
}

fn twitter_status_id_from_url(normalized: &str) -> Result<Option<String>> {
    let url = Url::parse(normalized)?;
    let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) else {
        return Ok(None);
    };
    if !matches!(
        host.as_str(),
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com"
    ) {
        return Ok(None);
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    for window in segments.windows(2) {
        if matches!(window[0], "status" | "statuses")
            && !window[1].is_empty()
            && window[1].chars().all(|ch| ch.is_ascii_digit())
        {
            return Ok(Some(window[1].to_string()));
        }
    }
    Ok(None)
}

fn write_twitter_metadata_sidecars(
    item_dir: &Utf8PathBuf,
    status_id: &str,
    normalized: &str,
    metadata: &TwitterPostMetadata,
) -> Result<()> {
    let mut info = Map::new();
    info.insert("id".into(), Value::String(status_id.to_string()));
    info.insert("extractor_key".into(), Value::String("Twitter".into()));
    info.insert("extractor".into(), Value::String("twitter".into()));
    info.insert("webpage_url".into(), Value::String(normalized.to_string()));
    info.insert("webpage_url_domain".into(), Value::String("x.com".into()));
    info.insert(
        "channel".into(),
        Value::String(metadata.source_name.clone()),
    );
    info.insert("title".into(), Value::String(metadata.text.clone()));

    if let Some(handle) = &metadata.source_handle {
        info.insert("uploader".into(), Value::String(handle.clone()));
        info.insert("uploader_id".into(), Value::String(handle.clone()));
    } else {
        info.insert(
            "uploader".into(),
            Value::String(metadata.source_name.clone()),
        );
    }
    if let Some(timestamp) = metadata.timestamp {
        info.insert("timestamp".into(), Value::Number(timestamp.into()));
    }

    let info_path = item_dir.join(format!("twitter-{status_id}.info.json"));
    fs::write(info_path, serde_json::to_vec_pretty(&Value::Object(info))?)?;

    let description_path = item_dir.join(format!("twitter-{status_id}.description"));
    fs::write(description_path, "")?;

    Ok(())
}

fn extract_twitter_metadata(html: &str, status_id: &str) -> Option<TwitterPostMetadata> {
    let state = extract_twitter_initial_state(html)?;
    let tweet = state.pointer(&format!("/entities/tweets/entities/{status_id}"))?;
    let user_id = tweet.get("user").and_then(Value::as_str);
    let user =
        user_id.and_then(|user_id| state.pointer(&format!("/entities/users/entities/{user_id}")));

    let source_name = user
        .and_then(|user| user.get("name"))
        .and_then(Value::as_str)
        .map(decode_twitter_text)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let source_handle = user
        .and_then(|user| user.get("screen_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|handle| !handle.is_empty())
        .map(|handle| format!("@{handle}"));
    let timestamp = tweet
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|created_at| DateTime::parse_from_rfc3339(created_at).ok())
        .map(|created_at| created_at.timestamp());
    let text = twitter_visible_text(tweet)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "untitled".into());

    Some(TwitterPostMetadata {
        source_name,
        source_handle,
        timestamp,
        text,
    })
}

fn extract_twitter_initial_state(html: &str) -> Option<Value> {
    let marker = "window.__INITIAL_STATE__=";
    let start = html.find(marker)? + marker.len();
    let mut deserializer = serde_json::Deserializer::from_str(&html[start..]);
    Value::deserialize(&mut deserializer).ok()
}

fn twitter_visible_text(tweet: &Value) -> Option<String> {
    let raw = tweet
        .get("full_text")
        .or_else(|| tweet.get("text"))
        .and_then(Value::as_str)?;
    let text = if let Some(range) = tweet.get("display_text_range").and_then(Value::as_array) {
        let start = range.first().and_then(Value::as_u64).unwrap_or_default() as usize;
        let end = range
            .get(1)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| raw.chars().count() as u64) as usize;
        raw.chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect::<String>()
    } else {
        raw.to_string()
    };
    Some(decode_twitter_text(&text))
}

fn decode_twitter_text(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

fn extract_twitter_image_urls(html: &str) -> Vec<String> {
    const PATTERN: &str = r#""media_url_https":""#;

    let mut urls = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(PATTERN) {
        let value_start = start + PATTERN.len();
        let value = &rest[value_start..];
        let Some(end) = value.find('"') else {
            break;
        };
        let media_url = decode_instagram_embed_url(&value[..end]);
        if let Some(original_url) = twitter_original_image_url(&media_url)
            && !urls.iter().any(|existing| existing == &original_url)
        {
            urls.push(original_url);
        }
        rest = &value[end + 1..];
    }
    urls
}

fn twitter_original_image_url(media_url: &str) -> Option<String> {
    let mut url = Url::parse(media_url).ok()?;
    if url.host_str()? != "pbs.twimg.com" {
        return None;
    }
    if !url.path().starts_with("/media/") {
        return None;
    }

    let extension = url
        .path_segments()?
        .next_back()?
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())?;
    let format = match extension.as_str() {
        "jpg" | "jpeg" => "jpg",
        "png" => "png",
        "webp" => "webp",
        _ => return None,
    };

    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("format", format)
        .append_pair("name", "orig");
    Some(url.to_string())
}

fn extract_instagram_sidecar_image_urls(html: &str) -> Vec<String> {
    let Some(sidecar_start) = html
        .find(r#"\"edge_sidecar_to_children\""#)
        .or_else(|| html.find(r#""edge_sidecar_to_children""#))
    else {
        return Vec::new();
    };
    let sidecar = &html[sidecar_start..];
    let escaped = extract_instagram_urls_with_pattern(sidecar, r#"\"display_url\":\""#, r#"\""#);
    if !escaped.is_empty() {
        return escaped;
    }
    extract_instagram_urls_with_pattern(sidecar, r#""display_url":""#, r#"""#)
}

fn extract_instagram_urls_with_pattern(
    haystack: &str,
    start_pattern: &str,
    end_pattern: &str,
) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = haystack;
    while let Some(start) = rest.find(start_pattern) {
        let value_start = start + start_pattern.len();
        let value = &rest[value_start..];
        let Some(end) = value.find(end_pattern) else {
            break;
        };
        let url = decode_instagram_embed_url(&value[..end]);
        if url.starts_with("https://") && !urls.contains(&url) {
            urls.push(url);
        }
        rest = &value[end + end_pattern.len()..];
    }
    urls
}

fn decode_instagram_embed_url(input: &str) -> String {
    input
        .replace(r"\\/", "/")
        .replace(r"\/", "/")
        .replace(r"\u0026", "&")
        .replace("&amp;", "&")
}

fn image_extension_from_url(image_url: &str) -> &'static str {
    Url::parse(image_url)
        .ok()
        .and_then(|url| {
            url.path_segments()?
                .next_back()?
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase())
        })
        .and_then(|extension| match extension.as_str() {
            "jpg" | "jpeg" => Some("jpg"),
            "png" => Some("png"),
            "webp" => Some("webp"),
            _ => None,
        })
        .unwrap_or("jpg")
}

async fn collect_delivery_files(
    item_dir: &Utf8PathBuf,
    filename_metadata: &DeliveryFilenameMetadata,
    progress: Option<&DownloadProgress<'_>>,
) -> Result<Vec<String>> {
    let mut video_files = Vec::new();
    let mut audio_files = Vec::new();
    let mut image_files = Vec::new();

    for entry in fs::read_dir(item_dir)? {
        let path = entry?.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("mp4") => video_files.push(path),
            Some("m4a") => audio_files.push(path),
            Some("jpg" | "jpeg" | "png" | "webp") => image_files.push(path),
            _ => {}
        }
    }

    video_files.sort();
    audio_files.sort();
    image_files.sort();

    if let Some(video_path) = video_files.first() {
        let normalized =
            normalize_video_for_delivery(video_path, audio_files.first(), progress).await?;
        let renamed = rename_delivery_file(&normalized, filename_metadata, None)?;
        return Ok(vec![renamed.to_string_lossy().to_string()]);
    }

    let total_images = image_files.len();
    let mut files = Vec::with_capacity(total_images);
    for (index, path) in image_files.into_iter().enumerate() {
        let suffix = (total_images > 1).then_some(index + 1);
        let renamed = normalize_image_for_delivery(&path, filename_metadata, suffix).await?;
        files.push(renamed.to_string_lossy().to_string());
    }
    if files.is_empty() {
        bail!("yt-dlp produced no deliverable media files");
    }
    Ok(files)
}

async fn normalize_video_for_delivery(
    video_path: &PathBuf,
    audio_path: Option<&PathBuf>,
    progress: Option<&DownloadProgress<'_>>,
) -> Result<PathBuf> {
    let output_path = video_path.with_file_name("delivery.mp4");
    let mut cmd = TokioCommand::new("ffmpeg");
    cmd.arg("-y").arg("-nostdin").arg("-i").arg(video_path);

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
        cmd.arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("0:a:0?")
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg("128k");
    }

    cmd.arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-vf")
        .arg("scale=trunc(iw/2)*2:trunc(ih/2)*2,setsar=1")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&output_path);

    set_download_progress_bar(progress, StatusPhase::ConvertingMedia, 0).await;
    let output = match run_ffmpeg_with_progress(
        &mut cmd,
        None,
        progress,
        StatusPhase::ConvertingMedia,
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            set_download_progress(progress, StatusPhase::ConvertingFailed).await;
            return Err(err);
        }
    };
    if let Err(err) = ensure_ffmpeg_success(output, "ffmpeg") {
        set_download_progress(progress, StatusPhase::ConvertingFailed).await;
        return Err(err);
    }

    let size = fs::metadata(&output_path)?.len();
    if size <= TELEGRAM_UPLOAD_LIMIT_BYTES {
        return Ok(output_path);
    }

    warn!(
        "normalized video is larger than {}; trying target-size re-encode file={} size_bytes={}",
        TELEGRAM_UPLOAD_LIMIT_LABEL,
        output_path.display(),
        size
    );
    match reencode_video_to_telegram_target(&output_path, progress).await {
        Ok(targeted_path) => Ok(targeted_path),
        Err(err) => {
            warn!(
                "target-size video re-encode failed for {}: {err:#}",
                output_path.display()
            );
            Ok(output_path)
        }
    }
}

async fn reencode_video_to_telegram_target(
    input_path: &Path,
    progress: Option<&DownloadProgress<'_>>,
) -> Result<PathBuf> {
    let media_info = match probe_video_reencode_info(input_path).await {
        Ok(media_info) => media_info,
        Err(err) => {
            set_download_progress(progress, StatusPhase::CompressingFailed).await;
            return Err(err);
        }
    };
    let video_bitrate_bps = match telegram_target_video_bitrate_bps(
        media_info.duration_seconds,
        media_info.audio.reserved_bitrate_bps,
    ) {
        Ok(video_bitrate_bps) => video_bitrate_bps,
        Err(err) => {
            set_download_progress(progress, StatusPhase::CompressingFailed).await;
            return Err(err);
        }
    };
    let video_bitrate = format!("{}k", video_bitrate_bps / 1000);
    let output_path = input_path.with_file_name("delivery-target.mp4");
    let passlog_path = input_path.with_file_name("delivery-target-passlog");

    set_download_progress_bar(progress, StatusPhase::CompressingMediaPass1, 0).await;
    let mut pass_one = TokioCommand::new("ffmpeg");
    pass_one
        .arg("-y")
        .arg("-nostdin")
        .arg("-i")
        .arg(input_path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-b:v")
        .arg(&video_bitrate)
        .arg("-vf")
        .arg("scale=trunc(iw/2)*2:trunc(ih/2)*2,setsar=1")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-pass")
        .arg("1")
        .arg("-passlogfile")
        .arg(&passlog_path)
        .arg("-an")
        .arg("-f")
        .arg("null")
        .arg("-");
    let pass_one = match run_ffmpeg_with_progress(
        &mut pass_one,
        Some(media_info.duration_seconds),
        progress,
        StatusPhase::CompressingMediaPass1,
    )
    .await
    {
        Ok(pass_one) => pass_one,
        Err(err) => {
            set_download_progress(progress, StatusPhase::CompressingFailed).await;
            return Err(err);
        }
    };
    if let Err(err) = ensure_ffmpeg_success(pass_one, "ffmpeg target-size first pass") {
        set_download_progress(progress, StatusPhase::CompressingFailed).await;
        return Err(err);
    }

    set_download_progress_bar(progress, StatusPhase::CompressingMediaPass2, 0).await;
    let mut pass_two = TokioCommand::new("ffmpeg");
    pass_two
        .arg("-y")
        .arg("-nostdin")
        .arg("-i")
        .arg(input_path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-b:v")
        .arg(&video_bitrate)
        .arg("-vf")
        .arg("scale=trunc(iw/2)*2:trunc(ih/2)*2,setsar=1")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-pass")
        .arg("2")
        .arg("-passlogfile")
        .arg(&passlog_path);
    if media_info.audio.copy {
        pass_two.arg("-c:a").arg("copy");
    } else {
        pass_two
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg(format!("{}k", TELEGRAM_REENCODE_AUDIO_BITRATE_BPS / 1000));
    }
    pass_two
        .arg("-movflags")
        .arg("+faststart")
        .arg(&output_path);
    let pass_two = match run_ffmpeg_with_progress(
        &mut pass_two,
        Some(media_info.duration_seconds),
        progress,
        StatusPhase::CompressingMediaPass2,
    )
    .await
    {
        Ok(pass_two) => pass_two,
        Err(err) => {
            set_download_progress(progress, StatusPhase::CompressingFailed).await;
            cleanup_ffmpeg_passlog(&passlog_path);
            return Err(err);
        }
    };
    let result = ensure_ffmpeg_success(pass_two, "ffmpeg target-size second pass");
    cleanup_ffmpeg_passlog(&passlog_path);
    if let Err(err) = result {
        set_download_progress(progress, StatusPhase::CompressingFailed).await;
        return Err(err);
    }

    let size = fs::metadata(&output_path)?.len();
    if size > TELEGRAM_UPLOAD_LIMIT_BYTES {
        warn!(
            "target-size video re-encode is still larger than {} file={} size_bytes={} target_bytes={}",
            TELEGRAM_UPLOAD_LIMIT_LABEL,
            output_path.display(),
            size,
            TELEGRAM_REENCODE_TARGET_BYTES
        );
    } else {
        info!(
            "target-size video re-encode completed file={} size_bytes={} video_bitrate_bps={} audio_bitrate_bps={} audio_copy={}",
            output_path.display(),
            size,
            video_bitrate_bps,
            media_info.audio.reserved_bitrate_bps,
            media_info.audio.copy
        );
    }

    Ok(output_path)
}

async fn run_ffmpeg_with_progress(
    cmd: &mut TokioCommand,
    duration_seconds: Option<f64>,
    progress: Option<&DownloadProgress<'_>>,
    phase: StatusPhase,
) -> Result<std::process::Output> {
    let mut child = cmd
        .arg("-progress")
        .arg("pipe:2")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture ffmpeg stderr"))?;
    let mut lines = BufReader::new(stderr).lines();
    let mut stderr_text = String::new();
    let mut throttle = ProgressThrottle::after_sent(phase, 0);
    let mut duration_seconds = duration_seconds;

    while let Some(line) = lines.next_line().await? {
        if duration_seconds.is_none() {
            duration_seconds = parse_ffmpeg_duration_seconds(&line);
        }
        if let Some(percent) = duration_seconds
            .and_then(|duration_seconds| parse_ffmpeg_progress_percent(&line, duration_seconds))
        {
            throttle.update(progress, percent).await;
        } else {
            stderr_text.push_str(&line);
            stderr_text.push('\n');
        }
    }

    throttle.flush(progress).await;
    let status = child.wait().await?;
    Ok(output_from_status(
        status,
        Vec::new(),
        stderr_text.into_bytes(),
    ))
}

fn parse_ffmpeg_progress_percent(line: &str, duration_seconds: f64) -> Option<f64> {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return None;
    }
    let raw = line.strip_prefix("out_time_ms=")?;
    let out_time_us = raw.trim().parse::<u64>().ok()?;
    Some((out_time_us as f64 / 1_000_000.0) * 100.0 / duration_seconds)
}

fn parse_ffmpeg_duration_seconds(line: &str) -> Option<f64> {
    let duration = line.split_once("Duration:")?.1.trim();
    let timestamp = duration.split_once(',')?.0.trim();
    parse_ffmpeg_timestamp_seconds(timestamp)
}

fn parse_ffmpeg_timestamp_seconds(timestamp: &str) -> Option<f64> {
    let mut parts = timestamp.split(':');
    let hours = parts.next()?.parse::<f64>().ok()?;
    let minutes = parts.next()?.parse::<f64>().ok()?;
    let seconds = parts.next()?.parse::<f64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn ensure_ffmpeg_success(output: std::process::Output, context: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        bail!("{context} failed with status {}", output.status);
    }
    bail!("{context} failed with status {}: {}", output.status, stderr);
}

fn telegram_target_video_bitrate_bps(duration_seconds: f64, audio_bitrate_bps: u64) -> Result<u64> {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        bail!("video duration is not usable for target-size re-encode: {duration_seconds}");
    }

    let total_bitrate_bps = (TELEGRAM_REENCODE_TARGET_BYTES as f64 * 8.0 / duration_seconds) as u64;
    let reserved_bps = audio_bitrate_bps
        + TELEGRAM_REENCODE_CONTAINER_OVERHEAD_BPS
        + TELEGRAM_REENCODE_MIN_VIDEO_BITRATE_BPS;
    if total_bitrate_bps <= reserved_bps {
        bail!(
            "video is too long to fit under {} with a usable bitrate: duration_seconds={duration_seconds:.1}",
            TELEGRAM_UPLOAD_LIMIT_LABEL
        );
    }

    Ok(total_bitrate_bps - audio_bitrate_bps - TELEGRAM_REENCODE_CONTAINER_OVERHEAD_BPS)
}

async fn probe_video_reencode_info(path: &Path) -> Result<VideoReencodeInfo> {
    let ffprobe = which("ffprobe").context("required tool `ffprobe` was not found on PATH")?;
    let output = TokioCommand::new(ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("stream=codec_type,codec_name,bit_rate:format=duration")
        .arg("-of")
        .arg("json")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("ffprobe failed with status {}", output.status);
        }
        bail!("ffprobe failed with status {}: {}", output.status, stderr);
    }

    let payload: Value =
        serde_json::from_slice(&output.stdout).context("failed to parse ffprobe JSON")?;
    let duration_seconds = payload
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .ok_or_else(|| {
            anyhow!(
                "ffprobe did not return video duration for {}",
                path.display()
            )
        })?;

    Ok(VideoReencodeInfo {
        duration_seconds,
        audio: target_audio_plan(&payload),
    })
}

fn target_audio_plan(payload: &Value) -> TargetAudioPlan {
    let audio = payload
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| {
            streams.iter().find(|stream| {
                stream
                    .get("codec_type")
                    .and_then(Value::as_str)
                    .is_some_and(|codec_type| codec_type == "audio")
            })
        });

    let codec_name = audio
        .and_then(|stream| stream.get("codec_name"))
        .and_then(Value::as_str);
    let source_bitrate = audio.and_then(read_stream_bitrate_bps);
    if codec_name.is_some_and(|codec| codec == "aac")
        && source_bitrate.is_some_and(|bitrate| bitrate <= TELEGRAM_COPY_AUDIO_MAX_BITRATE_BPS)
    {
        return TargetAudioPlan {
            copy: true,
            reserved_bitrate_bps: source_bitrate.unwrap(),
        };
    }

    TargetAudioPlan {
        copy: false,
        reserved_bitrate_bps: TELEGRAM_REENCODE_AUDIO_BITRATE_BPS,
    }
}

fn read_stream_bitrate_bps(stream: &Value) -> Option<u64> {
    stream
        .get("bit_rate")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
}

fn cleanup_ffmpeg_passlog(passlog_path: &Path) {
    for suffix in ["-0.log", "-0.log.mbtree", ".log", ".log.mbtree"] {
        let path = PathBuf::from(format!("{}{}", passlog_path.display(), suffix));
        let _ = fs::remove_file(path);
    }
}

async fn probe_telegram_video_metadata(path: &Path) -> Option<TelegramVideoMetadata> {
    let ffprobe = match which("ffprobe") {
        Ok(path) => path,
        Err(_) => return None,
    };

    let output = match TokioCommand::new(ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=width,height:format=duration")
        .arg("-of")
        .arg("json")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(output) => output,
        Err(err) => {
            warn!("ffprobe failed to start for {}: {}", path.display(), err);
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            warn!(
                "ffprobe exited with status {} while probing {}",
                output.status,
                path.display()
            );
        } else {
            warn!(
                "ffprobe exited with status {} while probing {}: {}",
                output.status,
                path.display(),
                stderr
            );
        }
        return None;
    }

    let payload: Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(err) => {
            warn!(
                "failed to parse ffprobe JSON for {}: {}",
                path.display(),
                err
            );
            return None;
        }
    };

    let stream = payload
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())?;
    let width = stream
        .get("width")
        .and_then(Value::as_u64)?
        .try_into()
        .ok()?;
    let height = stream
        .get("height")
        .and_then(Value::as_u64)?
        .try_into()
        .ok()?;
    let duration = payload
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .map(|seconds| seconds.ceil() as u32)?;

    Some(TelegramVideoMetadata {
        width,
        height,
        duration_seconds: duration,
    })
}

async fn normalize_image_for_delivery(
    image_path: &PathBuf,
    metadata: &DeliveryFilenameMetadata,
    sequence: Option<usize>,
) -> Result<PathBuf> {
    match image_path.extension().and_then(|ext| ext.to_str()) {
        Some("jpg" | "jpeg" | "png") => rename_delivery_file(image_path, metadata, sequence),
        _ => transcode_image_for_delivery(image_path, metadata, sequence).await,
    }
}

async fn transcode_image_for_delivery(
    image_path: &PathBuf,
    metadata: &DeliveryFilenameMetadata,
    sequence: Option<usize>,
) -> Result<PathBuf> {
    let output_path = image_path.with_file_name(build_delivery_filename(metadata, sequence, "jpg"));
    let output = TokioCommand::new("ffmpeg")
        .arg("-y")
        .arg("-nostdin")
        .arg("-i")
        .arg(image_path)
        .arg("-frames:v")
        .arg("1")
        .arg(&output_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!(
                "ffmpeg image conversion failed with status {}",
                output.status
            );
        }
        bail!(
            "ffmpeg image conversion failed with status {}: {}",
            output.status,
            stderr
        );
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
    let platform_name = read_platform_name(&info);
    let source_name = clean_source_name_for_platform(
        &platform_name,
        read_string_field(&info, &["channel", "uploader", "creator", "artist"])
            .unwrap_or_else(|| "unknown".into()),
    );
    let source_handle = read_source_handle(&info);
    let filename_username = if is_youtube_source(&info) {
        source_handle.clone().unwrap_or_else(|| source_name.clone())
    } else {
        read_string_field(
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
        .unwrap_or_else(|| source_name.clone())
    };
    DeliveryFilenameMetadata {
        filename_username,
        platform_name,
        source_name,
        source_handle,
        date: read_upload_date(&info).unwrap_or_default(),
        title: read_string_field(&info, &["title", "fulltitle", "alt_title"])
            .unwrap_or_else(|| "untitled".into()),
    }
}

fn clean_source_name_for_platform(platform_name: &str, source_name: String) -> String {
    if platform_name == "X" {
        let trimmed = source_name.trim();
        if let Some(prefix) = trimmed.strip_suffix(" on X") {
            let clean = prefix.trim();
            if !clean.is_empty() {
                return clean.to_string();
            }
        }
    }
    source_name
}

fn read_info_json(dir: &Utf8PathBuf) -> Option<Value> {
    let info_path = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && path.to_string_lossy().ends_with(".info.json")
        })?;
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

fn read_platform_name(value: &Value) -> String {
    let raw = read_string_field(value, &["extractor_key", "extractor", "webpage_url_domain"])
        .unwrap_or_else(|| "unknown".into());
    normalize_platform_name(&raw)
}

fn normalize_platform_name(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.contains("youtube") {
        return "YouTube".into();
    }
    if lower.contains("instagram") {
        return "Instagram".into();
    }
    if lower.contains("tiktok") {
        return "TikTok".into();
    }
    if lower.contains("bluesky") || lower.contains("bsky") {
        return "Bluesky".into();
    }
    if lower.contains("twitter") || lower == "x" || lower.contains("x.com") {
        return "X".into();
    }
    if lower.contains("reddit") {
        return "Reddit".into();
    }

    let platform = lower
        .split(['.', ':'])
        .next()
        .unwrap_or(lower.as_str())
        .split(['_', '-', ' '])
        .filter(|part| !part.is_empty())
        .map(title_case_ascii)
        .collect::<Vec<_>>()
        .join(" ");
    if platform.is_empty() {
        "Unknown".into()
    } else {
        platform
    }
}

fn title_case_ascii(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
    }
    for ch in chars {
        out.extend(ch.to_lowercase());
    }
    out
}

fn is_youtube_source(value: &Value) -> bool {
    read_string_field(value, &["webpage_url_domain", "extractor_key", "extractor"])
        .is_some_and(|text| text.to_ascii_lowercase().contains("youtube"))
}

fn read_source_handle(value: &Value) -> Option<String> {
    if is_youtube_source(value) {
        return read_youtube_handle(value);
    }

    read_string_field(value, &["uploader_id"])
        .or_else(|| read_string_field(value, &["channel_id"]))
        .and_then(|handle| normalize_display_handle(&handle))
}

fn read_youtube_handle(value: &Value) -> Option<String> {
    if let Some(handle) = read_string_field(value, &["uploader_id"])
        .and_then(|handle| normalize_display_handle(&handle))
    {
        return Some(handle);
    }

    for key in ["uploader_url", "channel_url", "webpage_url"] {
        if let Some(url) = value.get(key).and_then(Value::as_str) {
            if let Some(handle) = extract_at_handle_from_url(url) {
                return Some(handle);
            }
        }
    }

    None
}

fn normalize_display_handle(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let handle = trimmed.strip_prefix('@')?;
    if handle.is_empty() {
        return None;
    }
    Some(format!("@{handle}"))
}

fn extract_at_handle_from_url(input: &str) -> Option<String> {
    let marker = input.find("/@")?;
    let rest = &input[(marker + 2)..];
    let handle = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    if handle.is_empty() {
        return None;
    }
    Some(format!("@{handle}"))
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
            return Some(date.format("%Y.%m.%d").to_string());
        }
    }

    None
}

fn format_compact_date(input: &str) -> Option<String> {
    if input.len() != 8 || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    Some(format!(
        "{}.{}.{}",
        &input[0..4],
        &input[4..6],
        &input[6..8]
    ))
}

async fn run_yt_dlp_download(
    normalized: &str,
    out_tmpl: &Utf8PathBuf,
    progress: Option<&DownloadProgress<'_>>,
) -> Result<std::process::Output> {
    let preferred = spawn_yt_dlp_download(
        normalized,
        out_tmpl,
        Some("bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/b"),
        yt_dlp_format_sort(normalized),
        progress,
    )
    .await?;
    if preferred.status.success() {
        return Ok(preferred);
    }

    let stderr = String::from_utf8_lossy(&preferred.stderr);
    if should_retry_without_video_format(&stderr) {
        let fallback = spawn_yt_dlp_download(normalized, out_tmpl, None, None, progress).await?;
        return Ok(fallback);
    }

    Ok(preferred)
}

fn yt_dlp_format_sort(normalized: &str) -> Option<&'static str> {
    let host = Url::parse(normalized)
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    if is_instagram_host(&host) || is_youtube_host(&host) {
        return Some("res,vcodec:h265:h264");
    }

    None
}

fn is_instagram_host(host: &str) -> bool {
    host == "instagram.com" || host == "www.instagram.com"
}

fn is_youtube_host(host: &str) -> bool {
    matches!(
        host,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be"
    )
}

async fn spawn_yt_dlp_download(
    normalized: &str,
    out_tmpl: &Utf8PathBuf,
    format_selector: Option<&str>,
    format_sort: Option<&str>,
    progress: Option<&DownloadProgress<'_>>,
) -> Result<std::process::Output> {
    let mut cmd = TokioCommand::new("yt-dlp");
    if let Some(format_selector) = format_selector {
        cmd.arg("-f")
            .arg(format_selector)
            .arg("--merge-output-format")
            .arg("mp4");
    }
    if let Some(format_sort) = format_sort {
        cmd.arg("-S").arg(format_sort);
    }

    let mut child = cmd
        .arg("--newline")
        .arg("--progress")
        .arg("--progress-delta")
        .arg("1")
        .arg("--write-info-json")
        .arg("--write-description")
        .arg("--convert-thumbnails")
        .arg("jpg")
        .arg("--output")
        .arg(out_tmpl.as_str())
        .arg(normalized)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture yt-dlp stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture yt-dlp stderr"))?;
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stderr_text = String::new();
    let mut progress_throttle = ProgressThrottle::after_sent(StatusPhase::DownloadingMedia, 0);

    while stdout_open || stderr_open {
        tokio::select! {
            line = stdout_lines.next_line(), if stdout_open => {
                match line? {
                    Some(line) => handle_yt_dlp_progress_line(&line, progress, &mut progress_throttle).await,
                    None => stdout_open = false,
                }
            }
            line = stderr_lines.next_line(), if stderr_open => {
                match line? {
                    Some(line) => {
                        handle_yt_dlp_progress_line(&line, progress, &mut progress_throttle).await;
                        if !is_yt_dlp_progress_line(&line) {
                            stderr_text.push_str(&line);
                            stderr_text.push('\n');
                        }
                    }
                    None => stderr_open = false,
                }
            }
        }
    }

    progress_throttle.flush(progress).await;
    let status = child.wait().await?;
    Ok(output_from_status(
        status,
        Vec::new(),
        stderr_text.into_bytes(),
    ))
}

async fn handle_yt_dlp_progress_line(
    line: &str,
    progress: Option<&DownloadProgress<'_>>,
    throttle: &mut ProgressThrottle,
) {
    if let Some(percent) = parse_yt_dlp_progress_percent(line) {
        throttle.update(progress, percent).await;
    }
}

fn is_yt_dlp_progress_line(line: &str) -> bool {
    parse_yt_dlp_progress_percent(line).is_some()
}

fn parse_yt_dlp_progress_percent(line: &str) -> Option<f64> {
    let trimmed = line.trim();
    let raw = trimmed.strip_prefix("download:").unwrap_or(trimmed).trim();
    parse_percent_from_text(raw)
}

fn parse_percent_from_text(text: &str) -> Option<f64> {
    let percent_index = text.find('%')?;
    let before_percent = &text[..percent_index];
    let start = before_percent
        .rfind(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .map_or(0, |index| index + 1);
    let raw = before_percent[start..].trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse::<f64>().ok()
}

fn output_from_status(
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> std::process::Output {
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

fn should_retry_without_video_format(stderr: &str) -> bool {
    stderr.contains("There is no video in this post")
        || stderr.contains("Requested format is not available")
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
    let filename = build_delivery_filename(metadata, sequence, extension);
    let renamed = path.with_file_name(filename);

    if renamed != path {
        fs::rename(path, &renamed)?;
    }

    Ok(renamed)
}

fn build_delivery_filename(
    metadata: &DeliveryFilenameMetadata,
    sequence: Option<usize>,
    extension: &str,
) -> String {
    let suffix = sequence
        .map(|index| format!("_{index:02}"))
        .unwrap_or_default();
    let max_base_len = MAX_DELIVERY_FILENAME_LEN
        .saturating_sub(extension.chars().count() + 1)
        .saturating_sub(suffix.chars().count());
    let base = build_delivery_basename(metadata, max_base_len);
    format!("{base}{suffix}.{extension}")
}

fn build_delivery_basename(metadata: &DeliveryFilenameMetadata, max_len: usize) -> String {
    let username = sanitize_filename_part(&metadata.filename_username, "unknown");
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

fn format_post_source_line(metadata: &DeliveryFilenameMetadata) -> String {
    match metadata.source_handle.as_deref() {
        Some(handle) => format!("{} ({handle})", metadata.source_name),
        None => metadata.source_name.clone(),
    }
}

fn display_post_title(metadata: &DeliveryFilenameMetadata) -> Option<String> {
    let title = metadata.title.trim();
    if title.is_empty() || title.eq_ignore_ascii_case("untitled") {
        None
    } else {
        Some(title.to_string())
    }
}

fn compose_post_text(
    platform: &str,
    title: Option<&str>,
    source: &str,
    date: &str,
    body: &str,
) -> String {
    let mut out = String::new();
    if !platform.trim().is_empty() {
        out.push('(');
        out.push_str(platform);
        out.push_str(")\n");
    }
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        out.push_str(title);
        out.push('\n');
    }
    out.push_str(source);
    out.push('\n');
    out.push_str(date);
    out.push_str("\n\n");
    out.push_str(body);
    out
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

fn ensure_db_schema(conn: &mut Connection) -> Result<()> {
    ensure_users_table(conn)?;
    ensure_media_table(conn)?;
    Ok(())
}

fn ensure_users_table(conn: &mut Connection) -> Result<()> {
    if !table_exists(conn, "users")? {
        create_users_table(conn)?;
        return Ok(());
    }

    let has_username = table_has_column(conn, "users", "username")?;
    let platform_user_id_nullable = column_is_nullable(conn, "users", "platform_user_id")?;
    let strict = table_is_strict(conn, "users")?;

    if has_username && platform_user_id_nullable && strict {
        create_users_indexes(conn)?;
        return Ok(());
    }

    let username_select = if has_username {
        "username"
    } else {
        "NULL AS username"
    };
    let tx = conn.transaction()?;
    tx.execute_batch(
        "
        DROP INDEX IF EXISTS users_platform_user_id_idx;
        DROP INDEX IF EXISTS users_platform_pending_username_idx;
        DROP TABLE IF EXISTS users__new;
        CREATE TABLE users__new (
          id INTEGER PRIMARY KEY,
          platform TEXT NOT NULL,
          platform_user_id TEXT,
          username TEXT,
          role TEXT NOT NULL CHECK(role IN ('admin','user','none')),
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        ) STRICT;
        ",
    )?;
    tx.execute(
        &format!(
            "INSERT INTO users__new(id, platform, platform_user_id, username, role, created_at, updated_at)
             SELECT id, platform, platform_user_id, {username_select}, role, created_at, updated_at
             FROM users"
        ),
        [],
    )?;
    tx.execute_batch(
        "
        DROP TABLE users;
        ALTER TABLE users__new RENAME TO users;
        ",
    )?;
    create_users_indexes(&tx)?;
    tx.commit()?;
    Ok(())
}

fn create_users_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
          id INTEGER PRIMARY KEY,
          platform TEXT NOT NULL,
          platform_user_id TEXT,
          username TEXT,
          role TEXT NOT NULL CHECK(role IN ('admin','user','none')),
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        ) STRICT;
        ",
    )?;
    create_users_indexes(conn)
}

fn create_users_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE UNIQUE INDEX IF NOT EXISTS users_platform_user_id_idx
          ON users(platform, platform_user_id)
          WHERE platform_user_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS users_platform_pending_username_idx
          ON users(platform, username)
          WHERE username IS NOT NULL AND platform_user_id IS NULL;
        ",
    )?;
    Ok(())
}

fn ensure_media_table(conn: &mut Connection) -> Result<()> {
    if !table_exists(conn, "media")? {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS media (
              id INTEGER PRIMARY KEY,
              url TEXT NOT NULL,
              name TEXT NOT NULL,
              post_text TEXT,
              created_at TEXT NOT NULL,
              download_duration_ms INTEGER NOT NULL,
              convert_duration_ms INTEGER NOT NULL
            ) STRICT;
            ",
        )?;
        return Ok(());
    }

    if table_is_strict(conn, "media")? {
        return Ok(());
    }

    let tx = conn.transaction()?;
    tx.execute_batch(
        "
        DROP TABLE IF EXISTS media__new;
        CREATE TABLE media__new (
          id INTEGER PRIMARY KEY,
          url TEXT NOT NULL,
          name TEXT NOT NULL,
          post_text TEXT,
          created_at TEXT NOT NULL,
          download_duration_ms INTEGER NOT NULL,
          convert_duration_ms INTEGER NOT NULL
        ) STRICT;
        INSERT INTO media__new(id, url, name, post_text, created_at, download_duration_ms, convert_duration_ms)
        SELECT id, url, name, post_text, created_at, download_duration_ms, convert_duration_ms
        FROM media;
        DROP TABLE media;
        ALTER TABLE media__new RENAME TO media;
        ",
    )?;
    tx.commit()?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn column_is_nullable(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            let not_null: i64 = row.get(3)?;
            return Ok(not_null == 0);
        }
    }
    Ok(false)
}

fn table_is_strict(conn: &Connection, table: &str) -> Result<bool> {
    let strict = conn
        .query_row(
            "SELECT strict FROM pragma_table_list WHERE name = ?1 AND type = 'table'",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(matches!(strict, Some(1)))
}

fn db_conn(config: &Config) -> Result<Connection> {
    Ok(Connection::open(&config.database_path)?)
}

fn init_db(config: &Config) -> Result<()> {
    let mut conn = db_conn(config)?;
    ensure_db_schema(&mut conn)?;

    for u in &config.seed_users {
        upsert_user(config, &u.platform, Some(&u.platform_user_id), None, u.role)?;
    }
    Ok(())
}

fn upsert_user(
    config: &Config,
    platform: &str,
    platform_user_id: Option<&str>,
    username: Option<&str>,
    role: UserRole,
) -> Result<()> {
    let mut conn = db_conn(config)?;
    let tx = conn.transaction()?;
    let now: DateTime<Utc> = Utc::now();
    let now = now.to_rfc3339();

    if let Some(platform_user_id) = platform_user_id {
        let updated = tx.execute(
            "UPDATE users
             SET username = ?1, role = ?2, updated_at = ?3
             WHERE platform = ?4 AND platform_user_id = ?5",
            params![username, role.as_str(), now, platform, platform_user_id],
        )?;
        if updated == 0 {
            tx.execute(
                "INSERT INTO users(platform, platform_user_id, username, role, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![platform, platform_user_id, username, role.as_str(), now],
            )?;
        }
    } else if let Some(username) = username {
        let updated = tx.execute(
            "UPDATE users
             SET role = ?1, updated_at = ?2
             WHERE platform = ?3 AND username = ?4 AND platform_user_id IS NULL",
            params![role.as_str(), now, platform, username],
        )?;
        if updated == 0 {
            tx.execute(
                "INSERT INTO users(platform, platform_user_id, username, role, created_at, updated_at)
                 VALUES (?1, NULL, ?2, ?3, ?4, ?4)",
                params![platform, username, role.as_str(), now],
            )?;
        }
    } else {
        bail!("upsert_user requires a platform_user_id or username");
    }

    tx.commit()?;
    Ok(())
}

fn authorize_user(
    config: &Config,
    platform: &str,
    platform_user_id: &str,
    username: Option<&str>,
) -> Result<Option<UserRole>> {
    let mut conn = db_conn(config)?;
    let tx = conn.transaction()?;
    let now: DateTime<Utc> = Utc::now();
    let now = now.to_rfc3339();

    let direct_match: Option<(i64, String, Option<String>)> = tx
        .query_row(
            "SELECT id, role, username
             FROM users
             WHERE platform = ?1 AND platform_user_id = ?2",
            params![platform, platform_user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    if let Some((id, role, stored_username)) = direct_match {
        if stored_username.as_deref() != username {
            tx.execute(
                "UPDATE users SET username = ?1, updated_at = ?2 WHERE id = ?3",
                params![username, now, id],
            )?;
        } else {
            tx.execute(
                "UPDATE users SET updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
        }
        tx.commit()?;
        return Ok(Some(UserRole::from_str(&role)?));
    }

    if let Some(username) = username {
        let pending_match: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, role
                 FROM users
                 WHERE platform = ?1 AND username = ?2 AND platform_user_id IS NULL",
                params![platform, username],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((id, role)) = pending_match {
            tx.execute(
                "UPDATE users
                 SET platform_user_id = ?1, username = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![platform_user_id, username, now, id],
            )?;
            tx.commit()?;
            return Ok(Some(UserRole::from_str(&role)?));
        }
    }

    tx.commit()?;
    Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bluesky_post_refs() {
        assert_eq!(
            bluesky_post_ref_from_url(
                "https://bsky.app/profile/notpeter.bsky.social/post/3mjcw2b6ajk25"
            )
            .unwrap(),
            Some(BlueskyPostRef {
                handle: "notpeter.bsky.social".into(),
                rkey: "3mjcw2b6ajk25".into(),
            })
        );
        assert_eq!(
            bluesky_post_ref_from_url(
                "https://example.com/profile/notpeter.bsky.social/post/3mjcw2b6ajk25"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn extracts_bluesky_image_urls() {
        let post = serde_json::json!({
            "embed": {"images": [
                {"fullsize": "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:test/one"},
                {"fullsize": "https://cdn.bsky.app/img/feed_thumbnail/plain/did:plc:test/ignored"},
                {"fullsize": "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:test/two"},
                {"fullsize": "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:test/one"}
            ]}
        });

        assert_eq!(
            extract_bluesky_image_urls(&post),
            vec![
                "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:test/one",
                "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:test/two",
            ]
        );
    }

    #[test]
    fn extracts_bluesky_metadata() {
        let post = serde_json::json!({
            "author": {
                "handle": "notpeter.bsky.social",
                "displayName": "Peter &amp; Friends"
            },
            "record": {
                "createdAt": "2026-04-12T17:50:29.007Z",
                "text": "Steps to reproduce:\n1. Put these two PNG images in a folder."
            }
        });

        assert_eq!(
            extract_bluesky_metadata(&post),
            Some(BlueskyPostMetadata {
                source_name: "Peter & Friends".into(),
                source_handle: Some("@notpeter.bsky.social".into()),
                timestamp: Some(1776016229),
                title: "Steps to reproduce:".into(),
                text: "Steps to reproduce:\n1. Put these two PNG images in a folder.".into(),
            })
        );
    }

    #[test]
    fn continues_to_bluesky_image_fallback_for_photo_post_errors() {
        assert!(should_continue_with_bluesky_image_fallback(
            "https://bsky.app/profile/notpeter.bsky.social/post/3mjcw2b6ajk25",
            "ERROR: [Bluesky] 3mjcw2b6ajk25: No video could be found in this post"
        ));
        assert!(!should_continue_with_bluesky_image_fallback(
            "https://example.com/profile/notpeter.bsky.social/post/3mjcw2b6ajk25",
            "ERROR: [Bluesky] 3mjcw2b6ajk25: No video could be found in this post"
        ));
    }

    #[test]
    fn status_phase_labels_are_distinct() {
        let phases = [
            StatusPhase::FetchingLink,
            StatusPhase::FetchFailed,
            StatusPhase::DownloadingMedia,
            StatusPhase::DownloadFailed,
            StatusPhase::ConvertingMedia,
            StatusPhase::ConvertingFailed,
            StatusPhase::CompressingMediaPass1,
            StatusPhase::CompressingMediaPass2,
            StatusPhase::CompressingFailed,
            StatusPhase::UploadingMedia,
            StatusPhase::UploadFailed,
            StatusPhase::DownloadComplete,
        ];
        let labels = phases.map(StatusPhase::label);
        let unique = labels
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn formats_progress_bar_in_five_percent_steps_with_one_percent_text() {
        assert_eq!(progress_display_percent(52.9), Some(52));
        assert_eq!(progress_bucket_from_display_percent(52), 10);
        assert_eq!(progress_bar(10, 52), "[##########----------]  52%");
    }

    #[test]
    fn parses_yt_dlp_progress_template_lines() {
        assert_eq!(parse_yt_dlp_progress_percent("download: 42.7%"), Some(42.7));
        assert_eq!(parse_yt_dlp_progress_percent(" 42.7%"), Some(42.7));
        assert_eq!(
            parse_yt_dlp_progress_percent("[download]  42.7% of 10.00MiB"),
            Some(42.7)
        );
        assert_eq!(
            parse_yt_dlp_progress_percent("[download] Destination: file.mp4"),
            None
        );
    }

    #[test]
    fn parses_ffmpeg_progress_timestamps() {
        assert_eq!(
            parse_ffmpeg_progress_percent("out_time_ms=5000000", 20.0),
            Some(25.0)
        );
    }

    #[test]
    fn parses_ffmpeg_duration_lines() {
        assert_eq!(
            parse_ffmpeg_duration_seconds(
                "  Duration: 00:00:24.90, start: 0.000000, bitrate: 1240 kb/s"
            ),
            Some(24.9)
        );
    }

    #[test]
    fn normalizes_schemeless_urls_as_https() {
        assert_eq!(
            normalize_input_url("facebook.com/reel/1838643350132773").unwrap(),
            "https://facebook.com/reel/1838643350132773"
        );
    }

    #[test]
    fn normalizes_schemeless_twitter_urls_and_strips_query() {
        assert_eq!(
            normalize_input_url("x.com/notpeter/status/123?ref=ignored").unwrap(),
            "https://x.com/notpeter/status/123"
        );
    }

    #[test]
    fn extracts_instagram_metadata_from_graphql_media() {
        let media = serde_json::json!({
            "owner": {
                "username": "notpeter",
                "full_name": "Peter &amp; Friends"
            },
            "taken_at_timestamp": 1706248090,
            "accessibility_caption": "fallback title",
            "edge_media_to_caption": {"edges": [{"node": {
                "text": "First line title\n\nRest of caption &amp; details"
            }}]}
        });

        assert_eq!(
            extract_instagram_metadata(&media),
            Some(InstagramPostMetadata {
                source_name: "Peter & Friends".into(),
                source_handle: Some("@notpeter".into()),
                timestamp: Some(1706248090),
                title: "First line title".into(),
                caption: "First line title\n\nRest of caption & details".into(),
            })
        );
    }

    #[test]
    fn uses_instagram_accessibility_caption_when_caption_is_missing() {
        let media = serde_json::json!({
            "owner": {"username": "notpeter"},
            "accessibility_caption": "iHome HT160B Hilton Alarm Clock."
        });

        assert_eq!(
            extract_instagram_metadata(&media).map(|metadata| metadata.title),
            Some("iHome HT160B Hilton Alarm Clock.".into())
        );
    }

    #[test]
    fn extracts_largest_graphql_image_for_single_post() {
        let media = serde_json::json!({
            "__typename": "XDTGraphImage",
            "is_video": false,
            "display_resources": [
                {"config_width": 640, "config_height": 800, "src": "https://scontent.example.cdninstagram.com/v/t51.82787-15/small.jpg"},
                {"config_width": 1080, "config_height": 1350, "src": "https://scontent.example.cdninstagram.com/v/t51.82787-15/large.jpg"}
            ]
        });

        assert_eq!(
            extract_instagram_graphql_image_urls(&media),
            vec!["https://scontent.example.cdninstagram.com/v/t51.82787-15/large.jpg"]
        );
    }

    #[test]
    fn extracts_graphql_images_for_sidecar_children() {
        let media = serde_json::json!({
            "__typename": "XDTGraphSidecar",
            "edge_sidecar_to_children": {"edges": [
                {"node": {"is_video": false, "display_url": "https://scontent.example.cdninstagram.com/v/t51.82787-15/one.jpg"}},
                {"node": {"is_video": false, "display_resources": [
                    {"config_width": 750, "config_height": 750, "src": "https://scontent.example.cdninstagram.com/v/t51.82787-15/two-small.jpg"},
                    {"config_width": 1080, "config_height": 1080, "src": "https://scontent.example.cdninstagram.com/v/t51.82787-15/two-large.jpg"}
                ]}},
                {"node": {"is_video": true, "display_url": "https://scontent.example.cdninstagram.com/v/t51.82787-15/video-cover.jpg"}}
            ]}
        });

        assert_eq!(
            extract_instagram_graphql_image_urls(&media),
            vec![
                "https://scontent.example.cdninstagram.com/v/t51.82787-15/one.jpg",
                "https://scontent.example.cdninstagram.com/v/t51.82787-15/two-large.jpg",
            ]
        );
    }

    #[test]
    fn extracts_shortcode_from_instagram_post_urls() {
        assert_eq!(
            instagram_shortcode_from_url("https://www.instagram.com/p/C2jWM5qM0SJ/")
                .unwrap()
                .as_deref(),
            Some("C2jWM5qM0SJ")
        );
        assert_eq!(
            instagram_shortcode_from_url("https://www.instagram.com/notpeter/p/C2jWM5qM0SJ/")
                .unwrap()
                .as_deref(),
            Some("C2jWM5qM0SJ")
        );
    }

    #[test]
    fn extracts_twitter_metadata_from_initial_state() {
        let html = r#"
            window.__INITIAL_STATE__={"entities":{"users":{"entities":{"1542461":{
              "name":"Peter &amp; Friends",
              "screen_name":"notpeter"
            }}},"tweets":{"entities":{"1837259033054453894":{
              "full_text":"They literally made signs. https://t.co/voxvxhPSHN",
              "display_text_range":[0,26],
              "created_at":"2024-09-20T22:34:22.000Z",
              "user":"1542461"
            }}}}};
        "#;

        assert_eq!(
            extract_twitter_metadata(html, "1837259033054453894"),
            Some(TwitterPostMetadata {
                source_name: "Peter & Friends".into(),
                source_handle: Some("@notpeter".into()),
                timestamp: Some(1726871662),
                text: "They literally made signs.".into(),
            })
        );
    }

    #[test]
    fn copies_aac_audio_at_or_below_threshold_for_target_encode() {
        let payload = serde_json::json!({
            "streams": [{
                "codec_type": "audio",
                "codec_name": "aac",
                "bit_rate": "131000"
            }]
        });

        assert_eq!(
            target_audio_plan(&payload),
            TargetAudioPlan {
                copy: true,
                reserved_bitrate_bps: 131_000,
            }
        );
    }

    #[test]
    fn transcodes_audio_above_threshold_for_target_encode() {
        let payload = serde_json::json!({
            "streams": [{
                "codec_type": "audio",
                "codec_name": "aac",
                "bit_rate": "131001"
            }]
        });

        assert_eq!(
            target_audio_plan(&payload),
            TargetAudioPlan {
                copy: false,
                reserved_bitrate_bps: TELEGRAM_REENCODE_AUDIO_BITRATE_BPS,
            }
        );
    }

    #[test]
    fn target_size_bitrate_budget_leaves_upload_headroom() {
        let duration_seconds = 156.0;
        let video_bitrate = telegram_target_video_bitrate_bps(
            duration_seconds,
            TELEGRAM_REENCODE_AUDIO_BITRATE_BPS,
        )
        .unwrap();
        let total_bitrate = video_bitrate
            + TELEGRAM_REENCODE_AUDIO_BITRATE_BPS
            + TELEGRAM_REENCODE_CONTAINER_OVERHEAD_BPS;
        let estimated_bytes = (total_bitrate as f64 * duration_seconds / 8.0) as u64;

        assert!(estimated_bytes <= TELEGRAM_REENCODE_TARGET_BYTES);
        assert!(estimated_bytes < TELEGRAM_UPLOAD_LIMIT_BYTES);
    }

    #[test]
    fn target_size_bitrate_budget_rejects_too_long_videos() {
        assert!(
            telegram_target_video_bitrate_bps(1800.0, TELEGRAM_REENCODE_AUDIO_BITRATE_BPS).is_err()
        );
    }

    #[test]
    fn missing_upload_date_stays_empty() {
        let dir = std::env::temp_dir().join(format!("notnotsavebot-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let dir = Utf8PathBuf::from_path_buf(dir).unwrap();

        let metadata = read_delivery_filename_metadata(&dir);
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(metadata.date, "");
    }

    #[test]
    fn extracts_twitter_image_urls_from_initial_state() {
        let html = r#"
            window.__INITIAL_STATE__={"entities":{"tweets":{"entities":{"1":{
              "entities":{"media":[{"media_url_https":"https://pbs.twimg.com/media/one.jpg"}]},
              "extended_entities":{"media":[
                {"media_url_https":"https://pbs.twimg.com/media/one.jpg"},
                {"media_url_https":"https://pbs.twimg.com/media/two.png"}
              ]}
            }}}}};
        "#;

        assert_eq!(
            extract_twitter_image_urls(html),
            vec![
                "https://pbs.twimg.com/media/one.jpg?format=jpg&name=orig",
                "https://pbs.twimg.com/media/two.png?format=png&name=orig",
            ]
        );
    }

    #[test]
    fn extracts_twitter_status_ids() {
        assert_eq!(
            twitter_status_id_from_url("https://x.com/notpeter/status/1837259033054453894")
                .unwrap()
                .as_deref(),
            Some("1837259033054453894")
        );
        assert_eq!(
            twitter_status_id_from_url("https://twitter.com/i/web/status/1739889150990438752")
                .unwrap()
                .as_deref(),
            Some("1739889150990438752")
        );
        assert_eq!(
            twitter_status_id_from_url("https://example.com/notpeter/status/1837259033054453894")
                .unwrap()
                .as_deref(),
            None
        );
    }

    #[test]
    fn continues_to_twitter_image_fallback_for_photo_post_errors() {
        assert!(should_continue_with_twitter_image_fallback(
            "https://x.com/notpeter/status/1837259033054453894",
            "ERROR: [twitter] 1837259033054453894: No video could be found in this tweet"
        ));
        assert!(!should_continue_with_twitter_image_fallback(
            "https://example.com/notpeter/status/1837259033054453894",
            "ERROR: [twitter] 1837259033054453894: No video could be found in this tweet"
        ));
    }

    #[test]
    fn extracts_instagram_sidecar_display_urls_from_embed_payload() {
        let html = r#"
            "edge_sidecar_to_children":{"edges":[
              {"node":{"display_url":"https:\/\/cdn.example.com\/one.jpg?x=1&amp;y=2","display_resources":[{"src":"https:\/\/cdn.example.com\/ignored.jpg"}]}},
              {"node":{"display_url":"https:\/\/cdn.example.com\/two.webp?x=1\u0026y=2"}},
              {"node":{"display_url":"https:\/\/cdn.example.com\/one.jpg?x=1&amp;y=2"}}
            ]}
        "#;

        assert_eq!(
            extract_instagram_sidecar_image_urls(html),
            vec![
                "https://cdn.example.com/one.jpg?x=1&y=2",
                "https://cdn.example.com/two.webp?x=1&y=2",
            ]
        );
    }

    #[test]
    fn extracts_escaped_instagram_sidecar_display_urls_from_embed_payload() {
        let html = r#"
            \"edge_sidecar_to_children\":{\"edges\":[
              {\"node\":{\"display_url\":\"https:\\/\\/cdn.example.com\\/one.jpg?x=1&amp;y=2\"}},
              {\"node\":{\"display_url\":\"https:\\/\\/cdn.example.com\\/two.jpg?x=1\u0026y=2\"}}
            ]}
        "#;

        assert_eq!(
            extract_instagram_sidecar_image_urls(html),
            vec![
                "https://cdn.example.com/one.jpg?x=1&y=2",
                "https://cdn.example.com/two.jpg?x=1&y=2",
            ]
        );
    }

    #[test]
    fn continues_to_embed_fallback_for_instagram_photo_post_errors() {
        assert!(should_continue_with_instagram_embed_fallback(
            "https://www.instagram.com/p/Ce2szzFPqwO/",
            "ERROR: [Instagram] Ce2szzFPqwO: There is no video in this post"
        ));
        assert!(!should_continue_with_instagram_embed_fallback(
            "https://example.com/p/Ce2szzFPqwO/",
            "ERROR: Requested format is not available"
        ));
    }

    #[test]
    fn builds_instagram_embed_url_for_posts() {
        assert_eq!(
            instagram_embed_url("https://www.instagram.com/p/Ccwj6xYMEoX/?utm_source=x")
                .unwrap()
                .unwrap()
                .as_str(),
            "https://www.instagram.com/p/Ccwj6xYMEoX/embed"
        );
    }
}
