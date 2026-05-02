# Discord Gateway — Implementation Plan

**Module:** `src/discord/`
**Responsibility:** Connect to Discord, handle events, post messages, manage button interactions.

## Implementation Details

- Uses `serenity 0.12.5` with the `client`, `gateway`, `cache`, and `builder` features.
- Implements `EventHandler` trait with handlers for:
  - `message` — new message in any channel the bot can see
  - `interaction_create` — button clicks (inbox approve/reject, domain approval)
  - `ready` — bot connected, log startup
- Channel-to-project resolution: maintains a `HashMap<ChannelId, String>` mapping channels to project names, built at startup by scanning the guild's channels and matching against known projects.

## Key Functions

```rust
// discord/handler.rs

#[async_trait]
impl EventHandler for GlassHandler {
    async fn message(&self, ctx: Context, msg: SerenityMessage) {
        // 1. Ignore bot's own messages
        // 2. Resolve channel_id → project name
        // 3. If no matching project, ignore (or respond with "unknown channel")
        // 4. Spawn invocation task (don't block the event handler)
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        // 1. Match on component interaction (button click)
        // 2. Parse custom_id to determine action type:
        //    - "inbox_approve:{id}" → inbox::approval::approve(id)
        //    - "inbox_reject:{id}" → inbox::approval::reject(id)
        //    - "domain_approve:{project}:{domain}" → capabilities::allowlist::approve(...)
        //    - "domain_reject:{project}:{domain}" → capabilities::allowlist::reject(...)
        // 3. Acknowledge interaction, update the message to reflect the action
    }
}
```

## Message Posting

These are internal helper functions used by the `SerenityDiscord` implementation of `DiscordSink`. Code outside the `discord/` module calls `&dyn DiscordSink` methods instead — never these functions directly.

```rust
// discord/mod.rs (internal to SerenityDiscord impl)

/// Send a text message to a channel, splitting if > 2000 chars.
pub(crate) async fn send_message(
    http: &Http,
    channel_id: ChannelId,
    content: &str,
) -> Result<()>;

/// Send a message with button components (for inbox review, domain approval).
/// Converts ReviewButton → serenity CreateButton internally.
pub(crate) async fn send_review_message(
    http: &Http,
    channel_id: ChannelId,
    content: &str,
    buttons: Vec<CreateButton>,
) -> Result<()>;
```

## Discord Channel Conventions

| Channel | Purpose | Bot behavior |
|---------|---------|-------------|
| `#glass` | Root project | Agent invoked with root context |
| `#audit-log` | Audit summaries | Bot posts here, never reads |
| `#<project>` | Project channel | Agent invoked with project context |