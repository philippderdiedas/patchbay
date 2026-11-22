// SPDX-License-Identifier: GPL-3.0-or-later
//! Matrix adapter – not implemented yet (stub behind the `ChatPlatform` trait).
//!
//! Planned mapping (client-server API with a bot account and access token):
//! - inbound: `/sync` long polling; `m.room.message` (text) → command, `m.reaction` on a bot
//!   message → `InboundKind::Reaction` (Matrix has no buttons: the bot lists its actions as
//!   reactions/commands, e.g. "👍 = heard, 📞 = call back"; the bot core maps reactions per
//!   message once this adapter exists).
//! - `send`: `m.room.message` with `body` (plain, `OutMessage::plain`) and `formatted_body` (HTML);
//!   `edit`: `m.replace` relation; `send_audio`: upload to the media repo, `m.audio` (OGG/Opus).
//! - Dest::User: a DM room created with `is_direct` and stored per user; rooms are linked to
//!   call groups with `/pbx link-group` like Mattermost channels (admins only).
//! - identity: the e-mail address is not visible to other users; `link` mode (one-time codes).
//! - end-to-end encrypted rooms need matrix-sdk with crypto; plan for unencrypted bot rooms first.

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::{
    model::{Audio, AudioFormat, Dest, MessageRef, OutMessage},
    platform::{Capabilities, ChatPlatform, InboundHandler},
};

pub const ID: &str = "matrix";

pub struct Matrix {
    #[allow(dead_code)]
    homeserver: String,
    #[allow(dead_code)]
    token: String,
}

impl Matrix {
    pub fn new(homeserver: String, token: String) -> Arc<Self> {
        Arc::new(Matrix { homeserver, token })
    }
}

#[async_trait]
impl ChatPlatform for Matrix {
    fn id(&self) -> &'static str {
        ID
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { buttons: false, edit: true, audio: Some(AudioFormat::OggOpus), reactions: true, slash_commands: false, identity_email: false }
    }
    async fn send(&self, _to: &Dest, _msg: &OutMessage) -> Result<MessageRef> {
        bail!("Matrix adapter not implemented")
    }
    async fn edit(&self, _msg: &MessageRef, _new: &OutMessage) -> Result<()> {
        bail!("Matrix adapter not implemented")
    }
    async fn send_audio(&self, _to: &Dest, _msg: &OutMessage, _audio: &Audio) -> Result<MessageRef> {
        bail!("Matrix adapter not implemented")
    }
    async fn user_email(&self, _chat_user: &str) -> Result<Option<String>> {
        Ok(None)
    }
    async fn run(self: Arc<Self>, _handler: Arc<dyn InboundHandler>) {
        tracing::warn!("Matrix adapter not implemented; ignoring PBX_BOT_MATRIX_*");
    }
}
