use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderMap, HeaderValue, PRAGMA,
    UPGRADE_INSECURE_REQUESTS, USER_AGENT,
};
use reqwest::{Client as HttpClient, Proxy as HttpProxy, Url};
use scraper::{Html, Selector};
use serenity::all::ChannelId;
use serenity::all::UserId;
use serenity::http::Http as DiscordHttp;
use serenity::prelude::Mentionable;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::sleep;

type AppResult<T> = Result<T, Box<dyn Error>>;

const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
const DEFAULT_STATE_FILE: &str = "chapter_state.txt";
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";
const DEFAULT_STARTUP_RETRY_SECS: u64 = 15;

struct Config {
    discord_token: String,
    channel_id: ChannelId,
    url: Url,
    poll_interval: Duration,
    proxy_url: Option<String>,
    state_file: PathBuf,
}

impl Config {
    fn from_env() -> AppResult<Self> {
        let discord_token = read_first_env(&["TOKEN", "PRIVATE_KEY"])?;
        let channel_raw = read_first_env(&["CHANNEL", "CHANNEL_ID"])?;
        let channel_id = ChannelId::new(channel_raw.parse()?);

        let url = Url::parse(&env::var("URL")?)?;
        let poll_interval = Duration::from_secs(read_poll_interval_secs()?);
        let proxy_url = read_optional_env("PROXY_URL");
        let state_file = PathBuf::from(
            env::var("STATE_FILE").unwrap_or_else(|_| DEFAULT_STATE_FILE.to_string()),
        );

        Ok(Self {
            discord_token,
            channel_id,
            url,
            poll_interval,
            proxy_url,
            state_file,
        })
    }
}

#[tokio::main]
async fn main() -> AppResult<()> {
    dotenv::dotenv().ok();

    let config = Config::from_env()?;
    let web_client = build_web_client(config.proxy_url.as_deref())?;
    let discord_http = DiscordHttp::new(&config.discord_token);
    let mut last_chapter = initialize_chapter_state(&config, &web_client).await?;

    loop {
        sleep(config.poll_interval).await;

        match load_chapter_state(&config.state_file) {
            Ok(Some(saved_chapter)) if saved_chapter != last_chapter => {
                println!(
                    "External state update detected: {} -> {}",
                    last_chapter, saved_chapter
                );
                last_chapter = saved_chapter;
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = save_chapter_state(&config.state_file, last_chapter) {
                    eprintln!("Failed to recreate chapter state: {error}");
                }
            }
            Err(error) => {
                eprintln!("Failed to reload chapter state: {error}");
            }
        }

        let current_chapter = match fetch_chapter_number(&web_client, config.url.clone()).await {
            Ok(chapter) => chapter,
            Err(error) => {
                eprintln!("Fetch failed: {error}");
                continue;
            }
        };

        if current_chapter > last_chapter {
            match notify_new_chapters(
                &discord_http,
                config.channel_id,
                last_chapter,
                current_chapter,
            )
            .await
            {
                Ok(()) => {
                    last_chapter = current_chapter;

                    if let Err(error) = save_chapter_state(&config.state_file, last_chapter) {
                        eprintln!("Failed to persist chapter state: {error}");
                    }

                    println!("New chapter detected: {}", last_chapter);
                }
                Err(error) => {
                    eprintln!("Discord notification failed: {error}");
                }
            }

            continue;
        }

        if current_chapter < last_chapter {
            eprintln!(
                "Chapter number moved backwards from {} to {}. Resetting local state.",
                last_chapter, current_chapter
            );

            last_chapter = current_chapter;

            if let Err(error) = save_chapter_state(&config.state_file, last_chapter) {
                eprintln!("Failed to persist chapter state: {error}");
            }
        }
    }
}

fn build_web_client(proxy_url: Option<&str>) -> AppResult<HttpClient> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        USER_AGENT,
        HeaderValue::from_static(DEFAULT_USER_AGENT),
    );
    default_headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    default_headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9,fr-FR;q=0.8,fr;q=0.7"),
    );
    default_headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    default_headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    default_headers.insert(UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"));

    let mut builder = HttpClient::builder()
        .default_headers(default_headers)
        .timeout(Duration::from_secs(20));

    if let Some(proxy_url) = proxy_url {
        println!("Using configured proxy for chapter fetch.");
        builder = builder.proxy(HttpProxy::all(proxy_url)?);
    }

    Ok(builder.build()?)
}

async fn fetch_chapter_number(client: &HttpClient, url: Url) -> AppResult<u32> {
    let response = client
        .get(url)
        .send()
        .await?;

    let status = response.status();
    let html = response.text().await?;

    if !status.is_success() {
        return Err(io::Error::other(format_request_failure(status.as_u16(), &html)).into());
    }

    extract_chapter_number(&html)
}

fn extract_chapter_number(html: &str) -> AppResult<u32> {
    let document = Html::parse_document(html);
    let stats_selector = Selector::parse("div.header-stats span")?;
    let strong_selector = Selector::parse("strong")?;
    let small_selector = Selector::parse("small")?;

    for stat in document.select(&stats_selector) {
        let label = stat
            .select(&small_selector)
            .next()
            .map(|element| element.text().collect::<String>().trim().to_string());

        if label.as_deref() != Some("Chapters") {
            continue;
        }

        if let Some(strong) = stat.select(&strong_selector).next() {
            let raw = strong.text().collect::<String>();
            let digits: String = raw
                .chars()
                .filter(|character| character.is_ascii_digit())
                .collect();

            if digits.is_empty() {
                break;
            }

            return Ok(digits.parse()?);
        }
    }

    Err(io::Error::other("chapter count not found in page").into())
}

