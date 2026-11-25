// SPDX-License-Identifier: GPL-3.0-or-later
//! Bot core against fake PBX, store and chat platform.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use async_trait::async_trait;
use patchbay_notify::{
    Bot, BotConfig, InboundHandler, LinkMode, PbxEvent,
    event::{Answerer, MissReason, Party, Target, TargetKind},
    i18n::Lang,
    model::{Action, Audio, AudioFormat, Dest, MessageRef, OutMessage, Tone},
    pbx::*,
    platform::{Capabilities, ChatPlatform, Inbound, InboundKind},
};

// --- Fakes ---------------------------------------------------------------------------------

#[derive(Default)]
struct FakePbx {
    dnd: Mutex<HashMap<i32, bool>>,
    forward: Mutex<HashMap<i32, String>>,
    calls: Mutex<Vec<(i32, String)>>,
    vm_ops: Mutex<Vec<(i32, i32, VmOp)>>,
    delivery: Mutex<HashMap<i32, VmDelivery>>,
}

fn users() -> Vec<PbxUser> {
    vec![
        PbxUser { id: 2, ext: "1020".into(), name: "Admin Person".into(), admin: true },
        PbxUser { id: 4, ext: "1010".into(), name: "Grace".into(), admin: false },
    ]
}

fn emails() -> Vec<(&'static str, i32)> {
    vec![("admin@example.com", 2), ("grace@example.com", 4)]
}

#[async_trait]
impl Pbx for FakePbx {
    async fn user(&self, uid: i32) -> Result<Option<PbxUser>> {
        Ok(users().into_iter().find(|u| u.id == uid))
    }
    async fn user_by_email(&self, email: &str) -> Result<Option<PbxUser>> {
        let id = emails().into_iter().find(|(e, _)| e.eq_ignore_ascii_case(email)).map(|(_, id)| id);
        Ok(users().into_iter().find(|u| Some(u.id) == id))
    }
    async fn user_id_by_ext(&self, ext: &str) -> Result<Option<i32>> {
        Ok(users().into_iter().find(|u| u.ext == ext).map(|u| u.id))
    }
    async fn group_by_number(&self, number: &str) -> Result<Option<PbxGroup>> {
        Ok((number == "5110").then(|| PbxGroup { id: 3, number: "5110".into(), name: "Sales".into() }))
    }
    async fn group_label(&self, gid: i32) -> Result<Option<String>> {
        Ok((gid == 3).then(|| "5110 Sales".into()))
    }
    async fn group_members(&self, gid: i32) -> Result<Vec<i32>> {
        Ok(if gid == 3 { vec![4] } else { vec![] })
    }
    async fn status(&self, uid: i32) -> Result<UserStatus> {
        Ok(UserStatus {
            dnd: *self.dnd.lock().unwrap().get(&uid).unwrap_or(&false),
            forward: self.forward.lock().unwrap().get(&uid).cloned(),
            noanswer_mode: "nothing".into(),
            noanswer_value: None,
            ring_s: 25,
        })
    }
    async fn set_dnd(&self, uid: i32, on: bool) -> Result<bool> {
        self.dnd.lock().unwrap().insert(uid, on);
        Ok(on)
    }
    async fn set_forward(&self, uid: i32, target: Option<&str>) -> Result<std::result::Result<(), String>> {
        match target {
            Some("0900123") => Ok(Err("you may not call this number".into())),
            Some(t) => {
                self.forward.lock().unwrap().insert(uid, t.into());
                Ok(Ok(()))
            }
            None => {
                self.forward.lock().unwrap().remove(&uid);
                Ok(Ok(()))
            }
        }
    }
    async fn originate(&self, uid: i32, to: &str) -> Result<std::result::Result<(), String>> {
        if to.starts_with("0900") {
            return Ok(Err("not allowed".into()));
        }
        self.calls.lock().unwrap().push((uid, to.into()));
        Ok(Ok(()))
    }
    async fn voicemails(&self, _uid: i32, _limit: i64) -> Result<Vec<VoicemailInfo>> {
        Ok(vec![vm(7, false)])
    }
    async fn voicemail(&self, uid: Option<i32>, id: i32) -> Result<Option<VoicemailInfo>> {
        // user 2 has no access to voicemail 7
        let ops = self.vm_ops.lock().unwrap();
        Ok((id == 7 && uid != Some(2)).then(|| VoicemailInfo { deleted: ops.iter().any(|o| o.2 == VmOp::Delete), ..vm(7, ops.iter().any(|o| o.2 == VmOp::Heard)) }))
    }
    async fn voicemail_audio(&self, uid: Option<i32>, id: i32, format: AudioFormat) -> Result<Option<Audio>> {
        assert_eq!(format, AudioFormat::Mp3);
        Ok((id == 7 && uid != Some(2)).then(|| Audio { data: b"ID3".to_vec(), filename: "voicemail-7.mp3".into(), mime: "audio/mpeg".into(), duration_s: 12 }))
    }
    async fn voicemail_op(&self, uid: i32, id: i32, op: VmOp) -> Result<bool> {
        if id != 7 || uid == 2 {
            return Ok(false);
        }
        self.vm_ops.lock().unwrap().push((uid, id, op));
        Ok(true)
    }
    async fn directory(&self, _uid: i32, query: &str) -> Result<Vec<DirectoryEntry>> {
        Ok(if "grace".contains(&query.to_lowercase()) {
            vec![DirectoryEntry { name: "Grace".into(), number: "1010".into(), kind: "colleague", presence: Some(patchbay_notify::event::Presence::Available) }]
        } else {
            vec![]
        })
    }
    async fn presence(&self, _query: &str) -> Result<Vec<DirectoryEntry>> {
        self.directory(0, "").await
    }
    async fn call_list(&self, _uid: i32, _limit: i64) -> Result<Vec<CallListEntry>> {
        Ok(vec![CallListEntry { at: "01.11.26 10:00".into(), incoming: true, answered: false, number: "+4940123".into(), name: String::new(), duration_s: 0 }])
    }
    async fn vm_delivery(&self, uid: i32) -> Result<VmDelivery> {
        Ok(*self.delivery.lock().unwrap().get(&uid).unwrap_or(&VmDelivery::Mail))
    }
}

