// SPDX-License-Identifier: GPL-3.0-or-later
//! What the bot needs from the phone system. patchbay-web implements these traits with its own
//! functions (same permission checks as the web UI); tests use in-memory fakes.

use anyhow::Result;
use async_trait::async_trait;

use crate::{
    event::Presence,
    model::{Audio, AudioFormat},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PbxUser {
    pub id: i32,
    pub ext: String,
    pub name: String,
    pub admin: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PbxGroup {
    pub id: i32,
    pub number: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserStatus {
    pub dnd: bool,
    pub forward: Option<String>,
    /// after the ring time: "mailbox", "number" (see noanswer_value), "nothing" or "other"
    pub noanswer_mode: String,
    pub noanswer_value: Option<String>,
    pub ring_s: i16,
}

/// What happens when the user does not answer (set by the user, PBX_USER_NOANSWER).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoAnswerChoice {
    Mailbox,
    Number(String),
    Nothing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoicemailInfo {
    pub id: i32,
    /// "05.11.26 14:03"
    pub received: String,
    pub caller_number: Option<String>,
    pub caller_name: Option<String>,
    pub duration_s: i32,
    /// "1010" or "5110 Sales EN"
    pub mailbox: String,
    pub heard: bool,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub number: String,
    /// "colleague" or "contact"
    pub kind: &'static str,
    pub presence: Option<Presence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallListEntry {
    pub at: String,
    pub incoming: bool,
    pub answered: bool,
    pub number: String,
    pub name: String,
    pub duration_s: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmOp {
    Heard,
    Delete,
}

/// Personal setting: where new voicemails go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmDelivery {
    Mail,
    Chat,
    Both,
    None,
}

impl VmDelivery {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mail" => VmDelivery::Mail,
            "chat" => VmDelivery::Chat,
            "both" => VmDelivery::Both,
            "none" => VmDelivery::None,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            VmDelivery::Mail => "mail",
            VmDelivery::Chat => "chat",
            VmDelivery::Both => "both",
            VmDelivery::None => "none",
        }
    }
    pub fn chat(self) -> bool {
        matches!(self, VmDelivery::Chat | VmDelivery::Both)
    }
}

/// Phone system functions, always on behalf of a PBX user (their own permissions).
/// Errors meant for the user are returned as `Ok(Err(text))`.
#[async_trait]
pub trait Pbx: Send + Sync {
    async fn user(&self, uid: i32) -> Result<Option<PbxUser>>;
    async fn user_by_email(&self, email: &str) -> Result<Option<PbxUser>>;
    async fn user_id_by_ext(&self, ext: &str) -> Result<Option<i32>>;
    async fn group_by_number(&self, number: &str) -> Result<Option<PbxGroup>>;
    /// "5110 Sales EN"
    async fn group_label(&self, gid: i32) -> Result<Option<String>>;
    /// members of a call group (user ids, enabled memberships)
    async fn group_members(&self, gid: i32) -> Result<Vec<i32>>;
    async fn status(&self, uid: i32) -> Result<UserStatus>;
    async fn set_dnd(&self, uid: i32, on: bool) -> Result<bool>;
    async fn set_forward(&self, uid: i32, target: Option<&str>) -> Result<std::result::Result<(), String>>;
    /// own no-answer target and/or ring time; Err(reason) when refused
    async fn set_noanswer(&self, _uid: i32, _what: Option<NoAnswerChoice>, _ring_s: Option<i16>) -> Result<std::result::Result<(), String>> {
        Ok(Err("not supported".into()))
    }
    /// click-to-call: the user's devices ring, then `to` is dialled with the user's rights
    async fn originate(&self, uid: i32, to: &str) -> Result<std::result::Result<(), String>>;
    async fn voicemails(&self, uid: i32, limit: i64) -> Result<Vec<VoicemailInfo>>;
    /// one voicemail if the user may access it (None: not found / no access);
    /// `uid = None`: system access (delivery of new messages)
    async fn voicemail(&self, uid: Option<i32>, id: i32) -> Result<Option<VoicemailInfo>>;
    async fn voicemail_audio(&self, uid: Option<i32>, id: i32, format: AudioFormat) -> Result<Option<Audio>>;
    /// false: no access
    async fn voicemail_op(&self, uid: i32, id: i32, op: VmOp) -> Result<bool>;
    async fn directory(&self, uid: i32, query: &str) -> Result<Vec<DirectoryEntry>>;
    /// colleagues with their line state; empty query = everybody in the phone book
    async fn presence(&self, query: &str) -> Result<Vec<DirectoryEntry>>;
    async fn call_list(&self, uid: i32, limit: i64) -> Result<Vec<CallListEntry>>;
    async fn vm_delivery(&self, uid: i32) -> Result<VmDelivery>;
}

/// Bot state kept by patchbay-web in its database (links, group channels).
#[async_trait]
pub trait Store: Send + Sync {
    async fn linked_user(&self, platform: &str, chat_user: &str) -> Result<Option<i32>>;
    async fn chat_user_of(&self, platform: &str, uid: i32) -> Result<Option<String>>;
    /// `via`: "auto" or "code"
    async fn link_user(&self, platform: &str, chat_user: &str, chat_name: &str, uid: i32, via: &str) -> Result<()>;
    async fn unlink_user(&self, platform: &str, chat_user: &str) -> Result<bool>;
    /// one-time code from the web profile → user id (the code is used up)
    async fn redeem_code(&self, code: &str) -> Result<Option<i32>>;
    /// channels linked to a call group: (platform, channel)
    async fn group_channels(&self, gid: i32) -> Result<Vec<(String, String)>>;
    async fn channel_groups(&self, platform: &str, channel: &str) -> Result<Vec<i32>>;
    async fn link_group(&self, platform: &str, channel: &str, gid: i32, by: i32) -> Result<bool>;
    async fn unlink_group(&self, platform: &str, channel: &str, gid: i32) -> Result<bool>;
}
