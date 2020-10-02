#![feature(async_closure)]

use futures::{future, stream::FuturesUnordered, FutureExt, StreamExt};
use reqwest::{Client, ClientBuilder, Error as ReqwestError, StatusCode};
use rustbreak::{deser::Ron, FileDatabase, RustbreakError};
use serde::{Deserialize, Serialize};
use telegram_bot::{
    Api, CanReplySendMessage, Error as TelegramError, Message, MessageKind, SendMessage, Update,
    UpdateKind, UserId,
};
use thiserror::Error;
use tokio::time::Instant;

use std::{
    collections::{HashMap, HashSet},
    env, fmt,
    time::Duration,
};

/// The type for the data that is stored in the database.
type DBData = HashMap<String, (UrlStatus, HashSet<UserId>)>;
/// The database type itself for easy reference.
type DB = FileDatabase<DBData, Ron>;

/// Time to wait for the HEAD requests to finish.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);
/// Time between checking the urls.
const TIME_BETWEEN_UPDATES: Duration = Duration::from_secs(120);

/// Either type to merge the different streams
pub enum WakerOrUpdate {
    /// [`Instant`] from the [`tokio::time::interval`] stream.
    Waker(Instant),
    /// Update from the Telegram bot api stream.
    TelegramUpdate(Result<Update, TelegramError>),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Reqwest failed: {_0}")]
    Reqwest(#[source] ReqwestError),
    #[error("Telegram api failed: {_0}")]
    Telegram(#[source] TelegramError),
    #[error("Database error: {_0})")]
    Database(#[source] RustbreakError),
}

/// Status of a URL.
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum UrlStatus {
    /// The request returned some response code.
    Code(u16),
    /// The connection errored.
    Error(String),
    /// The status is yet to be checked.
    Unknown,
}

/// Collection of all important states
pub struct Pool {
    /// Telegram bot api
    pub api: Api,
    /// Database connection
    pub db: DB,
    /// request client
    pub client: Client,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Fetch environment
    let token = env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set");
    let db_path = env::var("WATCHBOT_DB").expect("WATCHBOT_DB not set");

    // Open the database
    let db = DB::load_from_path_or_default(db_path).expect("Database loading failed");

    // Initialize everything
    let api = Api::new(token);
    let telegram_update_stream = api.stream().map(WakerOrUpdate::TelegramUpdate);
    let ping_waker = tokio::time::interval(TIME_BETWEEN_UPDATES).map(WakerOrUpdate::Waker);
    let mut events = futures::stream::select(telegram_update_stream, ping_waker);
    let client = ClientBuilder::new()
        .timeout(CONNECTION_TIMEOUT)
        .build()
        .expect("infallible");
    let pool = Pool { api, db, client };

    // Fetch new updates via long poll method
    while let Some(event) = events.next().await {
        match event {
            WakerOrUpdate::Waker(_) => {
                match update_url_states_and_notify_users(&pool).await {
                    Ok(()) => println!("Updated URLs successfully"),
                    Err(e) => println!("{}", e),
                }
                pool.db.save()?;
            }
            WakerOrUpdate::TelegramUpdate(update) => {
                match handle_telegram_update(update, &pool).await {
                    Ok(()) => {}
                    Err(e) => println!("{}", e),
                }
            }
        }
    }
    Ok(())
}

async fn update_url_state(url: &str, pool: &Pool, status: &mut UrlStatus) -> Option<String> {
    let new_status = match pool.client.head(url).send().await {
        Ok(response) => {
            let status = response.status();
            UrlStatus::Code(status.as_u16())
        }
        Err(e) => UrlStatus::Error(e.to_string()),
    };
    let optional_text = if *status != new_status {
        println!("URL '{}' changed to '{}'", url, new_status);
        let text = format!("{} {} [{}]\n", new_status.as_emoji(), url, new_status);
        Some(text)
    } else {
        None
    };
    *status = new_status;
    optional_text
}

async fn handle_telegram_update(
    update: Result<Update, TelegramError>,
    pool: &Pool,
) -> Result<(), Error> {
    let update = update?;
    if let UpdateKind::Message(message) = update.kind {
        if let MessageKind::Text { ref data, .. } = message.kind {
            println!(
                "Got {} from {}({:?})",
                data, message.from.id, message.from.username
            );
            if data.starts_with("/add ") {
                let url = data.strip_prefix("/add ").expect("infallible").trim();
                add_url_from(url, &message, pool).await?;
            } else if data.trim() == "/list" {
                list_urls_by_user(&message.from.id, pool).await?;
            } else if data.trim() == "/help" || data.trim() == "/start" {
                send_help_to_user(&message.from.id, pool).await?;
            } else {
                pool.api
                    .send(message.text_reply("Come again? 🤷‍♀️\nTry to /add your_url!"))
                    .await?;
            }
        }
    }
    Ok(())
}

async fn send_help_to_user(user: &UserId, pool: &Pool) -> Result<(), Error> {
    let text = "Simple bot to notify you when your webpages go down\n/add URL to add a url to the watchlist\n/list to see your urls\n/help to see this help\n\n Have fun!";
    let message = SendMessage::new(user, text);
    pool.api.send(message).await.ok();

    Ok(())
}

async fn list_urls_by_user(user: &UserId, pool: &Pool) -> Result<(), Error> {
    let mut text = pool.db.read(|url_map| {
        url_map
            .iter()
            // Select all urls of the user
            .filter(|(_, (_, userids))| userids.contains(user))
            // Format the status
            .map(|(url, (status, _))| format!("{} {} [{}]\n", status.as_emoji(), url, status))
            // Collect it into one message
            .fold(String::new(), |s, item| s + &item)
    })?;
    if text.is_empty() {
        text = "Poor fella... Try /add some_url!".into();
    }
    let message = SendMessage::new(user, text);
    pool.api.send(message).await.ok();

    Ok(())
}

async fn add_url_from(url: &str, message: &Message, pool: &Pool) -> Result<(), Error> {
    let db_update_result = pool.db.write(|url_map| {
        url_map
            .entry(url.into())
            .or_insert((UrlStatus::Unknown, HashSet::new()))
            .1
            .insert(message.from.id);
    });
    if db_update_result.is_ok() {
        pool.api
            .send(message.text_reply(format!("{} added to watchlist", url)))
            .await?;
    }
    Ok(())
}

async fn update_url_states_and_notify_users(pool: &Pool) -> Result<(), Error> {
    let mut url_map = pool.db.borrow_data_mut()?;
    // Iterate over all users
    let updates: FuturesUnordered<_> = url_map
        .iter_mut()
        .map(|(url, (ref mut status, userids))| {
            update_url_state(url, pool, status).map(|text| (text, userids))
        })
        .collect();
    updates
        .filter_map(|(optional_text, userids)| {
            future::ready(optional_text.map(|text| (text, userids)))
        })
        .for_each(|(text, userids)| {
            let send_responses = userids.iter().map(|userid| {
                let message = SendMessage::new(userid, text.clone());
                pool.api.send(message)
            });
            future::join_all(send_responses).map(|_| ())
        })
        .await;
    Ok(())
}

impl fmt::Display for UrlStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UrlStatus::Unknown => write!(f, "???"),
            UrlStatus::Error(_) => write!(f, "Error"),
            UrlStatus::Code(code) => {
                let status = StatusCode::from_u16(*code).unwrap_or_default();
                status.fmt(f)
            }
        }
    }
}

impl UrlStatus {
    pub fn as_emoji(&self) -> &str {
        match self {
            UrlStatus::Unknown => "❔",
            UrlStatus::Error(_) => "⛈",
            UrlStatus::Code(code) => {
                let status = StatusCode::from_u16(*code).unwrap_or_default();
                if status.is_success() {
                    "☀️"
                } else if status.is_server_error() {
                    "⛈"
                } else if status.is_informational() {
                    "🌥"
                } else if status.is_client_error() {
                    "☁️"
                } else if status.is_redirection() {
                    "🌬"
                } else {
                    "🔥"
                }
            }
        }
    }
}

impl From<ReqwestError> for Error {
    fn from(re: ReqwestError) -> Self {
        Error::Reqwest(re)
    }
}

impl From<TelegramError> for Error {
    fn from(te: TelegramError) -> Self {
        Error::Telegram(te)
    }
}

impl From<RustbreakError> for Error {
    fn from(rbe: RustbreakError) -> Self {
        Error::Database(rbe)
    }
}