fn vm(id: i32, heard: bool) -> VoicemailInfo {
    VoicemailInfo {
        id,
        received: "01.11.26 10:00".into(),
        caller_number: Some("+4940123".into()),
        caller_name: Some("Kunde".into()),
        duration_s: 12,
        mailbox: "1010".into(),
        heard,
        heard_by: heard.then(|| "Grace".into()),
        deleted: false,
    }
}

#[derive(Default)]
struct FakeStore {
    links: Mutex<HashMap<(String, String), (i32, String)>>,
    groups: Mutex<Vec<(String, String, i32)>>,
    vm_posts: Mutex<Vec<(i32, String, MessageRef)>>,
}

#[async_trait]
impl Store for FakeStore {
    async fn linked_user(&self, platform: &str, chat_user: &str) -> Result<Option<i32>> {
        Ok(self.links.lock().unwrap().get(&(platform.into(), chat_user.into())).map(|l| l.0))
    }
    async fn chat_user_of(&self, platform: &str, uid: i32) -> Result<Option<String>> {
        Ok(self.links.lock().unwrap().iter().find(|((p, _), (u, _))| p == platform && *u == uid).map(|((_, c), _)| c.clone()))
    }
    async fn link_user(&self, platform: &str, chat_user: &str, _name: &str, uid: i32, via: &str) -> Result<()> {
        self.links.lock().unwrap().insert((platform.into(), chat_user.into()), (uid, via.into()));
        Ok(())
    }
    async fn unlink_user(&self, platform: &str, chat_user: &str) -> Result<bool> {
        Ok(self.links.lock().unwrap().remove(&(platform.into(), chat_user.into())).is_some())
    }
    async fn redeem_code(&self, code: &str) -> Result<Option<i32>> {
        Ok((code == "GOODCODE").then_some(4))
    }
    async fn group_channels(&self, gid: i32) -> Result<Vec<(String, String)>> {
        Ok(self.groups.lock().unwrap().iter().filter(|g| g.2 == gid).map(|g| (g.0.clone(), g.1.clone())).collect())
    }
    async fn channel_groups(&self, platform: &str, channel: &str) -> Result<Vec<i32>> {
        Ok(self.groups.lock().unwrap().iter().filter(|g| g.0 == platform && g.1 == channel).map(|g| g.2).collect())
    }
    async fn link_group(&self, platform: &str, channel: &str, gid: i32, _by: i32) -> Result<bool> {
        self.groups.lock().unwrap().push((platform.into(), channel.into(), gid));
        Ok(true)
    }
    async fn unlink_group(&self, platform: &str, channel: &str, gid: i32) -> Result<bool> {
        let mut g = self.groups.lock().unwrap();
        let n = g.len();
        g.retain(|x| !(x.0 == platform && x.1 == channel && x.2 == gid));
        Ok(g.len() != n)
    }
    async fn vm_post_add(&self, vm: i32, platform: &str, post: &MessageRef) -> Result<()> {
        self.vm_posts.lock().unwrap().push((vm, platform.into(), post.clone()));
        Ok(())
    }
    async fn vm_posts(&self, vm: i32) -> Result<Vec<(String, MessageRef)>> {
        Ok(self.vm_posts.lock().unwrap().iter().filter(|p| p.0 == vm).map(|p| (p.1.clone(), p.2.clone())).collect())
    }
    async fn vm_posts_forget(&self, vm: i32) -> Result<()> {
        self.vm_posts.lock().unwrap().retain(|p| p.0 != vm);
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum Sent {
    Post(Dest, OutMessage),
    Edit(MessageRef, OutMessage),
    Audio(Dest, OutMessage, String),
    Delete(MessageRef),
}

#[derive(Default)]
struct FakeChat {
    sent: Mutex<Vec<Sent>>,
}

impl FakeChat {
    fn take(&self) -> Vec<Sent> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }
}

#[async_trait]
impl ChatPlatform for FakeChat {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { buttons: true, edit: true, delete: true, audio: Some(AudioFormat::Mp3), reactions: false, slash_commands: true, identity_email: true }
    }
    async fn send(&self, to: &Dest, msg: &OutMessage) -> Result<MessageRef> {
        let mut s = self.sent.lock().unwrap();
        s.push(Sent::Post(to.clone(), msg.clone()));
        Ok(MessageRef { channel: "c".into(), id: format!("p{}", s.len()) })
    }
    async fn edit(&self, r: &MessageRef, msg: &OutMessage) -> Result<()> {
        self.sent.lock().unwrap().push(Sent::Edit(r.clone(), msg.clone()));
        Ok(())
    }
    async fn send_audio(&self, to: &Dest, msg: &OutMessage, audio: &Audio) -> Result<MessageRef> {
        let mut s = self.sent.lock().unwrap();
        s.push(Sent::Audio(to.clone(), msg.clone(), audio.filename.clone()));
        Ok(MessageRef { channel: "c".into(), id: format!("p{}", s.len()) })
    }
    async fn delete(&self, r: &MessageRef) -> Result<()> {
        self.sent.lock().unwrap().push(Sent::Delete(r.clone()));
        Ok(())
    }
    async fn user_email(&self, chat_user: &str) -> Result<Option<String>> {
        Ok(match chat_user {
            "mm-admin" => Some("Admin@Example.com".into()),
            "mm-grace" => Some("grace@example.com".into()),
            _ => Some("stranger@example.com".into()),
        })
    }
    async fn run(self: Arc<Self>, _handler: Arc<dyn InboundHandler>) {}
}

