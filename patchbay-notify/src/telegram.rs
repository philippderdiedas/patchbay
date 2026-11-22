// SPDX-License-Identifier: GPL-3.0-or-later
//! Telegram adapter – not implemented yet (stub behind the `ChatPlatform` trait).
//!
//! Planned mapping (Bot API, https://core.telegram.org/bots/api):
//! - inbound: long polling `getUpdates` (no public endpoint needed); `message` → command text
//!   (`/pbx` prefix optional), `callback_query` → button action (answer with `answerCallbackQuery`).
//! - `send`: `sendMessage` with `parse_mode=HTML` (convert the Markdown subset), buttons as
//!   `reply_markup.inline_keyboard`. Callback data is limited to 64 bytes: store the serialised
//!   `Action` server-side and put a short random key into `callback_data`.
//! - `edit`: `editMessageText` (+ keyboard); `send_audio`: `sendVoice` with OGG/Opus
//!   (`AudioFormat::OggOpus`, patchbay-web converts with ffmpeg), caption = message text.
//! - Dest::User needs the numeric chat id: the private chat id equals the user id once the user
//!   has started the bot; groups are linked with `/pbx link-group` like Mattermost channels.
//! - identity: Telegram knows no e-mail addresses, so only `link` mode (one-time codes) works.
//! - privacy: messages pass through Telegram's servers; opt-in for staff (docs/architecture.md, chat bot).

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::{
    model::{Audio, AudioFormat, Dest, MessageRef, OutMessage},
    platform::{Capabilities, ChatPlatform, InboundHandler},
};

pub const ID: &str = "telegram";

pub struct Telegram {
    #[allow(dead_code)]
    token: String,
}

impl Telegram {
    pub fn new(token: String) -> Arc<Self> {
        Arc::new(Telegram { token })
    }
}

#[async_trait]
impl ChatPlatform for Telegram {
    fn id(&self) -> &'static str {
        ID
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { buttons: true, edit: true, audio: Some(AudioFormat::OggOpus), reactions: false, slash_commands: true, identity_email: false }
    }
    async fn send(&self, _to: &Dest, _msg: &OutMessage) -> Result<MessageRef> {
        bail!("Telegram adapter not implemented")
    }
    async fn edit(&self, _msg: &MessageRef, _new: &OutMessage) -> Result<()> {
        bail!("Telegram adapter not implemented")
    }
    async fn send_audio(&self, _to: &Dest, _msg: &OutMessage, _audio: &Audio) -> Result<MessageRef> {
        bail!("Telegram adapter not implemented")
    }
    async fn user_email(&self, _chat_user: &str) -> Result<Option<String>> {
        Ok(None)
    }
    async fn run(self: Arc<Self>, _handler: Arc<dyn InboundHandler>) {
        tracing::warn!("Telegram adapter not implemented; ignoring PBX_BOT_TELEGRAM_*");
    }
}
