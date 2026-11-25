// SPDX-License-Identifier: GPL-3.0-or-later
//! Bot core: message lifecycle for calls and voicemails, routing (group → linked channel,
//! person → direct message), linking chat users, commands and button actions.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;

use crate::{
    commands::{Command, parse},
    event::{MissReason, Party, PbxEvent, Presence, Target, TargetKind},
    i18n::Lang,
    model::{Action, AudioFormat, Button, Dest, MessageRef, OutMessage, Reply, Section, Tone},
    pbx::{Pbx, PbxUser, Store, VmOp, VoicemailInfo},
    platform::{ChatPlatform, Inbound, InboundHandler, InboundKind},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMode {
    /// match the chat account's e-mail address with the PBX user (where the platform tells it);
    /// one-time codes work as well
    Auto,
    /// only one-time codes from the web profile
    Code,
}

impl LinkMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(LinkMode::Auto),
            "link" | "code" => Some(LinkMode::Code),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BotConfig {
    pub link_mode: LinkMode,
    pub lang: Lang,
    /// web interface, for "link your account" hints
    pub public_url: String,
}

/// State of one call as shown in a channel/DM post.
#[derive(Clone, Debug, PartialEq)]
struct Card {
    target: Target,
    caller: Party,
    did: Option<String>,
    answered_by: Option<String>,
    missed: Option<MissReason>,
    talk_s: Option<u64>,
    voicemail: bool,
}

struct CardPost {
    platform: &'static str,
    msg: MessageRef,
    card: Card,
}

struct CallCards {
    since: Instant,
    posts: Vec<CardPost>,
}

pub struct Bot {
    cfg: BotConfig,
    pbx: Arc<dyn Pbx>,
    store: Arc<dyn Store>,
    platforms: Vec<Arc<dyn ChatPlatform>>,
    cards: Mutex<HashMap<String, CallCards>>,
}

const MAX_LIST: usize = 10;

fn fmt_dur(s: u64) -> String {
    format!("{}:{:02}", s / 60, s % 60)
}

impl Bot {
    pub fn new(cfg: BotConfig, pbx: Arc<dyn Pbx>, store: Arc<dyn Store>, platforms: Vec<Arc<dyn ChatPlatform>>) -> Arc<Self> {
        Arc::new(Bot { cfg, pbx, store, platforms, cards: Mutex::new(HashMap::new()) })
    }

    pub fn platforms(&self) -> &[Arc<dyn ChatPlatform>] {
        &self.platforms
    }

    fn platform(&self, id: &str) -> Option<&Arc<dyn ChatPlatform>> {
        self.platforms.iter().find(|p| p.id() == id)
    }

    fn t(&self, en: &'static str) -> &'static str {
        self.cfg.lang.t(en)
    }

    /// Refusal reasons from the PBX side (English) in the bot language.
    fn refusal(&self, why: &str) -> String {
        const KNOWN: [&str; 13] = [
            "you may not call this number",
            "database error",
            "forwarding to yourself",
            "invalid number",
            "none of your phones is registered",
            "not a forwarding target",
            "not a valid phone number",
            "not registered",
            "ring time must be 10–120 seconds",
            "set by the admins at this site",
            "unknown device",
            "unknown internal number",
            "not supported",
        ];
        match KNOWN.iter().find(|k| **k == why) {
            Some(k) => self.t(k).to_string(),
            None => why.to_string(),
        }
    }

    /// Starts the inbound loops of all platforms (WebSocket, polling).
    pub fn spawn_inbound(self: &Arc<Self>) {
        for p in &self.platforms {
            let p = p.clone();
            let h: Arc<dyn InboundHandler> = self.clone();
            tokio::spawn(async move { p.run(h).await });
        }
    }

    // --- Events -----------------------------------------------------------------------------

    /// Handles one PBX event. Call it from a single task in event order (posts before edits).
    pub async fn on_event(&self, ev: &PbxEvent) {
        if let Err(e) = self.on_event_inner(ev).await {
            tracing::warn!("bot: {} failed: {e:#}", ev.name());
        }
        self.expire_cards();
    }

    async fn on_event_inner(&self, ev: &PbxEvent) -> anyhow::Result<()> {
        match ev {
            PbxEvent::CallIncoming { call_id, caller, did, target } if target.kind == TargetKind::Group => {
                let Some(g) = self.pbx.group_by_number(&target.number).await? else { return Ok(()) };
                let card = Card { target: target.clone(), caller: caller.clone(), did: did.clone(), answered_by: None, missed: None, talk_s: None, voicemail: false };
                let msg = self.render_card(&card);
                for (platform, channel) in self.store.group_channels(g.id).await? {
                    let Some(p) = self.platform(&platform) else { continue };
                    match p.send(&Dest::Channel(channel.clone()), &msg).await {
                        Ok(r) => self.add_card(call_id, CardPost { platform: p.id(), msg: r, card: card.clone() }),
                        Err(e) => tracing::warn!("bot: post to {platform}:{channel} failed: {e:#}"),
                    }
                }
            }
            PbxEvent::CallAnswered { call_id, target: Some(t), by, .. } => {
                let label = by.label();
                self.update_cards(call_id, |c| {
                    if c.target.key() == t.key() && c.answered_by.is_none() {
                        c.answered_by = Some(label.clone());
                        c.missed = None;
                        true
                    } else {
                        false
                    }
                })
                .await;
            }
            PbxEvent::CallMissed { call_id, caller, did, target, reason, .. } => {
                self.update_cards(call_id, |c| {
                    if c.target.key() == target.key() && c.answered_by.is_none() && c.missed.is_none() {
                        c.missed = Some(*reason);
                        c.voicemail |= *reason == MissReason::Voicemail;
                        true
                    } else {
                        false
                    }
                })
                .await;
                if target.kind == TargetKind::User && *reason != MissReason::Forwarded {
                    self.missed_dm(caller, did, target, *reason).await?;
                }
            }
            PbxEvent::CallEnded { call_id, talk_s, .. } => {
                self.update_cards(call_id, |c| {
                    if c.answered_by.is_some() {
                        c.talk_s = *talk_s;
                        true
                    } else if c.missed.is_none() {
                        c.missed = Some(MissReason::Hangup);
                        true
                    } else {
                        false
                    }
                })
                .await;
                self.cards.lock().unwrap().remove(call_id);
            }
            PbxEvent::VoicemailNew { id, call_id, owner, .. } => {
                if let Some(cid) = call_id {
                    self.update_cards(cid, |c| {
                        let hit = c.target.key() == owner.key() && !c.voicemail;
                        if hit {
                            c.voicemail = true;
                        }
                        hit
                    })
                    .await;
                }
                self.deliver_voicemail(*id, owner).await?;
            }
            PbxEvent::VoicemailChanged { id } => self.sync_voicemail(*id).await?,
            _ => {}
        }
        Ok(())
    }

    fn add_card(&self, call_id: &str, post: CardPost) {
        let mut cards = self.cards.lock().unwrap();
        cards.entry(call_id.to_string()).or_insert_with(|| CallCards { since: Instant::now(), posts: vec![] }).posts.push(post);
    }

    fn expire_cards(&self) {
        // calls whose end we never saw (lost AMI connection): forget after 4 hours
        self.cards.lock().unwrap().retain(|_, c| c.since.elapsed() < Duration::from_secs(4 * 3600));
    }

    /// Applies `f` to every card of the call; edits the posts it changed.
    async fn update_cards(&self, call_id: &str, mut f: impl FnMut(&mut Card) -> bool) {
        let changed: Vec<(&'static str, MessageRef, Card)> = {
            let mut cards = self.cards.lock().unwrap();
            let Some(cc) = cards.get_mut(call_id) else { return };
            cc.posts.iter_mut().filter_map(|p| f(&mut p.card).then(|| (p.platform, p.msg.clone(), p.card.clone()))).collect()
        };
        for (platform, msg, card) in changed {
            if let Some(p) = self.platform(platform) {
                if !p.capabilities().edit {
                    continue;
                }
                if let Err(e) = p.edit(&msg, &self.render_card(&card)).await {
                    tracing::warn!("bot: edit on {platform} failed: {e:#}");
                }
            }
        }
    }

    async fn missed_dm(&self, caller: &Party, did: &Option<String>, target: &Target, reason: MissReason) -> anyhow::Result<()> {
        let Some(uid) = self.pbx.user_id_by_ext(&target.number).await? else { return Ok(()) };
        // the voicemail itself follows as a chat message
        if reason == MissReason::Voicemail && self.pbx.vm_delivery(uid).await?.chat() {
            return Ok(());
        }
        let card = Card { target: target.clone(), caller: caller.clone(), did: did.clone(), answered_by: None, missed: Some(reason), talk_s: None, voicemail: false };
        let msg = self.render_personal_missed(&card);
        for p in &self.platforms {
            if let Some(chat_user) = self.store.chat_user_of(p.id(), uid).await? {
                if let Err(e) = p.send(&Dest::User(chat_user), &msg).await {
                    tracing::warn!("bot: missed-call DM on {} failed: {e:#}", p.id());
                }
            }
        }
        Ok(())
    }

    async fn deliver_voicemail(&self, id: i32, owner: &Target) -> anyhow::Result<()> {
        let Some(vm) = self.pbx.voicemail(None, id).await? else { return Ok(()) };
        let msg = self.render_voicemail(&vm, None);
        // (platform, destination)
        let mut dests: Vec<(&Arc<dyn ChatPlatform>, Dest)> = vec![];
        match owner.kind {
            TargetKind::User => {
                if let Some(uid) = self.pbx.user_id_by_ext(&owner.number).await? {
                    if self.pbx.vm_delivery(uid).await?.chat() {
                        for p in &self.platforms {
                            if let Some(cu) = self.store.chat_user_of(p.id(), uid).await? {
                                dests.push((p, Dest::User(cu)));
                            }
                        }
                    }
                }
            }
            TargetKind::Group => {
                let Some(g) = self.pbx.group_by_number(&owner.number).await? else { return Ok(()) };
                let channels = self.store.group_channels(g.id).await?;
                for (platform, channel) in &channels {
                    if let Some(p) = self.platform(platform) {
                        dests.push((p, Dest::Channel(channel.clone())));
                    }
                }
                // members who want voicemails in chat and do not see the group's channel there
                for uid in self.pbx.group_members(g.id).await? {
                    if !self.pbx.vm_delivery(uid).await?.chat() {
                        continue;
                    }
                    for p in &self.platforms {
                        if channels.iter().any(|(pl, _)| pl == p.id()) {
                            continue;
                        }
                        if let Some(cu) = self.store.chat_user_of(p.id(), uid).await? {
                            dests.push((p, Dest::User(cu)));
                        }
                    }
                }
            }
            TargetKind::Ivr => return Ok(()),
        }
        let mut audio_cache: HashMap<AudioFormat, crate::model::Audio> = HashMap::new();
        for (p, dest) in dests {
            let res = match p.capabilities().audio {
                Some(fmt) => {
                    if !audio_cache.contains_key(&fmt) {
                        if let Some(a) = self.pbx.voicemail_audio(None, id, fmt).await? {
                            audio_cache.insert(fmt, a);
                        }
                    }
                    match audio_cache.get(&fmt) {
                        Some(a) => p.send_audio(&dest, &msg, a).await,
                        None => p.send(&dest, &msg).await,
                    }
                }
                None => p.send(&dest, &msg).await,
            };
            match res {
                Ok(r) => self.store.vm_post_add(id, p.id(), &r).await?,
                Err(e) => tracing::warn!("bot: voicemail {id} to {} failed: {e:#}", p.id()),
            }
        }
        Ok(())
    }

    /// Brings every chat post of a voicemail in line with its state: heard / unheard are edited,
    /// a deleted voicemail's posts are deleted (and forgotten).
    async fn sync_voicemail(&self, id: i32) -> anyhow::Result<()> {
        let posts = self.store.vm_posts(id).await?;
        if posts.is_empty() {
            return Ok(());
        }
        let v = self.pbx.voicemail(None, id).await?;
        match v.filter(|v| !v.deleted) {
            None => {
                // a post that cannot be deleted (no permission, archived channel, platform without
                // delete) at least loses its buttons and says so
                let gone = self.pbx.voicemail(None, id).await?.map(|v| self.render_voicemail(&v, Some(self.t("deleted").to_string())));
                for (platform, post) in &posts {
                    let Some(p) = self.platform(platform) else { continue };
                    let res = if p.capabilities().delete { p.delete(post).await } else { Err(anyhow::anyhow!("not supported")) };
                    if let Err(e) = res {
                        tracing::warn!("bot: deleting voicemail {id} post on {platform} failed: {e:#}");
                        if let (Some(m), true) = (&gone, p.capabilities().edit) {
                            if let Err(e) = p.edit(post, m).await {
                                tracing::warn!("bot: marking voicemail {id} post on {platform} deleted failed: {e:#}");
                            }
                        }
                    }
                }
                self.store.vm_posts_forget(id).await?;
            }
            Some(v) => {
                let state = match (&v.heard, &v.heard_by) {
                    (true, Some(by)) => Some(format!("{} {by}", self.t("heard by"))),
                    _ => None,
                };
                let msg = self.render_voicemail(&v, state);
                for (platform, post) in &posts {
                    let Some(p) = self.platform(platform).filter(|p| p.capabilities().edit) else { continue };
                    if let Err(e) = p.edit(post, &msg).await {
                        tracing::warn!("bot: editing voicemail {id} post on {platform} failed: {e:#}");
                    }
                }
            }
        }
        Ok(())
    }

    // --- Rendering ------------------------------------------------------------------------------

    fn caller_label(&self, p: &Party) -> String {
        p.label(self.t("unknown caller"))
    }

    fn render_card(&self, c: &Card) -> OutMessage {
        let caller = self.caller_label(&c.caller);
        let target = c.target.label();
        let (text, tone) = if let Some(by) = &c.answered_by {
            let mut t = format!("✅ {} **{target}** {} **{caller}** – {} **{by}**", self.t("Call for"), self.t("from"), self.t("answered by"));
            if let Some(s) = c.talk_s {
                t += &format!(" ({})", fmt_dur(s));
            }
            (t, Tone::Good)
        } else if let Some(r) = c.missed {
            (format!("❌ {} **{target}** {} **{caller}** – {}", self.t("Missed call for"), self.t("from"), self.reason(r, c.voicemail)), Tone::Bad)
        } else {
            (format!("📞 {} **{target}** {} **{caller}**", self.t("Incoming call for"), self.t("from")), Tone::Info)
        };
        let mut s = Section { tone, ..Default::default() };
        if let Some(d) = &c.did {
            s.fields.push((self.t("Dialled number").into(), d.clone()));
        }
        if c.answered_by.is_none() && c.missed.is_some() {
            if let Some(n) = c.caller.callback() {
                s.buttons.push(Button::new(self.t("Call back"), Action::Call { n: n.into() }).primary());
            }
        }
        OutMessage::text(text).section(s)
    }

    fn reason(&self, r: MissReason, voicemail: bool) -> &'static str {
        if voicemail {
            return self.t("voicemail left");
        }
        match r {
            MissReason::NoAnswer => self.t("no answer"),
            MissReason::Voicemail => self.t("voicemail left"),
            MissReason::Hangup => self.t("caller hung up"),
            MissReason::Forwarded => self.t("forwarded"),
            MissReason::Dnd => self.t("do not disturb"),
        }
    }

    fn render_personal_missed(&self, c: &Card) -> OutMessage {
        let caller = self.caller_label(&c.caller);
        let mut s = Section { tone: Tone::Bad, ..Default::default() };
        if let Some(d) = &c.did {
            s.fields.push((self.t("Dialled number").into(), d.clone()));
        }
        if let Some(n) = c.caller.callback() {
            s.buttons.push(Button::new(self.t("Call back"), Action::Call { n: n.into() }).primary());
        }
        let r = c.missed.map(|r| self.reason(r, false)).unwrap_or("");
        OutMessage::text(format!("❌ {} **{caller}** – {r}", self.t("Missed call from"))).section(s)
    }

    fn vm_caller(&self, v: &VoicemailInfo) -> String {
        Party { number: v.caller_number.clone().unwrap_or_default(), name: v.caller_name.clone() }.label(self.t("unknown caller"))
    }

    /// One voicemail as its own message (with the audio), `state`: who heard/deleted it.
    fn render_voicemail(&self, v: &VoicemailInfo, state: Option<String>) -> OutMessage {
        let mut text = format!("🎙 {} **{}** {} **{}** ({} s, {})", self.t("Voicemail for"), v.mailbox, self.t("from"), self.vm_caller(v), v.duration_s, v.received);
        if let Some(s) = state {
            text += &format!(" – {s}");
        }
        let tone = if v.deleted || v.heard { Tone::Neutral } else { Tone::Warn };
        OutMessage::text(text).section(Section { tone, buttons: self.vm_buttons(v, false, false), ..Default::default() })
    }

    fn vm_buttons(&self, v: &VoicemailInfo, play: bool, list: bool) -> Vec<Button> {
        let mut b = vec![];
        if v.deleted {
            return b;
        }
        if play {
            b.push(Button::new(self.t("Play"), Action::VmPlay { id: v.id }));
        }
        if !v.heard {
            b.push(Button::new(self.t("Heard"), Action::VmHeard { id: v.id, l: list }));
        }
        if let Some(n) = v.caller_number.as_deref().filter(|n| !n.is_empty() && *n != "anonymous") {
            b.push(Button::new(self.t("Call back"), Action::Call { n: n.into() }).primary());
        }
        b.push(Button::new(self.t("Delete"), Action::VmDelete { id: v.id, l: list }).danger());
        b
    }

    async fn render_mailbox(&self, uid: i32) -> anyhow::Result<OutMessage> {
        let vms = self.pbx.voicemails(uid, MAX_LIST as i64).await?;
        if vms.is_empty() {
            return Ok(OutMessage::text(self.t("Your mailbox is empty.")));
        }
        let mut m = OutMessage::text(format!("**{}**", self.t("Mailbox")));
        for v in vms {
            let new = if v.heard { "" } else { "🔵 " };
            m.sections.push(Section {
                text: format!("{new}**{}** · {} · {} s · {}", self.vm_caller(&v), v.received, v.duration_s, v.mailbox),
                buttons: self.vm_buttons(&v, true, true),
                tone: if v.heard { Tone::Neutral } else { Tone::Warn },
                ..Default::default()
            });
        }
        Ok(m)
    }

    async fn render_status(&self, u: &PbxUser) -> anyhow::Result<OutMessage> {
        let st = self.pbx.status(u.id).await?;
        let mut s = Section::default();
        s.fields.push((self.t("Do not disturb").into(), self.t(if st.dnd { "on" } else { "off" }).into()));
        s.fields.push((self.t("Forward").into(), st.forward.clone().unwrap_or_else(|| self.t("off").into())));
        let then = match st.noanswer_mode.as_str() {
            "mailbox" => self.t("mailbox").to_string(),
            "number" => st.noanswer_value.clone().unwrap_or_default(),
            "nothing" => self.t("nothing").to_string(),
            _ => st.noanswer_value.clone().unwrap_or_default(),
        };
        s.fields.push((self.t("If not answered").into(), format!("{} {} s: {then}", self.t("after"), st.ring_s)));
        s.tone = if st.dnd || st.forward.is_some() { Tone::Warn } else { Tone::Good };
        s.buttons.push(if st.dnd {
            Button::new(self.t("DND off"), Action::Dnd { on: false }).primary()
        } else {
            Button::new(self.t("DND on"), Action::Dnd { on: true })
        });
        if st.forward.is_some() {
            s.buttons.push(Button::new(self.t("Forward off"), Action::ForwardOff).primary());
        }
        Ok(OutMessage::text(format!("**{} {}**", u.ext, u.name)).section(s))
    }

    fn presence_icon(p: Option<Presence>) -> &'static str {
        match p {
            Some(Presence::Available) => "🟢",
            Some(Presence::Ringing) => "🔔",
            Some(Presence::Busy) => "🔴",
            Some(Presence::Dnd) => "⛔",
            Some(Presence::Offline) => "⚪",
            None => "▫️",
        }
    }

    fn help(&self) -> OutMessage {
        let text = crate::commands::COMMANDS
            .iter()
            .map(|c| format!("`{}` – {}{}", c.syntax, if c.admin { self.t("admins: ") } else { "" }, self.t(c.text)))
            .collect::<Vec<_>>()
            .join("\n");
        OutMessage::text(format!("**{}**\n{text}", self.t("PBX bot commands")))
    }

    fn not_linked(&self) -> Reply {
        let hint = match self.cfg.link_mode {
            LinkMode::Auto => self.t("No phone account with your chat e-mail address was found."),
            LinkMode::Code => self.t("Your chat account is not linked to the phone system yet."),
        };
        Reply::text(format!(
            "{hint} {} {}/me/chat {} `/pbx link <code>`.",
            self.t("Create a code at"),
            self.cfg.public_url.trim_end_matches('/'),
            self.t("and send")
        ))
    }

    // --- Inbound ----------------------------------------------------------------------------------

    /// PBX user of a chat user: linked before, or (auto mode) found by e-mail and linked now.
    async fn resolve(&self, inb: &Inbound) -> anyhow::Result<Option<PbxUser>> {
        if let Some(uid) = self.store.linked_user(inb.platform, &inb.chat_user).await? {
            return self.pbx.user(uid).await;
        }
        if self.cfg.link_mode != LinkMode::Auto {
            return Ok(None);
        }
        let Some(p) = self.platform(inb.platform) else { return Ok(None) };
        if !p.capabilities().identity_email {
            return Ok(None);
        }
        // a failing lookup only means "not linked" (the user gets the hint how to link)
        let email = match p.user_email(&inb.chat_user).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("bot: e-mail of {} user {} not available: {e:#}", inb.platform, inb.chat_user);
                None
            }
        };
        let Some(email) = email.filter(|e| e.contains('@')) else { return Ok(None) };
        let Some(u) = self.pbx.user_by_email(&email).await? else { return Ok(None) };
        self.store.link_user(inb.platform, &inb.chat_user, &inb.chat_user_name, u.id, "auto").await?;
        tracing::info!("bot: {} user {} linked to {} by e-mail", inb.platform, inb.chat_user, u.ext);
        Ok(Some(u))
    }

    async fn handle_inner(&self, inb: Inbound) -> anyhow::Result<Reply> {
        let user = self.resolve(&inb).await?;
        match &inb.kind {
            InboundKind::Command(text) => self.command(&inb, user, parse(text)).await,
            InboundKind::Action { action, .. } => match user {
                Some(u) => {
                    tracing::info!("bot: action {action:?} by {} ({})", u.id, inb.platform);
                    let r = self.action(&inb, &u, action.clone()).await;
                    if let Err(e) = &r {
                        tracing::warn!("bot: action {action:?} failed: {e:#}");
                    }
                    r
                }
                None => Ok(self.not_linked()),
            },
            // no reaction mapping yet (Matrix adapter)
            InboundKind::Reaction { .. } => Ok(Reply::none()),
        }
    }

    async fn command(&self, inb: &Inbound, user: Option<PbxUser>, cmd: Command) -> anyhow::Result<Reply> {
        match cmd {
            Command::Help => return Ok(Reply::msg(self.help())),
            Command::Unknown(t) => {
                return Ok(Reply::text(format!("{} `{t}`. {} `/pbx help`.", self.t("Unknown command"), self.t("See"))));
            }
            Command::Link(code) => {
                let code = code.trim().to_uppercase();
                if code.is_empty() {
                    return Ok(self.not_linked());
                }
                return Ok(match self.store.redeem_code(&code).await? {
                    Some(uid) => {
                        self.store.link_user(inb.platform, &inb.chat_user, &inb.chat_user_name, uid, "code").await?;
                        let u = self.pbx.user(uid).await?;
                        Reply::text(format!("{} {}.", self.t("Linked with"), u.map(|u| format!("{} {}", u.ext, u.name)).unwrap_or_default()))
                    }
                    None => Reply::text(self.t("This code is invalid or has expired. Create a new one in the web interface.")),
                });
            }
            _ => {}
        }
        let Some(u) = user else { return Ok(self.not_linked()) };
        Ok(match cmd {
            Command::Unlink => {
                self.store.unlink_user(inb.platform, &inb.chat_user).await?;
                Reply::text(self.t("Your chat account is no longer linked to the phone system."))
            }
            Command::Status => Reply::msg(self.render_status(&u).await?),
            Command::Dnd(on) => {
                let on = match on {
                    Some(b) => b,
                    None => !self.pbx.status(u.id).await?.dnd,
                };
                self.pbx.set_dnd(u.id, on).await?;
                Reply::msg(self.render_status(&u).await?)
            }
            Command::Forward(t) => match self.pbx.set_forward(u.id, t.as_deref()).await? {
                Ok(()) => Reply::msg(self.render_status(&u).await?),
                Err(why) => Reply::text(format!("{}: {}", self.t("Forwarding not set"), self.refusal(&why))),
            },
            Command::NoAnswer(None) | Command::RingTime(None) => Reply::msg(self.render_status(&u).await?),
            Command::NoAnswer(Some(c)) => match self.pbx.set_noanswer(u.id, Some(c), None).await? {
                Ok(()) => Reply::msg(self.render_status(&u).await?),
                Err(why) => Reply::text(format!("{}: {}", self.t("Not saved"), self.refusal(&why))),
            },
            Command::RingTime(Some(secs)) => match self.pbx.set_noanswer(u.id, None, Some(secs)).await? {
                Ok(()) => Reply::msg(self.render_status(&u).await?),
                Err(why) => Reply::text(format!("{}: {}", self.t("Not saved"), self.refusal(&why))),
            },
            Command::Call(n) => self.call(&u, &n).await?,
            Command::Mailbox => Reply::msg(self.render_mailbox(u.id).await?),
            Command::Phonebook(q) => {
                if q.trim().is_empty() {
                    return Ok(Reply::text(format!("{}: `/pbx pb <name>`", self.t("Usage"))));
                }
                let hits = self.pbx.directory(u.id, q.trim()).await?;
                if hits.is_empty() {
                    return Ok(Reply::text(self.t("Nothing found.")));
                }
                let mut s = Section::default();
                let lines: Vec<String> = hits
                    .iter()
                    .take(15)
                    .map(|h| {
                        let icon = if h.kind == "colleague" { Self::presence_icon(h.presence) } else { "👤" };
                        format!("{icon} **{}** {}", h.name, h.number)
                    })
                    .collect();
                s.text = lines.join("\n");
                for h in hits.iter().take(MAX_LIST) {
                    if !h.number.is_empty() {
                        s.buttons.push(Button::new(format!("📞 {}", h.name), Action::Call { n: h.number.clone() }));
                    }
                }
                Reply::msg(OutMessage::text(format!("**{}** „{}“", self.t("Phone book"), q.trim())).section(s))
            }
            Command::Presence(q) => {
                let hits = self.pbx.presence(q.trim()).await?;
                if hits.is_empty() {
                    return Ok(Reply::text(self.t("Nothing found.")));
                }
                let lines: Vec<String> = hits.iter().take(40).map(|h| format!("{} {} **{}**", Self::presence_icon(h.presence), h.number, h.name)).collect();
                Reply::msg(OutMessage::text(format!("**{}**\n{}", self.t("Presence"), lines.join("\n"))))
            }
            Command::Calls => {
                let calls = self.pbx.call_list(u.id, MAX_LIST as i64).await?;
                if calls.is_empty() {
                    return Ok(Reply::text(self.t("No calls.")));
                }
                let mut s = Section::default();
                let mut seen = vec![];
                let mut lines = vec![];
                for c in &calls {
                    let dir = match (c.incoming, c.answered) {
                        (true, true) => "↙",
                        (true, false) => "❌",
                        (false, _) => "↗",
                    };
                    let who = if c.name.is_empty() { c.number.clone() } else { format!("{} ({})", c.name, c.number) };
                    let dur = if c.answered { format!(" · {}", fmt_dur(c.duration_s.max(0) as u64)) } else { String::new() };
                    lines.push(format!("{dir} {} **{who}**{dur}", c.at));
                    if !c.number.is_empty() && !seen.contains(&c.number) && seen.len() < 5 {
                        seen.push(c.number.clone());
                        let label = if c.name.is_empty() { c.number.clone() } else { c.name.clone() };
                        s.buttons.push(Button::new(format!("📞 {label}"), Action::Call { n: c.number.clone() }));
                    }
                }
                s.text = lines.join("\n");
                Reply::msg(OutMessage::text(format!("**{}**", self.t("Recent calls"))).section(s))
            }
            Command::LinkGroup(n) | Command::UnlinkGroup(n) if !u.admin => {
                let _ = n;
                Reply::text(self.t("Only phone system admins can link call groups."))
            }
            Command::LinkGroup(n) => {
                if inb.direct {
                    return Ok(Reply::text(self.t("Run this command in the channel the call group should be linked to.")));
                }
                let Some(g) = self.pbx.group_by_number(n.trim()).await? else { return Ok(Reply::text(self.t("Unknown call group."))) };
                self.store.link_group(inb.platform, &inb.channel, g.id, u.id).await?;
                tracing::info!("bot: group {} linked to {}:{} by {}", g.number, inb.platform, inb.channel, u.ext);
                Reply::text(format!(
                    "{} **{} {}** {}",
                    self.t("Calls for"),
                    g.number,
                    g.name,
                    self.t("now appear in this channel. Make sure the bot is a member of the channel (/invite).")
                ))
            }
            Command::UnlinkGroup(n) => {
                let Some(g) = self.pbx.group_by_number(n.trim()).await? else { return Ok(Reply::text(self.t("Unknown call group."))) };
                let done = self.store.unlink_group(inb.platform, &inb.channel, g.id).await?;
                Reply::text(if done { self.t("Call group unlinked from this channel.") } else { self.t("This call group is not linked to this channel.") })
            }
            Command::Groups => {
                let ids = self.store.channel_groups(inb.platform, &inb.channel).await?;
                if ids.is_empty() {
                    return Ok(Reply::text(self.t("No call groups are linked to this channel.")));
                }
                let mut names = vec![];
                for id in ids {
                    names.push(self.pbx.group_label(id).await?.unwrap_or_else(|| id.to_string()));
                }
                Reply::text(format!("{}: {}", self.t("Linked call groups"), names.join(", ")))
            }
            Command::Help | Command::Unknown(_) | Command::Link(_) => unreachable!(),
        })
    }

    async fn call(&self, u: &PbxUser, n: &str) -> anyhow::Result<Reply> {
        Ok(match self.pbx.originate(u.id, n).await? {
            Ok(()) => Reply::text(format!("📞 {} {n} – {}", self.t("Calling"), self.t("your phone rings first."))),
            Err(why) => Reply::text(format!("{}: {}", self.t("Call not possible"), self.refusal(&why))),
        })
    }

    async fn action(&self, inb: &Inbound, u: &PbxUser, action: Action) -> anyhow::Result<Reply> {
        Ok(match action {
            Action::Call { n } => self.call(u, &n).await?,
            Action::Dnd { on } => {
                self.pbx.set_dnd(u.id, on).await?;
                Reply { message: None, update: Some(self.render_status(u).await?) }
            }
            Action::ForwardOff => {
                if let Err(why) = self.pbx.set_forward(u.id, None).await? {
                    return Ok(Reply::text(why));
                }
                Reply { message: None, update: Some(self.render_status(u).await?) }
            }
            Action::VmPlay { id } => {
                let Some(p) = self.platform(inb.platform) else { return Ok(Reply::none()) };
                let Some(v) = self.pbx.voicemail(Some(u.id), id).await? else { return Ok(Reply::text(self.t("Voicemail not found."))) };
                let msg = self.render_voicemail(&v, None);
                match p.capabilities().audio {
                    Some(fmt) => match self.pbx.voicemail_audio(Some(u.id), id, fmt).await? {
                        Some(a) => {
                            let r = p.send_audio(&Dest::User(inb.chat_user.clone()), &msg, &a).await?;
                            if let Err(e) = self.store.vm_post_add(id, p.id(), &r).await {
                                tracing::warn!("bot: recording voicemail {id} post failed: {e:#}");
                            }
                            // deleted meanwhile (the change event may have run before the post was
                            // recorded): remove this copy as well
                            if self.pbx.voicemail(None, id).await?.is_none_or(|v| v.deleted) {
                                self.sync_voicemail(id).await?;
                            }
                            if inb.direct { Reply::none() } else { Reply::text(self.t("Sent to you as a direct message.")) }
                        }
                        None => Reply::text(self.t("Voicemail not found.")),
                    },
                    None => Reply::text(format!("{}/voicemail", self.cfg.public_url.trim_end_matches('/'))),
                }
            }
            Action::VmHeard { id, l } | Action::VmDelete { id, l } => {
                let op = if matches!(action, Action::VmHeard { .. }) { VmOp::Heard } else { VmOp::Delete };
                // a tracked post of a deleted voicemail is deleted by the change event; editing it
                // here as well would race with that
                let message = match &inb.kind {
                    InboundKind::Action { message, .. } => message.clone(),
                    _ => None,
                };
                let can_delete = self.platform(inb.platform).is_some_and(|p| p.capabilities().delete);
                let tracked = match &message {
                    Some(m) if op == VmOp::Delete && !l && can_delete => {
                        self.store.vm_posts(id).await?.iter().any(|(pl, r)| pl == inb.platform && r.id == m.id)
                    }
                    _ => false,
                };
                if !self.pbx.voicemail_op(u.id, id, op).await? {
                    return Ok(Reply::text(self.t("Voicemail not found.")));
                }
                if tracked {
                    return Ok(Reply::none());
                }
                if l {
                    return Ok(Reply { message: None, update: Some(self.render_mailbox(u.id).await?) });
                }
                let state = match op {
                    VmOp::Heard => format!("{} {}", self.t("heard by"), u.name),
                    VmOp::Delete => format!("{} {}", self.t("deleted by"), u.name),
                };
                match self.pbx.voicemail(None, id).await? {
                    Some(v) => Reply { message: None, update: Some(self.render_voicemail(&v, Some(state))) },
                    None => Reply::none(),
                }
            }
        })
    }
}

#[async_trait]
impl InboundHandler for Bot {
    async fn handle(&self, inb: Inbound) -> Reply {
        let platform = inb.platform;
        match self.handle_inner(inb).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("bot: inbound from {platform} failed: {e:#}");
                Reply::text(self.t("Something went wrong, please try again later."))
            }
        }
    }
}