struct Setup {
    bot: Arc<Bot>,
    pbx: Arc<FakePbx>,
    store: Arc<FakeStore>,
    chat: Arc<FakeChat>,
}

fn setup(mode: LinkMode) -> Setup {
    let pbx = Arc::new(FakePbx::default());
    let store = Arc::new(FakeStore::default());
    let chat = Arc::new(FakeChat::default());
    let bot = Bot::new(
        BotConfig { link_mode: mode, lang: Lang::En, public_url: "https://pbx.example.com".into() },
        pbx.clone(),
        store.clone(),
        vec![chat.clone()],
    );
    Setup { bot, pbx, store, chat }
}

fn cmd(user: &str, channel: &str, direct: bool, text: &str) -> Inbound {
    Inbound { platform: "fake", chat_user: user.into(), chat_user_name: user.into(), channel: channel.into(), direct, kind: InboundKind::Command(text.into()) }
}

fn click(user: &str, action: Action) -> Inbound {
    Inbound {
        platform: "fake",
        chat_user: user.into(),
        chat_user_name: user.into(),
        channel: "dm".into(),
        direct: true,
        kind: InboundKind::Action { action, message: Some(MessageRef { channel: "dm".into(), id: "p9".into() }) },
    }
}

fn reply_text(r: &patchbay_notify::model::Reply) -> String {
    r.message.as_ref().or(r.update.as_ref()).map(|m| m.plain()).unwrap_or_default()
}