async fn notify_new_chapters(
    discord_http: &DiscordHttp,
    channel_id: ChannelId,
    previous_chapter: u32,
    current_chapter: u32,
) -> AppResult<()> {
    let new_chapters = current_chapter - previous_chapter;
    let user1 = UserId::new(529330119290912774);
    let user2 = UserId::new(745257659036598354);

    let message = format!(
    "{new_chapters} nouveau(x) chapitre(s) disponible(s). Dernier chapitre: {current_chapter}. {} {}",
    user1.mention(),
    user2.mention(),
);
    channel_id.say(discord_http, message).await?;
    Ok(())
}

fn save_chapter_state(path: &Path, chapter_number: u32) -> AppResult<()> {
    fs::write(path, chapter_number.to_string())?;
    Ok(())
}

fn format_request_failure(status: u16, body: &str) -> String {
    let preview: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect();

    format!(
        "request failed with HTTP {status}. Response preview: {preview}. If this stays on 403 from a VPS, the site is likely blocking the server IP or non-browser traffic."
    )
}

fn load_chapter_state(path: &Path) -> AppResult<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let trimmed = contents.trim();

            if trimmed.is_empty() {
                return Ok(None);
            }

            Ok(Some(trimmed.parse()?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn initialize_chapter_state(config: &Config, client: &HttpClient) -> AppResult<u32> {
    match load_chapter_state(&config.state_file) {
        Ok(Some(saved_chapter)) => {
            println!(
                "Loaded saved chapter: {} ({})",
                saved_chapter,
                config.state_file.display()
            );
            Ok(saved_chapter)
        }
        Err(error) => {
            eprintln!("Failed to load chapter state: {error}. Trying fresh fetch.");
            initialize_chapter_state_from_fetch(config, client).await
        }
        Ok(None) => initialize_chapter_state_from_fetch(config, client).await,
    }
}

async fn initialize_chapter_state_from_fetch(
    config: &Config,
    client: &HttpClient,
) -> AppResult<u32> {
    loop {
        match fetch_chapter_number(client, config.url.clone()).await {
            Ok(fetched_chapter) => {
                save_chapter_state(&config.state_file, fetched_chapter)?;
                println!(
                    "Initial chapter saved: {} ({})",
                    fetched_chapter,
                    config.state_file.display()
                );
                return Ok(fetched_chapter);
            }
            Err(error) => {
                let delay = startup_retry_delay(config.poll_interval);
                eprintln!(
                    "Initial fetch failed: {error}. Retrying in {}s.",
                    delay.as_secs()
                );
                sleep(delay).await;
            }
        }
    }
}

fn read_first_env(keys: &[&str]) -> AppResult<String> {
    for key in keys {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }

    Err(io::Error::other(format!(
        "missing environment variable, expected one of: {}",
        keys.join(", ")
    ))
    .into())
}

fn read_optional_env(key: &str) -> Option<String> {
    env::var(key).ok().and_then(|value| {
        let trimmed = value.trim();

        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn read_poll_interval_secs() -> AppResult<u64> {
    let raw =
        env::var("POLL_INTERVAL_SECS").unwrap_or_else(|_| DEFAULT_POLL_INTERVAL_SECS.to_string());

    Ok(raw.parse()?)
}

fn startup_retry_delay(poll_interval: Duration) -> Duration {
    poll_interval.min(Duration::from_secs(DEFAULT_STARTUP_RETRY_SECS))
}

#[cfg(test)]
mod tests {
    use super::{
        extract_chapter_number, format_request_failure, load_chapter_state, read_optional_env,
        save_chapter_state, startup_retry_delay,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_state_file() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        std::env::temp_dir().join(format!("chapter_updater_state_{unique}.txt"))
    }

    #[test]
    fn extract_chapter_number_reads_header_stats() {
        let html = r#"
            <html>
                <body>
                    <div class="header-stats">
                        <span>
                            <small>Chapters</small>
                            <strong>512</strong>
                        </span>
                    </div>
                </body>
            </html>
        "#;

        let chapter_number = extract_chapter_number(html).unwrap();

        assert_eq!(chapter_number, 512);
    }

    #[test]
    fn save_and_load_chapter_state_roundtrip() {
        let path = temp_state_file();

        save_chapter_state(&path, 2913).unwrap();

        let saved_chapter = load_chapter_state(&path).unwrap();

        assert_eq!(saved_chapter, Some(2913));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_chapter_state_returns_none_when_missing() {
        let path = temp_state_file();

        let saved_chapter = load_chapter_state(&path).unwrap();

        assert_eq!(saved_chapter, None);
    }

    #[test]
    fn format_request_failure_includes_status_and_body_preview() {
        let message = format_request_failure(403, "<html> blocked by upstream protection </html>");

        assert!(message.contains("HTTP 403"));
        assert!(message.contains("blocked by upstream protection"));
    }

    #[test]
    fn read_optional_env_ignores_missing_or_blank_values() {
        unsafe {
            std::env::remove_var("PROXY_URL");
        }
        assert_eq!(read_optional_env("PROXY_URL"), None);

        unsafe {
            std::env::set_var("PROXY_URL", "   ");
        }
        assert_eq!(read_optional_env("PROXY_URL"), None);

        unsafe {
            std::env::set_var("PROXY_URL", "socks5h://127.0.0.1:1080");
        }
        assert_eq!(
            read_optional_env("PROXY_URL"),
            Some("socks5h://127.0.0.1:1080".to_string())
        );

        unsafe {
            std::env::remove_var("PROXY_URL");
        }
    }

    #[test]
    fn startup_retry_delay_caps_at_fifteen_seconds() {
        assert_eq!(startup_retry_delay(Duration::from_secs(5)), Duration::from_secs(5));
        assert_eq!(
            startup_retry_delay(Duration::from_secs(60)),
            Duration::from_secs(15)
        );
    }
}
