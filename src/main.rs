use std::{sync::atomic::Ordering, time::Duration};

use anyhow::{anyhow, Result};
use defaultmap::DefaultHashMap;
use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use lazy_static::lazy_static;
use serde::Deserialize;
use serde_json;
use sqlx::{migrate, query, sqlite};
use std::sync::atomic::AtomicBool;
use teloxide::{
    payloads::SendMessageSetters, prelude::*, types::ParseMode, utils::command::BotCommand,
    RequestError,
};
use tokio::time::sleep;
use zmq;

const BOT_NAME: &str = "AOSC 第二包通委";
const LIST_MAX_SIZE: usize = 30;
// The maximum size of a Telegram message is 4096 chars. 4000 is just for the safety.
const LIST_MAX_LENGTH: isize = 4000;
const COOLDOWN_TIME: usize = 20usize;

type EntryMapping = DefaultHashMap<String, Vec<String>>;

lazy_static! {
    static ref UPDATED: AtomicBool = AtomicBool::new(false);
}

macro_rules! send_to_subscribers {
    ($c:expr, $bot:ident, $subs:ident) => {
        for sub in $subs.iter() {
            if let Err(e) = send_with_retry($c, $bot, sub.chat_id).await {
                log::error!("{}", e);
            }
        }
    };
}

#[derive(BotCommand)]
#[command(rename = "lowercase", description = "These commands are supported:")]
enum Command {
    #[command(description = "display this text.")]
    Help,
    #[command(description = "subscribe to updates.")]
    Start,
    #[command(description = "unsubscribe.")]
    Stop,
    #[command(description = "ping.")]
    Ping,
    #[command(description = "display the `chat_id` of this chat.")]
    ChatID,
}

#[derive(Deserialize, Clone, Debug)]
struct PVMessage {
    comp: String,
    pkg: String,
    arch: String,
    method: String,
    from_ver: Option<String>,
    to_ver: Option<String>,
}

impl PVMessage {
    fn to_html(&self) -> String {
        match self.method.as_str() {
            "new" => format!(
                r#"<code> +</code> <a href="https://packages.aosc.io/packages/{}">{}</a> <code>{}</code>"#,
                self.pkg,
                self.pkg,
                self.to_ver.as_ref().unwrap_or(&"?".to_string())
            ),
            "upgrade" => format!(
                r#"<code> ^</code> <a href="https://packages.aosc.io/packages/{}">{}</a> <code>{}</code> ⇒ <code>{}</code>"#,
                self.pkg,
                self.pkg,
                self.from_ver.as_ref().unwrap_or(&"?".to_string()),
                self.to_ver.as_ref().unwrap_or(&"?".to_string())
            ),
            "delete" => format!(
                r#"<code> -</code> <a href="https://packages.aosc.io/packages/{}">{}</a> <code>{}</code>"#,
                self.pkg,
                self.pkg,
                self.from_ver.as_ref().unwrap_or(&"?".to_string())
            ),
            "overwrite" => format!(
                r#"<code> *</code> <a href="https://packages.aosc.io/packages/{}">{}</a> <code>{}</code>"#,
                self.pkg,
                self.pkg,
                self.from_ver.as_ref().unwrap_or(&"?".to_string())
            ),
            _ => format!(
                r#"<code> ?</code> <a href="https://packages.aosc.io/packages/{}">{}</a> Unknown operation"#,
                self.pkg, self.pkg,
            ),
        }
    }
}

fn connect_zmq(endpoint: &str) -> Result<zmq::Socket> {
    let ctx = zmq::Context::new();
    let sock = ctx.socket(zmq::SUB)?;
    sock.connect(endpoint)?;
    sock.set_subscribe(b"")?;

    Ok(sock)
}

#[inline]
fn method_to_priority(v: &PVMessage) -> u8 {
    match v.method.as_ref() {
        "delete" => 0,
        "new" => 1,
        "overwrite" => 2,
        "upgrade" => 3,
        _ => 99,
    }
}