fn caller() -> Party {
    Party { number: "+4940123".into(), name: Some("Kunde GmbH".into()) }
}

fn group() -> Target {
    Target { kind: TargetKind::Group, number: "5110".into(), name: Some("Sales".into()) }
}

fn person() -> Target {
    Target { kind: TargetKind::User, number: "1010".into(), name: Some("Grace".into()) }
}

// --- Tests -----------------------------------------------------------------------------------

#[tokio::test]
async fn auto_link_by_email_and_status() {
    let s = setup(LinkMode::Auto);
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "status")).await;
    assert!(reply_text(&r).contains("1010 Grace"), "{r:?}");
    assert_eq!(s.store.links.lock().unwrap().get(&("fake".into(), "mm-grace".into())), Some(&(4, "auto".into())));
    // unknown e-mail: not linked, hint to the web interface
    let r = s.bot.handle(cmd("mm-x", "dm", true, "status")).await;
    assert!(reply_text(&r).contains("https://pbx.example.com/me/chat"), "{r:?}");
}

#[tokio::test]
async fn code_mode_never_links_by_email() {
    let s = setup(LinkMode::Code);
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "dnd on")).await;
    assert!(reply_text(&r).contains("not linked"), "{r:?}");
    assert!(s.pbx.dnd.lock().unwrap().is_empty());
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "link wrong")).await;
    assert!(reply_text(&r).contains("invalid"), "{r:?}");
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "link goodcode")).await;
    assert!(reply_text(&r).contains("Linked with 1010 Grace"), "{r:?}");
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "dnd on")).await;
    assert_eq!(s.pbx.dnd.lock().unwrap().get(&4), Some(&true));
    assert!(reply_text(&r).contains("Do not disturb: on"), "{r:?}");
    s.bot.handle(cmd("mm-grace", "dm", true, "unlink")).await;
    assert!(s.store.links.lock().unwrap().is_empty());
}

#[tokio::test]
async fn commands_use_the_users_own_rights() {
    let s = setup(LinkMode::Auto);
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "call 040 123")).await;
    assert!(reply_text(&r).contains("Calling 040123"), "{r:?}");
    assert_eq!(*s.pbx.calls.lock().unwrap(), vec![(4, "040123".to_string())]);
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "call 0900 1")).await;
    assert!(reply_text(&r).contains("Call not possible: not allowed"), "{r:?}");
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "forward 0900123")).await;
    assert!(reply_text(&r).contains("Forwarding not set"), "{r:?}");
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "forward 1020")).await;
    assert!(reply_text(&r).contains("Forward: 1020"), "{r:?}");
    // status shows a "forward off" button that works
    let b = r.message.unwrap().buttons().map(|b| b.action.clone()).collect::<Vec<_>>();
    assert!(b.contains(&Action::ForwardOff));
    let r = s.bot.handle(click("mm-grace", Action::ForwardOff)).await;
    assert!(r.update.unwrap().plain().contains("Forward: off"));
    // click to call back uses the clicker's account
    s.bot.handle(click("mm-admin", Action::Call { n: "+4940123".into() })).await;
    assert_eq!(s.pbx.calls.lock().unwrap().last(), Some(&(2, "+4940123".to_string())));
}

