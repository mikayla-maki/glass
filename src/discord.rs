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
        if let Err(e) = self
            .tx
            .send(IncomingDm {
                author: AuthorId(msg.author.id.get()),
                channel: ConversationId(msg.channel_id.get()),
                content: msg.content,
            })
            .await
        {
            error!("dropped DM (bus closed): {e}");
        }
    }
}

pub async fn connect(token: &str) -> Result<(SerenityBus, JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel(64);
    let intents = GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(token, intents)
        .event_handler(Forwarder { tx })
        .await?;

    let http = client.http.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = client.start().await {
            error!("serenity client exited: {e:#}");
        }
    });

    Ok((
        SerenityBus {
            http,
            rx: Mutex::new(rx),
        },
        handle,
    ))
}