/// Sort the messages by priority and then truncate them to the given length
fn sort_pending_messages_chunk(pending: &mut Vec<PVMessage>) -> EntryMapping {
    let mut mapping = DefaultHashMap::new(vec![]);
    let mut remaining = LIST_MAX_LENGTH;
    mapping.reserve(LIST_MAX_SIZE);
    pending.sort_unstable_by(|a, b| method_to_priority(&a).cmp(&method_to_priority(&b)));
    while !pending.is_empty() && remaining > 0 {
        let p = pending.pop();
        if p.is_none() {
            break;
        }
        let p = p.unwrap();
        let html = p.to_html();
        let len = html.len();
        mapping[format!("<b>{}</b> {}\n", p.comp, p.arch)].push(html);
        remaining -= len as isize;
    }

    mapping
}

fn format_sorted_mapping(mapping: EntryMapping) -> String {
    let mut output = String::new();
    output.reserve(4096);
    for (k, v) in mapping.iter() {
        output += k;
        output += &v.join("\n");
    }

    output
}

#[inline]
async fn send_with_retry(msg: &str, bot: &AutoSend<Bot>, chat_id: i64) -> Result<()> {
    let mut retries = 5usize;
    let mut chat_id = chat_id;
    while retries > 0 {
        let result = bot
            .send_message(chat_id, msg)
            .parse_mode(ParseMode::Html)
            .await;
        if let Err(e) = result {
            retries -= 1;
            match e {
                RequestError::RetryAfter(t) => {
                    log::warn!("Rate limited, will retry after {} seconds", t);
                    if t > 0 {
                        sleep(Duration::from_secs(t as u64)).await;
                    } else {
                        // the fuck?
                        sleep(Duration::from_secs(60)).await;
                    }
                }
                RequestError::MigrateToChatId(id) => {
                    log::warn!("Chat ID {} changed to {}", chat_id, id);
                    chat_id = id;
                }
                _ => {
                    log::warn!("Unexpected error occurred ({:?}), retrying ...", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        } else {
            return Ok(());
        }
    }

    Err(anyhow!("Failed to send message to {}", chat_id))
}

/// Send all the pending messages to the subscribers
async fn send_all_pending_messages(
    pending: &mut Vec<PVMessage>,
    bot: &AutoSend<Bot>,
    db: &sqlite::SqlitePool,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let subs = query!("SELECT chat_id FROM subbed").fetch_all(db).await?;
    UPDATED.fetch_or(true, Ordering::SeqCst);
    while !pending.is_empty() {
        let sorted = sort_pending_messages_chunk(pending);
        let formatted = format_sorted_mapping(sorted);
        send_to_subscribers!(&formatted, bot, subs);
    }

    Ok(())
}

/// Monitor the ZMQ endpoint of p-vector
async fn monitor_pv(sock: zmq::Socket, bot: &AutoSend<Bot>, db: &sqlite::SqlitePool) -> Result<()> {
    let mut fail_count = 0usize;
    let mut pending = Vec::new();
    let mut pending_time = COOLDOWN_TIME;
    loop {
        let payload = sock.recv_bytes(zmq::DONTWAIT);
        match payload {
            Ok(msg) => {
                let msg = serde_json::from_slice::<PVMessage>(&msg);
                if let Ok(msg) = msg {
                    pending.push(msg);
                    pending_time = COOLDOWN_TIME; // reset the pending time
                } else {
                    log::warn!("Invalid message received.");
                    fail_count += 1;
                    if fail_count > 10 {
                        log::error!("Too many errors encountered. Stopped monitoring ZMQ!");
                        // Flush all the pending messages and then return
                        send_all_pending_messages(&mut pending, bot, db).await.ok();
                        return Err(anyhow!("Too many errors encountered"));
                    }
                }
            }
            Err(e) => {
                if pending_time < 1 {
                    // accumulate enough pending messages to send
                    send_all_pending_messages(&mut pending, bot, db).await.ok();
                    pending_time = COOLDOWN_TIME; // reset the pending time
                    continue;
                }
                pending_time -= 1;
                if e == zmq::Error::EAGAIN {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                } else {
                    log::error!("Error occurred while receiving zmq message: {}", e);
                    fail_count += 1;
                    if fail_count > 10 {
                        log::error!("Too many errors encountered. Stopped monitoring ZMQ!");
                        // Flush all the pending messages and then return
                        send_all_pending_messages(&mut pending, bot, db).await.ok();
                        return Err(anyhow!("Too many errors encountered"));
                    }
                }
            }
        }
    }
}

/// Monitor the `last_update` file
async fn monitor_last_update(f: &str, bot: &AutoSend<Bot>, db: &sqlite::SqlitePool) -> Result<()> {
    let mut inotify = Inotify::init()?;
    let mut buffer = [0; 32];
    inotify.add_watch(f, WatchMask::CREATE | WatchMask::MODIFY)?;
    let mut stream = inotify.event_stream(&mut buffer)?;
    log::info!("Last update file monitoring started.");
    while let Some(_) = stream.next().await {
        // Only sends this notification if there are package updates
        if !UPDATED.fetch_and(false, Ordering::SeqCst) {
            continue;
        }
        let subs = query!("SELECT chat_id FROM subbed").fetch_all(db).await?;
        send_to_subscribers!("🔄 Repository refreshed.", bot, subs);
    }

    Ok(())
}

/// Handle bot commands from Telegram
async fn answer(
    cx: UpdateWithCx<AutoSend<Bot>, Message>,
    command: Command,
    pool: sqlite::SqlitePool,
) -> Result<()> {
    match command {
        Command::Help => cx.reply_to(Command::descriptions()).send().await?,
        Command::Start => {
            let id = cx.chat_id();
            query!("INSERT OR IGNORE INTO subbed (chat_id) VALUES (?)", id)
                .execute(&pool)
                .await?;
            cx.reply_to("Subscribed to updates.").await?
        }
        Command::Stop => {
            let id = cx.chat_id();
            query!("DELETE FROM subbed WHERE chat_id = ?", id)
                .execute(&pool)
                .await?;
            cx.reply_to("Unsubbed.").await?
        }
        Command::Ping => cx.reply_to("Pong!").await?,
        Command::ChatID => cx.reply_to(format!("{}", cx.chat_id())).await?,
    };

    Ok(())
}

async fn run() -> Result<()> {
    let pool = sqlite::SqlitePool::connect(&std::env::var("DATABASE_URL").unwrap()).await?;
    migrate!().run(&pool).await?;
    let zmq_addr =
        std::env::var("ZMQ_ENDPOINT").expect("Please set ZMQ_ENDPOINT environment variable!");
    teloxide::enable_logging!();
    log::info!("Starting bot...");

    let rx = connect_zmq(&zmq_addr).expect("Unable to connect to zmq endpoint!");
    log::info!("ZMQ connected.");
    let bot = Bot::from_env().auto_send();
    log::info!("Bot connected.");
    let pool_clone = pool.clone();
    tokio::try_join!(
        async {
            Ok(
                teloxide::commands_repl(bot.clone(), BOT_NAME, move |cx, cmd| {
                    answer(cx, cmd, pool_clone.clone())
                })
                .await,
            )
        },
        monitor_pv(rx, &bot, &pool),
        async {
            let path = std::env::var("LAST_UPDATE");
            if let Ok(path) = path {
                Ok(monitor_last_update(&path, &bot, &pool).await.ok())
            } else {
                log::warn!("Not monitoring last update file.");
                Ok(None)
            }
        }
    )
    .ok();
    log::error!("Stopping bot ...");

    Err(anyhow!("Bot exited due to an error."))
}

#[tokio::main]
async fn main() {
    run().await.unwrap();
}