#[tokio::test]
async fn lists() {
    let s = setup(LinkMode::Auto);
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "pb gra")).await;
    let m = r.message.unwrap();
    assert!(m.plain().contains("Grace"));
    assert_eq!(m.buttons().next().unwrap().action, Action::Call { n: "1010".into() });
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "calls")).await;
    assert!(reply_text(&r).contains("+4940123"));
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "who")).await;
    assert!(reply_text(&r).contains("1010"));
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "mailbox")).await;
    let m = r.message.unwrap();
    assert!(m.buttons().any(|b| b.action == Action::VmPlay { id: 7 }));
    assert!(m.buttons().any(|b| b.action == Action::VmHeard { id: 7, l: true }));
    let r = s.bot.handle(cmd("mm-grace", "dm", true, "nonsense")).await;
    assert!(reply_text(&r).contains("Unknown command"));
}

#[tokio::test]
async fn group_linking_is_admin_only_and_in_channels() {
    let s = setup(LinkMode::Auto);
    let r = s.bot.handle(cmd("mm-grace", "town", false, "link-group 5110")).await;
    assert!(reply_text(&r).contains("Only phone system admins"));
    let r = s.bot.handle(cmd("mm-admin", "dm", true, "link-group 5110")).await;
    assert!(reply_text(&r).contains("in the channel"));
    let r = s.bot.handle(cmd("mm-admin", "sales", false, "link-group 9999")).await;
    assert!(reply_text(&r).contains("Unknown call group"));
    let r = s.bot.handle(cmd("mm-admin", "sales", false, "link-group 5110")).await;
    assert!(reply_text(&r).contains("5110 Sales"), "{r:?}");
    assert_eq!(*s.store.groups.lock().unwrap(), vec![("fake".to_string(), "sales".to_string(), 3)]);
    let r = s.bot.handle(cmd("mm-admin", "sales", false, "groups")).await;
    assert!(reply_text(&r).contains("5110 Sales"));
    let r = s.bot.handle(cmd("mm-admin", "sales", false, "unlink-group 5110")).await;
    assert!(reply_text(&r).contains("unlinked"));
    assert!(s.store.groups.lock().unwrap().is_empty());
}

#[tokio::test]
async fn group_call_lifecycle_post_then_edits() {
    let s = setup(LinkMode::Auto);
    s.store.groups.lock().unwrap().push(("fake".into(), "sales".into(), 3));
    let c = "1700000000.1".to_string();
    s.bot.on_event(&PbxEvent::CallIncoming { call_id: c.clone(), caller: caller(), did: Some("+494012345053".into()), target: group() }).await;
    s.bot.on_event(&PbxEvent::CallAnswered {
        call_id: c.clone(),
        caller: caller(),
        target: Some(group()),
        by: Answerer { endpoint: "abc".into(), device: Some("Desk".into()), user_ext: Some("1010".into()), user_name: Some("Grace".into()) },
    })
    .await;
    s.bot.on_event(&PbxEvent::CallEnded { call_id: c.clone(), caller: caller(), answered: true, duration_s: 80, talk_s: Some(72) }).await;
    let sent = s.chat.take();
    assert_eq!(sent.len(), 3, "{sent:?}");
    let Sent::Post(Dest::Channel(ch), m) = &sent[0] else { panic!("{sent:?}") };
    assert_eq!(ch, "sales");
    assert!(m.text.contains("Incoming call for **5110 Sales** from **Kunde GmbH (+4940123)**"), "{}", m.text);
    assert_eq!(m.sections[0].tone, Tone::Info);
    let Sent::Edit(r, m) = &sent[1] else { panic!() };
    assert_eq!(r.id, "p1");
    assert!(m.text.contains("answered by **Grace (1010)**"), "{}", m.text);
    let Sent::Edit(_, m) = &sent[2] else { panic!() };
    assert!(m.text.contains("(1:12)"), "{}", m.text);
    // events after the end change nothing
    s.bot.on_event(&PbxEvent::CallMissed { call_id: c, caller: caller(), did: None, target: group(), reason: MissReason::Hangup, rang: vec![] }).await;
    assert!(s.chat.take().is_empty());
}

