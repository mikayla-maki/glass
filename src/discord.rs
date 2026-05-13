use crate::bus::{chunk_message, AuthorId, ConversationId, IncomingDm, MessageBus};
use anyhow::Result;
use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info};

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
        let serenity_channel = serenity::model::id::ChannelId::new(channel.0);
        for chunk in chunk_message(content, 1900) {
            serenity_channel.say(&self.http, chunk).await?;
        }
        Ok(())
    }

    async fn typing(&self, channel: ConversationId) -> Result<()> {
        let serenity_channel = serenity::model::id::ChannelId::new(channel.0);
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
        // Discord records the send time as UTC; convert to the orchestrator's
        // local timezone so it matches the `The current time is …` line in
        // the system prompt and any `when` values the agent generates for
        // the schedule tool. The bus formats it into a `[YYYY-MM-DD HH:MM]`
        // prefix on the prompt the agent sees.
        let timestamp = (*msg.timestamp).with_timezone(&chrono::Local);
        if let Err(e) = self
            .tx
            .send(IncomingDm {
                author: AuthorId(msg.author.id.get()),
                channel: ConversationId(msg.channel_id.get()),
                content: msg.content,
                timestamp,
            })
            .await
        {
            error!("dropped DM (bus closed): {e}");
        }
    }
}

/// Connected Discord client: a bus to drive the DM loop, the owner's
/// resolved DM channel (so unsolicited cron-side `send_dm` calls have a
/// target), and a join handle on the underlying serenity gateway task.
pub struct Connected {
    pub bus: SerenityBus,
    pub owner_channel: ConversationId,
    pub gateway: JoinHandle<()>,
}

pub async fn connect(token: &str, owner: AuthorId) -> Result<Connected> {
    let (tx, rx) = mpsc::channel(64);
    let intents = GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(token, intents)
        .event_handler(Forwarder { tx })
        .await?;

    let http = client.http.clone();

    // Resolve / open the DM channel to the owner so the orchestrator can
    // initiate sends (e.g., from cron) without waiting for an inbound DM
    // first. Discord guarantees one DM channel per user pair; this returns
    // the existing one if it exists.
    let private = serenity::model::id::UserId::new(owner.0)
        .create_dm_channel(&http)
        .await?;
    let owner_channel = ConversationId(private.id.get());

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
        owner_channel,
        gateway,
    })
}
