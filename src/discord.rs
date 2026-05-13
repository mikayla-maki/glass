use crate::bus::{chunk_message, AuthorId, ConversationId, IncomingDm, MessageBus};
use anyhow::Result;
use async_trait::async_trait;
use serenity::builder::GetMessages;
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId, UserId};
use serenity::prelude::*;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Upper bound on how many missed DMs we pull at startup. Discord's
/// `GET /channels/:id/messages` returns up to 100 per call, and we don't
/// paginate — for a bot that's been offline for a very long stretch, the
/// older end of the backlog is silently dropped. That's the right tradeoff:
/// recent context matters more than ancient, and a runaway replay loop
/// (200 missed messages = 200 sequential turns = hours) would lock the bot
/// in catch-up mode.
const CATCHUP_LIMIT: u8 = 100;

pub struct SerenityBus {
    http: Arc<Http>,
    rx: Mutex<mpsc::Receiver<IncomingDm>>,
}

#[async_trait]
impl MessageBus for SerenityBus {
    async fn next(&self) -> Option<IncomingDm> {
        self.rx.lock().await.recv().await
    }

    async fn reply(&self, channel: ConversationId, content: &str) -> Result<()> {
        let serenity_channel = ChannelId::new(channel.0);
        for chunk in chunk_message(content, 1900) {
            serenity_channel.say(&self.http, chunk).await?;
        }
        Ok(())
    }

    async fn typing(&self, channel: ConversationId) -> Result<()> {
        let serenity_channel = ChannelId::new(channel.0);
        serenity_channel.broadcast_typing(&self.http).await?;
        Ok(())
    }
}

struct Forwarder {
    tx: mpsc::Sender<IncomingDm>,
}

#[serenity::async_trait]
impl EventHandler for Forwarder {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("connected as {}", ready.user.name);
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        if msg.guild_id.is_some() {
            return;
        }
        let dm = incoming_dm_from_message(&msg);
        if let Err(e) = self.tx.send(dm).await {
            error!("dropped DM (bus closed): {e}");
        }
    }
}

/// Translate a serenity `Message` (live or catch-up) into an `IncomingDm`.
/// Centralized so the gateway path and the startup-replay path produce
/// identical shapes.
fn incoming_dm_from_message(msg: &Message) -> IncomingDm {
    // Discord records the send time as UTC; convert to local so it matches
    // the `The current time is …` line in the system prompt and any `when`
    // values the agent generates for the schedule tool. The bus formats it
    // into a `[YYYY-MM-DD HH:MM]` prefix on the prompt the agent sees.
    let timestamp = (*msg.timestamp).with_timezone(&chrono::Local);
    IncomingDm {
        author: AuthorId(msg.author.id.get()),
        channel: ConversationId(msg.channel_id.get()),
        content: msg.content.clone(),
        timestamp,
        message_id: msg.id.get(),
    }
}

/// Connected Discord client: a bus to drive the DM loop, the operator's
/// resolved DM channel (so unsolicited cron-side `send_dm` calls have a
/// target), a count of messages replayed from catch-up (zero on first run
/// or when the bot was offline through no new DMs), and a join handle on
/// the underlying serenity gateway task.
pub struct Connected {
    pub bus: SerenityBus,
    pub operator_channel: ConversationId,
    pub catchup_count: usize,
    pub gateway: JoinHandle<()>,
}

pub async fn connect(
    token: &str,
    operator: AuthorId,
    last_dm_id: Option<u64>,
) -> Result<Connected> {
    let (tx, rx) = mpsc::channel(256);
    let intents = GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(token, intents)
        .event_handler(Forwarder { tx: tx.clone() })
        .await?;

    let http = client.http.clone();

    // Resolve / open the DM channel to the operator so the orchestrator can
    // initiate sends (e.g., from cron) without waiting for an inbound DM
    // first. Discord guarantees one DM channel per user pair; this returns
    // the existing one if it exists.
    let private = UserId::new(operator.0).create_dm_channel(&http).await?;
    let operator_channel = ConversationId(private.id.get());

    // Catch-up replay: if we have a stored last-seen message id, fetch
    // anything newer from the DM channel and push it onto the bus's
    // channel before the gateway starts handing in live messages. Bus
    // processes them sequentially through the dispatcher, just like live
    // DMs. Bounded by CATCHUP_LIMIT (Discord caps at 100/page; we don't
    // paginate).
    let catchup_count = if let Some(last_id) = last_dm_id {
        match fetch_missed_dms(&http, operator_channel, operator, last_id).await {
            Ok(missed) => {
                let n = missed.len();
                for dm in missed {
                    if tx.send(dm).await.is_err() {
                        warn!("catch-up: bus closed before replay finished");
                        break;
                    }
                }
                n
            }
            Err(e) => {
                warn!("catch-up: failed to fetch missed DMs ({e:#}); proceeding without replay");
                0
            }
        }
    } else {
        0
    };

    let gateway = tokio::spawn(async move {
        if let Err(e) = client.start().await {
            error!("serenity client exited: {e:#}");
        }
    });

    Ok(Connected {
        bus: SerenityBus {
            http,
            rx: Mutex::new(rx),
        },
        operator_channel,
        catchup_count,
        gateway,
    })
}

/// Query Discord for DMs in `channel` newer than `after_id`, sorted oldest
/// → newest (replay order). Drops bot messages and messages from anyone
/// other than the operator — strangers can't sneak in via catch-up the way
/// they can't via live DMs either.
async fn fetch_missed_dms(
    http: &Arc<Http>,
    channel: ConversationId,
    operator: AuthorId,
    after_id: u64,
) -> Result<Vec<IncomingDm>> {
    let builder = GetMessages::new()
        .after(MessageId::new(after_id))
        .limit(CATCHUP_LIMIT);
    let mut messages = ChannelId::new(channel.0).messages(http, builder).await?;
    // Discord returns newest first; flip to chronological for replay.
    messages.reverse();
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages.iter() {
        if msg.author.bot {
            continue;
        }
        if msg.guild_id.is_some() {
            continue;
        }
        if msg.author.id.get() != operator.0 {
            continue;
        }
        if msg.content.trim().is_empty() {
            continue;
        }
        out.push(incoming_dm_from_message(msg));
    }
    Ok(out)
}
