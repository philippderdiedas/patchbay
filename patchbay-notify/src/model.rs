// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform-neutral message model. Adapters render it (Mattermost: message + attachments with
//! buttons; Telegram: text + inline keyboard; Matrix: text, actions as commands).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tone {
    #[default]
    Neutral,
    Info,
    Good,
    Warn,
    Bad,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
}

/// What a button does. Serialised into the button's callback data (signed by the adapter where
/// the platform passes it through a client).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "a", rename_all = "snake_case")]
pub enum Action {
    /// call this number with the clicking user's own permissions (their devices ring first)
    Call { n: String },
    VmPlay { id: i32 },
    /// `l`: the button belongs to the mailbox list (the list is redrawn)
    VmHeard {
        id: i32,
        #[serde(default)]
        l: bool,
    },
    VmDelete {
        id: i32,
        #[serde(default)]
        l: bool,
    },
    Dnd { on: bool },
    ForwardOff,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub label: String,
    pub action: Action,
    #[serde(default)]
    pub style: ButtonStyle,
}

impl Button {
    pub fn new(label: impl Into<String>, action: Action) -> Self {
        Button { label: label.into(), action, style: ButtonStyle::Default }
    }
    pub fn primary(mut self) -> Self {
        self.style = ButtonStyle::Primary;
        self
    }
    pub fn danger(mut self) -> Self {
        self.style = ButtonStyle::Danger;
        self
    }
}

/// One block of a message (Mattermost: one attachment).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub title: Option<String>,
    /// Markdown subset: **bold**, `code`, line breaks
    pub text: String,
    pub fields: Vec<(String, String)>,
    pub buttons: Vec<Button>,
    pub tone: Tone,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutMessage {
    /// main text (Markdown subset); also used for push notifications
    pub text: String,
    pub sections: Vec<Section>,
}

impl OutMessage {
    pub fn text(t: impl Into<String>) -> Self {
        OutMessage { text: t.into(), sections: vec![] }
    }
    pub fn section(mut self, s: Section) -> Self {
        self.sections.push(s);
        self
    }
    /// All buttons of the message (for platforms that render them in one keyboard)
    pub fn buttons(&self) -> impl Iterator<Item = &Button> {
        self.sections.iter().flat_map(|s| s.buttons.iter())
    }
    /// Plain text rendering of everything (fallback for platforms without attachments)
    pub fn plain(&self) -> String {
        let mut out = self.text.clone();
        for s in &self.sections {
            if !out.is_empty() {
                out.push('\n');
            }
            if let Some(t) = &s.title {
                out += &format!("**{t}**\n");
            }
            out += &s.text;
            for (k, v) in &s.fields {
                out += &format!("\n{k}: {v}");
            }
        }
        out
    }
}

/// Where to post: a direct message to a chat user or a channel/room.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dest {
    User(String),
    Channel(String),
}

/// A posted message, for later edits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageRef {
    pub channel: String,
    pub id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioFormat {
    Mp3,
    /// OGG/Opus (Telegram voice messages)
    OggOpus,
}

#[derive(Clone, Debug)]
pub struct Audio {
    pub data: Vec<u8>,
    pub filename: String,
    pub mime: String,
    pub duration_s: i32,
}

/// Answer to an inbound command or button click.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    /// shown only to the user who sent the command/clicked (where the platform can)
    pub message: Option<OutMessage>,
    /// replaces the message the button belonged to
    pub update: Option<OutMessage>,
}

impl Reply {
    pub fn text(t: impl Into<String>) -> Self {
        Reply { message: Some(OutMessage::text(t)), update: None }
    }
    pub fn msg(m: OutMessage) -> Self {
        Reply { message: Some(m), update: None }
    }
    pub fn none() -> Self {
        Reply::default()
    }
}
