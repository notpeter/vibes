use std::{
    collections::BTreeMap,
    fs,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Instant,
};

use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, HttpError, HttpResponseCreated, HttpServerStarter,
    RequestContext, TypedBody,
};
use rusqlite::{params, Connection};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use teloxide::{
    prelude::*,
    types::{InputFile, InputMedia, InputMediaPhoto},
};
use tokio::{process::Command as TokioCommand, task};
use tracing::{error, info};
use url::Url;
use uuid::Uuid;

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
    Api {
        #[arg(long = "host")]
        host: Option<String>,
        #[arg(short = 'p', long = "port")]
        port: Option<u16>,
    },
    Config {
        #[command(subcommand)]
        command: ConfigSubcommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
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
    #[serde(default = "default_bind_address")]
    bind_address: String,
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
            bind_address: default_bind_address(),
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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
}

#[derive(Debug, Clone)]
struct AppState {
    config: Config,
}

type AppCtx = Arc<AppState>;

#[derive(Debug, Deserialize, JsonSchema)]
struct DownloadRequest {
    url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DownloadResponse {
    media_id: i64,
    normalized_url: String,
    post_text: String,
    files: Vec<String>,
}

#[derive(Debug, Clone)]
enum PathSegment {
    Key(String),
    Index(usize),
}

const DEFAULT_API_PORT: u16 = 53211;
const DEFAULT_API_HOST: &str = "127.0.0.1";
const SHARED_PORTS_PATH: &str = "../conf/ports.conl";

fn default_bind_address() -> String {
    format!("{DEFAULT_API_HOST}:{DEFAULT_API_PORT}")
}

fn default_database_path() -> Utf8PathBuf {
    Utf8PathBuf::from("./mine-bot.sqlite")
}

fn default_download_dir() -> Utf8PathBuf {
    Utf8PathBuf::from("./downloads")
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CliCommand::Api { host, port } => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .init();
            run_api(cli.config, host, port).await
        }
        CliCommand::Config { command } => run_config_command(&cli.config, command),
    }
}

async fn run_api(
    config_path: Utf8PathBuf,
    cli_host: Option<String>,
    cli_port: Option<u16>,
) -> Result<()> {
    let config = load_config(&config_path)?.config;
    let bind_address = resolve_bind_address(cli_host, cli_port)?;

    fs::create_dir_all(&config.download_dir)?;
    init_db(&config)?;

    let app = Arc::new(AppState {
        config: config.clone(),
    });

    if config.telegram.enabled {
        let tg_state = app.clone();
        tokio::spawn(async move {
            if let Err(e) = run_telegram(tg_state).await {
                error!("telegram adapter failed: {e:#}");
            }
        });
    }

    let mut api = ApiDescription::new();
    api.register(api_endpoints::download_endpoint)?;

    let log = dropshot::ConfigLogging::StderrTerminal {
        level: dropshot::ConfigLoggingLevel::Info,
    }
    .to_logger("mine-bot")
    .map_err(|e| anyhow!(e.to_string()))?;

    let server = HttpServerStarter::new(
        &ConfigDropshot {
            bind_address,
            request_body_max_bytes: 20 * 1024 * 1024,
            ..Default::default()
        },
        api,
        app,
        &log,
    )
    .map_err(|e| anyhow!(e.to_string()))?
    .start();

    info!("mine-bot running");
    server.await.map_err(|e| anyhow!(e))?;
    Ok(())
}

fn resolve_bind_address(cli_host: Option<String>, cli_port: Option<u16>) -> Result<SocketAddr> {
    Ok(SocketAddr::new(
        resolve_api_host(cli_host)?,
        resolve_api_port(cli_port)?,
    ))
}

fn resolve_api_host(cli_host: Option<String>) -> Result<IpAddr> {
    if let Some(host) = cli_host {
        return parse_api_host(&host);
    }

    if let Ok(host) = std::env::var("HOST") {
        return parse_api_host(&host);
    }

    parse_api_host(DEFAULT_API_HOST)
}

fn parse_api_host(host: &str) -> Result<IpAddr> {
    host.parse::<IpAddr>()
        .with_context(|| format!("invalid host value: {host}"))
}

