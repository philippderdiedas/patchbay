// SPDX-License-Identifier: GPL-3.0-or-later
//! The interface every chat platform adapter implements.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{Action, Audio, AudioFormat, Dest, MessageRef, OutMessage, Reply};

/// What a platform can do; the bot adapts its messages (e.g. no buttons → commands in the text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub buttons: bool,
    pub edit: bool,
    /// the bot can delete its own messages (voicemail posts of deleted voicemails)
    pub delete: bool,
    pub audio: Option<AudioFormat>,
    /// reactions on messages arrive as inbound events (Matrix without buttons)
    pub reactions: bool,
    pub slash_commands: bool,
    /// the platform can tell the e-mail address of a chat user (for `auto` linking)
    pub identity_email: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboundKind {
    /// command text without the slash command itself ("dnd on", "call 0401234")
    Command(String),
    /// a button was clicked; `message` is the message it belongs to
    Action { action: Action, message: Option<MessageRef> },
    Reaction { message: MessageRef, emoji: String },
}

/// Something a chat user sent to the bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inbound {
    pub platform: &'static str,
    pub chat_user: String,
    pub chat_user_name: String,
    /// channel/room it came from
    pub channel: String,
    /// direct message with the bot (false: a channel/group room)
    pub direct: bool,
    pub kind: InboundKind,
}

/// The bot as seen by an adapter: hands inbound messages to it and gets the reply.
#[async_trait]
pub trait InboundHandler: Send + Sync {
    async fn handle(&self, inbound: Inbound) -> Reply;
}

#[async_trait]
pub trait ChatPlatform: Send + Sync {
    /// stable id, used as key in the database ("mattermost", "telegram", "matrix")
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    async fn send(&self, to: &Dest, msg: &OutMessage) -> Result<MessageRef>;
    async fn edit(&self, msg: &MessageRef, new: &OutMessage) -> Result<()>;
    async fn send_audio(&self, to: &Dest, msg: &OutMessage, audio: &Audio) -> Result<MessageRef>;
    /// Deletes a message the bot sent (a message that is gone already counts as deleted).
    async fn delete(&self, _msg: &MessageRef) -> Result<()> {
        anyhow::bail!("deleting messages is not supported on {}", self.id())
    }
    /// e-mail address of a chat user, if the platform knows and shows it
    async fn user_email(&self, chat_user: &str) -> Result<Option<String>>;
    /// Receives inbound messages (WebSocket, long polling …) until the process ends; reconnects
    /// itself. Platforms whose inbound traffic arrives only over HTTP callbacks return at once.
    async fn run(self: Arc<Self>, handler: Arc<dyn InboundHandler>);
}
