use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Instant,
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
    prelude::*,
    types::{
        BotCommand, BotCommandScope, InputFile, InputMedia, InputMediaPhoto, LinkPreviewOptions,
        ParseMode,
    },
    utils::command::BotCommands,
};
use tokio::{process::Command as TokioCommand, task};
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

#[derive(Debug, Clone)]
enum AuthorizationTarget {
    UserId(String),
    Username(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
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
    let download = download_media(config, input).await;
    match download {
        Ok(media) => send_download_result(bot, msg, media).await?,
        Err(e) => {
            error!(
                "download request failed for chat {} input {:?}: {e:#}",
                msg.chat.id.0, input
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
        if is_image_path(file) {
            bot.send_photo(msg.chat.id, InputFile::file(PathBuf::from(file)))
                .await?;
        } else {
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

fn normalize_input_url(input_url: &str) -> Result<String> {
    let mut url = Url::parse(input_url.trim())?;
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

async fn download_media(config: &Config, input_url: &str) -> Result<DownloadResponse> {
    let normalized = normalize_input_url(input_url)?;

    let started = Instant::now();
    let item_dir = config.download_dir.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&item_dir)?;
    let out_tmpl = item_dir.join("%(title).100B-%(id)s.%(ext)s");

    let output = run_yt_dlp_download(&normalized, &out_tmpl).await?;

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
        &config,
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
            Some("jpg" | "jpeg" | "png" | "webp") => image_files.push(path),
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

    let output = cmd
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-vf")
        .arg("scale=trunc(iw/2)*2:trunc(ih/2)*2,setsar=1")
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
        date: read_upload_date(&info).unwrap_or_else(current_delivery_date),
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

fn current_delivery_date() -> String {
    Utc::now().format("%Y.%m.%d").to_string()
}

async fn run_yt_dlp_download(
    normalized: &str,
    out_tmpl: &Utf8PathBuf,
) -> Result<std::process::Output> {
    let preferred = spawn_yt_dlp_download(
        normalized,
        out_tmpl,
        Some("bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/b"),
    )
    .await?;
    if preferred.status.success() {
        return Ok(preferred);
    }

    let stderr = String::from_utf8_lossy(&preferred.stderr);
    if should_retry_without_video_format(&stderr) {
        let fallback = spawn_yt_dlp_download(normalized, out_tmpl, None).await?;
        return Ok(fallback);
    }

    Ok(preferred)
}

async fn spawn_yt_dlp_download(
    normalized: &str,
    out_tmpl: &Utf8PathBuf,
    format_selector: Option<&str>,
) -> Result<std::process::Output> {
    let mut cmd = TokioCommand::new("yt-dlp");
    if let Some(format_selector) = format_selector {
        cmd.arg("-f")
            .arg(format_selector)
            .arg("--merge-output-format")
            .arg("mp4");
    }

    let output = cmd
        .arg("--write-info-json")
        .arg("--write-description")
        .arg("--convert-thumbnails")
        .arg("jpg")
        .arg("--output")
        .arg(out_tmpl.as_str())
        .arg(normalized)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;

    Ok(output)
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
