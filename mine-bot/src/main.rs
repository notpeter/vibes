use std::{path::PathBuf, process::Stdio, sync::Arc, time::Instant};

use anyhow::{anyhow, Context, Result};
use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use dropshot::{endpoint, ApiDescription, ConfigDropshot, HttpError, HttpResponseCreated, HttpServerStarter, RequestContext, TypedBody};
use rusqlite::{params, Connection};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use teloxide::{
    prelude::*,
    types::{InputFile, InputMedia, InputMediaPhoto},
};
use tokio::{process::Command, task};
use tracing::{error, info};
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
struct Config {
    #[serde(default = "default_bind_address")]
    bind_address: String,
    database_path: Utf8PathBuf,
    download_dir: Utf8PathBuf,
    telegram: TelegramConfig,
    seed_users: Vec<SeedUser>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramConfig {
    enabled: bool,
    bot_token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SeedUser {
    platform: String,
    platform_user_id: String,
    role: UserRole,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

fn default_bind_address() -> String {
    "0.0.0.0:53211".to_string()
}

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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = std::env::var("MINE_BOT_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config_raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("unable to read config at {config_path}"))?;
    let config: Config = toml::from_str(&config_raw)?;

    std::fs::create_dir_all(&config.download_dir)?;
    init_db(&config)?;

    let app = Arc::new(AppState { config: config.clone() });

    if config.telegram.enabled {
        let tg_state = app.clone();
        tokio::spawn(async move {
            if let Err(e) = run_telegram(tg_state).await {
                error!("telegram adapter failed: {e:#}");
            }
        });
    }

    let mut api = ApiDescription::new();
    api.register(download_endpoint)?;

    let log = dropshot::ConfigLogging::StderrTerminal { level: dropshot::ConfigLoggingLevel::Info }
        .to_logger("mine-bot")
        .map_err(|e| anyhow!(e.to_string()))?;

    let server = HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: config.bind_address.parse()?,
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

#[endpoint {
    method = POST,
    path = "/download"
}]
async fn download_endpoint(
    rqctx: RequestContext<AppCtx>,
    body: TypedBody<DownloadRequest>,
) -> Result<HttpResponseCreated<DownloadResponse>, HttpError> {
    let app = rqctx.context();
    let result = download_media(app.config.clone(), "api", "api", &body.into_inner().url)
        .await
        .map_err(|e| HttpError::for_bad_request(None, format!("download failed: {e:#}")))?;

    Ok(HttpResponseCreated(result))
}

async fn run_telegram(state: AppCtx) -> Result<()> {
    let bot = Bot::new(state.config.telegram.bot_token.clone());

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let state = state.clone();
        async move {
            if let Some(text) = msg.text() {
                let user_id = msg.from.as_ref().map(|u| u.id.0.to_string()).unwrap_or_else(|| "unknown".into());
                let download = download_media(state.config.clone(), "telegram", &user_id, text).await;
                match download {
                    Ok(media) => {
                        bot.send_message(msg.chat.id, media.post_text.clone()).await?;
                        if media.files.len() == 1 {
                            bot.send_video(msg.chat.id, InputFile::file(PathBuf::from(&media.files[0]))).await?;
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
                    }
                    Err(e) => {
                        bot.send_message(msg.chat.id, format!("Download failed: {e:#}")).await?;
                    }
                }
            }
            respond(())
        }
    })
    .await;
    Ok(())
}

async fn download_media(config: Config, platform: &str, platform_user_id: &str, input_url: &str) -> Result<DownloadResponse> {
    let normalized = Url::parse(input_url)?.to_string();
    upsert_user(&config, platform, platform_user_id, UserRole::None)?;

    let started = Instant::now();
    let item_dir = config.download_dir.join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&item_dir)?;
    let out_tmpl = item_dir.join("%(title).100B-%(id)s.%(ext)s");

    let status = Command::new("yt-dlp")
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

    let files: Vec<String> = std::fs::read_dir(&item_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "mp4" || x == "jpg" || x == "png"))
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
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "description"))
        .and_then(|p| std::fs::read_to_string(p).ok())
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

fn upsert_user(config: &Config, platform: &str, platform_user_id: &str, role: UserRole) -> Result<()> {
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

fn insert_media_record(config: &Config, url: &str, name: &str, post_text: &str, download_ms: i64, convert_ms: i64) -> Result<i64> {
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