#[tokio::test]
async fn group_missed_with_callback_button_and_voicemail() {
    let s = setup(LinkMode::Auto);
    s.store.groups.lock().unwrap().push(("fake".into(), "sales".into(), 3));
    let c = "1700000000.2".to_string();
    s.bot.on_event(&PbxEvent::CallIncoming { call_id: c.clone(), caller: caller(), did: None, target: group() }).await;
    s.bot.on_event(&PbxEvent::CallMissed { call_id: c.clone(), caller: caller(), did: None, target: group(), reason: MissReason::NoAnswer, rang: vec!["1010".into()] }).await;
    s.bot.on_event(&PbxEvent::VoicemailNew { id: 7, call_id: Some(c.clone()), owner: group(), caller: caller(), duration_s: 12 }).await;
    let sent = s.chat.take();
    let Sent::Edit(_, m) = &sent[1] else { panic!("{sent:?}") };
    assert!(m.text.contains("Missed call") && m.text.contains("no answer"), "{}", m.text);
    assert_eq!(m.buttons().next().unwrap().action, Action::Call { n: "+4940123".into() });
    let Sent::Edit(_, m) = &sent[2] else { panic!("{sent:?}") };
    assert!(m.text.contains("voicemail left"), "{}", m.text);
    // the voicemail itself goes to the group's channel as audio
    let Sent::Audio(Dest::Channel(ch), m, f) = &sent[3] else { panic!("{sent:?}") };
    assert_eq!((ch.as_str(), f.as_str()), ("sales", "voicemail-7.mp3"));
    assert!(m.buttons().any(|b| b.action == Action::VmHeard { id: 7, l: false }));
    assert_eq!(sent.len(), 4);
}

#[tokio::test]
async fn personal_missed_call_dm() {
    let s = setup(LinkMode::Auto);
    s.store.links.lock().unwrap().insert(("fake".into(), "mm-grace".into()), (4, "auto".into()));
    let ev = |reason| PbxEvent::CallMissed { call_id: "x".into(), caller: caller(), did: None, target: person(), reason, rang: vec![] };
    s.bot.on_event(&ev(MissReason::Forwarded)).await;
    assert!(s.chat.take().is_empty(), "no DM for an immediate forward");
    s.bot.on_event(&ev(MissReason::NoAnswer)).await;
    let sent = s.chat.take();
    let Sent::Post(Dest::User(u), m) = &sent[0] else { panic!("{sent:?}") };
    assert_eq!(u, "mm-grace");
    assert!(m.text.contains("Missed call from **Kunde GmbH (+4940123)**"), "{}", m.text);
    // voicemail with chat delivery: the voicemail message replaces the missed-call DM
    s.pbx.delivery.lock().unwrap().insert(4, VmDelivery::Chat);
    s.bot.on_event(&ev(MissReason::Voicemail)).await;
    assert!(s.chat.take().is_empty());
    s.bot.on_event(&PbxEvent::VoicemailNew { id: 7, call_id: None, owner: person(), caller: caller(), duration_s: 12 }).await;
    let sent = s.chat.take();
    assert!(matches!(&sent[..], [Sent::Audio(Dest::User(u), _, _)] if u == "mm-grace"), "{sent:?}");
    // mail-only users get no voicemail in chat
    s.pbx.delivery.lock().unwrap().insert(4, VmDelivery::Mail);
    s.bot.on_event(&PbxEvent::VoicemailNew { id: 7, call_id: None, owner: person(), caller: caller(), duration_s: 12 }).await;
    assert!(s.chat.take().is_empty());
}