fn resolve_api_port(cli_port: Option<u16>) -> Result<u16> {
    if let Some(port) = cli_port {
        return Ok(port);
    }

    if let Ok(port) = std::env::var("PORT") {
        return port
            .parse::<u16>()
            .with_context(|| format!("invalid PORT value: {port}"));
    }

    if let Some(port) = load_shared_port(env!("CARGO_PKG_NAME"))? {
        return Ok(port);
    }

    Ok(DEFAULT_API_PORT)
}

fn load_shared_port(program_name: &str) -> Result<Option<u16>> {
    let path = Utf8PathBuf::from(SHARED_PORTS_PATH);
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("unable to read shared ports config at {path}"))?;
    let ports: BTreeMap<String, u16> =
        serde_conl::from_str(&strip_double_slash_comment_lines(&raw))
            .with_context(|| format!("unable to parse shared ports config at {path}"))?;

    Ok(ports.get(program_name).copied())
}

fn run_config_command(config_path: &Utf8PathBuf, command: ConfigSubcommand) -> Result<()> {
    match command {
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
    let stripped = strip_double_slash_comment_lines(&raw);
    let config = match format {
        ConfigFormat::Json => serde_json::from_str(&stripped)
            .with_context(|| format!("unable to parse JSON config at {path}"))?,
        ConfigFormat::Conl => serde_conl::from_str(&stripped)
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
        if trimmed.is_empty() || trimmed.starts_with("//") {
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

fn strip_double_slash_comment_lines(text: &str) -> String {
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

#[allow(dead_code)]
mod api_endpoints {
    use super::*;

    #[endpoint {
        method = POST,
        path = "/download"
    }]
    pub(super) async fn download_endpoint(
        rqctx: RequestContext<AppCtx>,
        body: TypedBody<DownloadRequest>,
    ) -> Result<HttpResponseCreated<DownloadResponse>, HttpError> {
        let app = rqctx.context();
        let result = download_media(app.config.clone(), "api", "api", &body.into_inner().url)
            .await
            .map_err(|e| HttpError::for_bad_request(None, format!("download failed: {e:#}")))?;

        Ok(HttpResponseCreated(result))
    }
}

async fn run_telegram(state: AppCtx) -> Result<()> {
    let bot = Bot::new(state.config.telegram.bot_token.clone());

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let state = state.clone();
        async move {
            if let Some(text) = msg.text() {
                let user_id = msg
                    .from
                    .as_ref()
                    .map(|u| u.id.0.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let download =
                    download_media(state.config.clone(), "telegram", &user_id, text).await;
                match download {
                    Ok(media) => {
                        bot.send_message(msg.chat.id, media.post_text.clone())
                            .await?;
                        if media.files.len() == 1 {
                            bot.send_video(
                                msg.chat.id,
                                InputFile::file(PathBuf::from(&media.files[0])),
                            )
                            .await?;
                        } else {
                            let photos: Vec<InputMedia> = media
                                .files
                                .iter()
                                .map(|f| {
                                    InputMedia::Photo(InputMediaPhoto::new(InputFile::file(
                                        PathBuf::from(f),
                                    )))
                                })
                                .collect();
                            if !photos.is_empty() {
                                bot.send_media_group(msg.chat.id, photos).await?;
                            }
                        }
                    }
                    Err(e) => {
                        bot.send_message(msg.chat.id, format!("Download failed: {e:#}"))
                            .await?;
                    }
                }
            }
            respond(())
        }
    })
    .await;
    Ok(())
}

async fn download_media(
    config: Config,
    platform: &str,
    platform_user_id: &str,
    input_url: &str,
) -> Result<DownloadResponse> {
    let normalized = Url::parse(input_url)?.to_string();
    upsert_user(&config, platform, platform_user_id, UserRole::None)?;

    let started = Instant::now();
    let item_dir = config.download_dir.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&item_dir)?;
    let out_tmpl = item_dir.join("%(title).100B-%(id)s.%(ext)s");

    let status = TokioCommand::new("yt-dlp")
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
        .status()
        .await?;

    if !status.success() {
        return Err(anyhow!("yt-dlp failed with status: {status}"));
    }
    let download_ms = started.elapsed().as_millis() as i64;

    let files: Vec<String> = fs::read_dir(&item_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x == "mp4" || x == "jpg" || x == "png")
        })
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let description = read_sidecar_text(&item_dir).unwrap_or_else(|| "(no description)".into());
    let convert_ms = 0_i64;

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

fn read_sidecar_text(dir: &Utf8PathBuf) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "description"))
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
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
