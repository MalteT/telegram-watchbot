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

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::time::Duration;

type DBData = HashMap<UserId, Vec<(String, UrlStatus)>>;
type DB = FileDatabase<DBData, Ron>;

pub enum WakerOrUpdate {
    Waker(Instant),
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

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum UrlStatus {
    Code(u16),
    Error(String),
    Unknown,
}

/// Collection of all important states
pub struct Pool {
    pub api: Api,
    pub db: DB,
    pub client: Client,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Fetch environment
    let token = env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set");
    let db_path = env::var("WATCHBOT_DB").expect("WATCHBOT_DB not set");

    // Open the database
    let db = DB::load_from_path_or_default(db_path).expect("Database loading failed");

    // Initialize things
    let api = Api::new(token);
    let telegram_update_stream = api.stream().map(WakerOrUpdate::TelegramUpdate);
    let ping_waker = tokio::time::interval(Duration::from_secs(60)).map(WakerOrUpdate::Waker);
    let mut events = futures::stream::select(telegram_update_stream, ping_waker);
    let client = ClientBuilder::new()
        .timeout(Duration::from_secs(10))
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

async fn update_url_status(url: &str, pool: &Pool, status: &mut UrlStatus) -> Option<String> {
    println!("  Checking {}", url);
    let new_status = match pool.client.head(url).send().await {
        Ok(response) => {
            let status = response.status();
            UrlStatus::Code(status.as_u16())
        }
        Err(e) => UrlStatus::Error(e.to_string()),
    };
    let optional_text = if *status != new_status {
        let text = format!("{} {} [{}]", new_status.as_emoji(), url, new_status);
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
    let text = pool.db.read(|list| {
        if let Some(urls) = list.get(user) {
            urls.iter()
                .map(|(url, status)| format!("{} {} [{}]\n", status.as_emoji(), url, status))
                .fold(String::new(), |s, item| s + &item)
        } else {
            "Poor fella.. 🤷‍♀️ You should /add some_url!".to_owned()
        }
    })?;
    let message = SendMessage::new(user, text);
    pool.api.send(message).await.ok();

    Ok(())
}

async fn add_url_from(url: &str, message: &Message, pool: &Pool) -> Result<(), Error> {
    let db_update_result = pool.db.write(|list| {
        let urls = list.entry(message.from.id).or_default();
        urls.push((url.into(), UrlStatus::Unknown));
    });
    if db_update_result.is_ok() {
        pool.api
            .send(message.text_reply(format!("{} added to watchlist", url)))
            .await?;
    }
    Ok(())
}

async fn update_url_states_and_notify_users(pool: &Pool) -> Result<(), Error> {
    let mut list = pool.db.borrow_data_mut()?;
    // Iterate over all users
    for (user, watchlist) in &mut *list {
        // Assemble a stream of requests
        let updates: FuturesUnordered<_> = watchlist
            .iter_mut()
            .map(|(url, ref mut status)| update_url_status(url, pool, status))
            .collect();
        // Filter all updates that returned None and chunk the rest into eadible pieces.
        // Then inform the user about the updates
        updates
            .filter_map(future::ready)
            .chunks(10)
            .for_each(|mut updates| {
                let text = updates
                    .drain(..)
                    .fold(String::from("Response changed:\n"), |s, elem| s + &elem);
                let message = SendMessage::new(user, text);
                pool.api.send(message).map(|_| ())
            })
            .await;
    }

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