#[tokio::test]
async fn voicemail_buttons_apply_to_the_mailbox() {
    let s = setup(LinkMode::Auto);
    let r = s.bot.handle(click("mm-grace", Action::VmHeard { id: 7, l: false })).await;
    assert_eq!(*s.pbx.vm_ops.lock().unwrap(), vec![(4, 7, VmOp::Heard)]);
    let u = r.update.unwrap();
    assert!(u.text.contains("heard by Grace"), "{}", u.text);
    assert!(!u.buttons().any(|b| matches!(b.action, Action::VmHeard { .. })));
    // no access → nothing happens
    let r = s.bot.handle(click("mm-admin", Action::VmDelete { id: 7, l: false })).await;
    assert!(reply_text(&r).contains("not found"));
    assert_eq!(s.pbx.vm_ops.lock().unwrap().len(), 1);
    // play: audio as DM to the clicker
    s.bot.handle(click("mm-grace", Action::VmPlay { id: 7 })).await;
    let sent = s.chat.take();
    assert!(matches!(&sent[..], [Sent::Audio(Dest::User(u), _, _)] if u == "mm-grace"), "{sent:?}");
}

#[tokio::test]
async fn voicemail_posts_follow_heard_and_delete() {
    let s = setup(LinkMode::Auto);
    // group voicemail: channel post + DM to a member who wants voicemails in chat
    s.store.groups.lock().unwrap().push(("fake".into(), "sales".into(), 3));
    s.store.links.lock().unwrap().insert(("fake".into(), "mm-grace".into()), (4, "auto".into()));
    s.bot.on_event(&PbxEvent::VoicemailNew { id: 7, call_id: None, owner: group(), caller: caller(), duration_s: 12 }).await;
    let sent = s.chat.take();
    assert!(matches!(&sent[..], [Sent::Audio(Dest::Channel(_), _, _)]), "{sent:?}");
    // "Play" sends a copy as DM; it is tracked as well
    s.bot.handle(click("mm-grace", Action::VmPlay { id: 7 })).await;
    s.chat.take();
    let posts = s.store.vm_posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 2, "{posts:?}");

    // heard somewhere else (web): every post is edited
    s.pbx.vm_ops.lock().unwrap().push((4, 7, VmOp::Heard));
    s.bot.on_event(&PbxEvent::VoicemailChanged { id: 7 }).await;
    let sent = s.chat.take();
    assert_eq!(sent.len(), 2, "{sent:?}");
    for x in &sent {
        let Sent::Edit(_, m) = x else { panic!("{sent:?}") };
        assert!(m.text.contains("heard by Grace"), "{}", m.text);
        assert!(!m.buttons().any(|b| matches!(b.action, Action::VmHeard { .. })));
    }

    // deleted with the button on the channel post: no edit of that post, the change event deletes
    // both posts and forgets them
    let mut c = click("mm-grace", Action::VmDelete { id: 7, l: false });
    c.kind = InboundKind::Action { action: Action::VmDelete { id: 7, l: false }, message: Some(posts[0].2.clone()) };
    let r = s.bot.handle(c).await;
    assert!(r.update.is_none() && r.message.is_none(), "{r:?}");
    s.bot.on_event(&PbxEvent::VoicemailChanged { id: 7 }).await;
    let sent = s.chat.take();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent.iter().all(|x| matches!(x, Sent::Delete(_))), "{sent:?}");
    assert!(s.store.vm_posts.lock().unwrap().is_empty());
    // a later change finds nothing to do
    s.bot.on_event(&PbxEvent::VoicemailChanged { id: 7 }).await;
    assert!(s.chat.take().is_empty());
}
