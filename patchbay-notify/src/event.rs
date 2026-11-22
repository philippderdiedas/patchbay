// SPDX-License-Identifier: GPL-3.0-or-later
//! Business-level PBX events (produced by patchbay-web's call tracking, consumed by the bot and other
//! outputs such as webhooks).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    User,
    Group,
    Ivr,
}

/// Whom a call was meant for (a person, a call group or a voice menu).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub kind: TargetKind,
    /// 4-digit internal number (for IVRs without a number: the IVR id)
    pub number: String,
    /// display name / group name, filled in by patchbay-web
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Target {
    pub fn new(kind: TargetKind, number: impl Into<String>) -> Self {
        Target { kind, number: number.into(), name: None }
    }
    /// "5110 Sales EN" or "5110"
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) if !n.is_empty() => format!("{} {}", self.number, n),
            _ => self.number.clone(),
        }
    }
    /// Key for maps: "group:5110"
    pub fn key(&self) -> String {
        format!("{:?}:{}", self.kind, self.number).to_lowercase()
    }
}

/// A calling or called party.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    /// E.164 (external) or internal number; empty when withheld
    pub number: String,
    /// caller ID name (from the dialplan, e.g. the contact name)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Party {
    pub fn label(&self, unknown: &str) -> String {
        let num = Some(self.number.as_str()).filter(|n| !n.is_empty() && *n != "anonymous");
        match (self.name.as_deref().filter(|n| !n.is_empty() && Some(*n) != num), num) {
            (Some(n), Some(c)) => format!("{n} ({c})"),
            (None, Some(c)) => c.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => unknown.to_string(),
        }
    }
    /// Number that can be called back (empty/anonymous: none)
    pub fn callback(&self) -> Option<&str> {
        Some(self.number.as_str()).filter(|n| !n.is_empty() && *n != "anonymous" && n.chars().all(|c| c.is_ascii_digit() || c == '+'))
    }
}

/// Who answered: the device (SIP endpoint) and, filled in by patchbay-web, its owner.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answerer {
    /// SIP endpoint (device user name or trunk)
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_ext: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
}

impl Answerer {
    pub fn label(&self) -> String {
        match (&self.user_name, &self.user_ext) {
            (Some(n), Some(e)) => format!("{n} ({e})"),
            (None, Some(e)) => e.clone(),
            (Some(n), None) => n.clone(),
            (None, None) => self.device.clone().unwrap_or_else(|| self.endpoint.clone()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissReason {
    /// rang, nobody answered, the call moved on to the no-answer target
    NoAnswer,
    /// went to the mailbox
    Voicemail,
    /// the caller hung up while it was ringing (or during the announcement)
    Hangup,
    /// immediate forward of a person: never rang
    Forwarded,
    /// do not disturb: never rang
    Dnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Available,
    Ringing,
    Busy,
    Dnd,
    Offline,
}

impl Presence {
    /// DND shows a free line as "do not disturb"; ringing and busy lines stay as they are.
    pub fn with_dnd(self, dnd: bool) -> Presence {
        if dnd && self == Presence::Available { Presence::Dnd } else { self }
    }

    /// Wire name, same as the serde name (web phone book JSON, CSS class presence-<name>).
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Available => "available",
            Presence::Ringing => "ringing",
            Presence::Busy => "busy",
            Presence::Dnd => "dnd",
            Presence::Offline => "offline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum PbxEvent {
    /// A call reached a target (person, group, IVR). One call can reach several targets in turn
    /// (group → no answer → person); each gets its own event.
    #[serde(rename = "call.incoming")]
    CallIncoming { call_id: String, caller: Party, did: Option<String>, target: Target },
    #[serde(rename = "call.answered")]
    CallAnswered { call_id: String, caller: Party, target: Option<Target>, by: Answerer },
    /// A person or group target was not answered.
    #[serde(rename = "call.missed")]
    CallMissed { call_id: String, caller: Party, did: Option<String>, target: Target, reason: MissReason, rang: Vec<String> },
    /// End of a tracked call (only calls that reached a target).
    #[serde(rename = "call.ended")]
    CallEnded { call_id: String, caller: Party, answered: bool, duration_s: u64, talk_s: Option<u64> },
    #[serde(rename = "voicemail.new")]
    VoicemailNew { id: i32, call_id: Option<String>, owner: Target, caller: Party, duration_s: i32 },
    #[serde(rename = "presence.changed")]
    PresenceChanged { ext: String, state: Presence },
}

impl PbxEvent {
    pub fn name(&self) -> &'static str {
        match self {
            PbxEvent::CallIncoming { .. } => "call.incoming",
            PbxEvent::CallAnswered { .. } => "call.answered",
            PbxEvent::CallMissed { .. } => "call.missed",
            PbxEvent::CallEnded { .. } => "call.ended",
            PbxEvent::VoicemailNew { .. } => "voicemail.new",
            PbxEvent::PresenceChanged { .. } => "presence.changed",
        }
    }
    pub fn call_id(&self) -> Option<&str> {
        match self {
            PbxEvent::CallIncoming { call_id, .. }
            | PbxEvent::CallAnswered { call_id, .. }
            | PbxEvent::CallMissed { call_id, .. }
            | PbxEvent::CallEnded { call_id, .. } => Some(call_id),
            PbxEvent::VoicemailNew { call_id, .. } => call_id.as_deref(),
            PbxEvent::PresenceChanged { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn party_labels() {
        assert_eq!(Party { number: "+4940123".into(), name: Some("Max".into()) }.label("?"), "Max (+4940123)");
        assert_eq!(Party { number: "+4940123".into(), name: Some("+4940123".into()) }.label("?"), "+4940123");
        assert_eq!(Party { number: "anonymous".into(), name: None }.label("?"), "?");
        assert_eq!(Party { number: "anonymous".into(), name: None }.callback(), None);
        assert_eq!(Party { number: "1010".into(), name: None }.callback(), Some("1010"));
    }

    #[test]
    fn event_json() {
        let e = PbxEvent::CallIncoming {
            call_id: "1.2".into(),
            caller: Party { number: "+491".into(), name: None },
            did: None,
            target: Target::new(TargetKind::Group, "5110"),
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["event"], "call.incoming");
        assert_eq!(j["target"]["kind"], "group");
        assert_eq!(serde_json::from_value::<PbxEvent>(j).unwrap(), e);
    }

    #[test]
    fn presence_names() {
        for p in [Presence::Available, Presence::Ringing, Presence::Busy, Presence::Dnd, Presence::Offline] {
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
        assert_eq!(Presence::Available.with_dnd(true), Presence::Dnd);
        assert_eq!(Presence::Busy.with_dnd(true), Presence::Busy);
        assert_eq!(Presence::Available.with_dnd(false), Presence::Available);
    }
}
