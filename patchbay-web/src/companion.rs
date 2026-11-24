// SPDX-License-Identifier: GPL-3.0-or-later
//! Call companion (widget at the bottom right of every page): the person's current calls with
//! actions (decline, reject = busy, hang up, mute, blind transfer), colleagues' ringing calls to
//! pick up, quick toggles (DND, group log in/out) and the caller's context (last calls, voicemails,
//! contact, personal blocklist, call notes – see `blocklist.rs`, `notes.rs`).
//!
//! Stateless on purpose: every request reads the live channel list from Asterisk
//! (CoreShowChannels, cached for a second for all users) and maps channels to devices and persons,
//! so the widget can never drift from what Asterisk really does. Only "via" (which number / group
//! a call came in on) comes from call tracking (`calltrack.rs` publishes it per Linkedid).
//! The browser polls `/me/live` every few seconds (see app.js). See docs/architecture.md
//! (call companion).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use axum::{
    Form, Json,
    extract::{Path, Query, State},
};
use patchbay_notify::event::TargetKind;
use serde::{Deserialize, Serialize};

use crate::{AppError, AppState, ami, auth::CurrentUser, db};

// --- Shared state ------------------------------------------------------------------------------

/// Which number / target a call is on (from call tracking), per Linkedid.
#[derive(Clone, Debug)]
pub struct Via {
    pub did: Option<String>,
    pub kind: TargetKind,
    pub number: String,
}

#[derive(Clone, Default)]
pub struct Live {
    via: Arc<RwLock<HashMap<String, Via>>>,
    /// channels muted through the widget (Asterisk has no event or query for MuteAudio)
    muted: Arc<Mutex<HashSet<String>>>,
    snap: Arc<tokio::sync::Mutex<Option<(Instant, Arc<Vec<Chan>>)>>>,
}

impl Live {
    pub fn set_via(&self, m: HashMap<String, Via>) {
        if let Ok(mut v) = self.via.write() {
            *v = m;
        }
    }

    fn via(&self, linkedid: &str) -> Option<Via> {
        self.via.read().ok()?.get(linkedid).cloned()
    }

    /// Live channels, at most one second old.
    async fn channels(&self, fresh: bool) -> anyhow::Result<Arc<Vec<Chan>>> {
        let mut s = self.snap.lock().await;
        if let Some((t, c)) = s.as_ref() {
            if !fresh && t.elapsed() < Duration::from_secs(1) {
                return Ok(c.clone());
            }
        }
        let c = Arc::new(ami::list("CoreShowChannels", "CoreShowChannel").await?.iter().map(Chan::from_ami).collect::<Vec<_>>());
        if let Ok(mut m) = self.muted.lock() {
            let live: HashSet<&str> = c.iter().map(|c| c.name.as_str()).collect();
            m.retain(|n| live.contains(n.as_str()));
        }
        *s = Some((Instant::now(), c.clone()));
        Ok(c)
    }

    fn is_muted(&self, chan: &str) -> bool {
        self.muted.lock().map(|m| m.contains(chan)).unwrap_or(false)
    }
}

// --- Channels ----------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Chan {
    name: String,
    linkedid: String,
    /// ChannelStateDesc: Down, Ring (dialling out), Ringing, Up, Busy …
    state: String,
    app: String,
    con_num: String,
    con_name: String,
    secs: u64,
    context: String,
    exten: String,
}

fn clean(s: &str) -> String {
    if s == "<unknown>" { String::new() } else { s.to_string() }
}

impl Chan {
    fn from_ami(m: &ami::Message) -> Chan {
        let g = |k: &str| m.get(k).cloned().unwrap_or_default();
        let secs = g("Duration").split(':').fold(0u64, |a, p| a * 60 + p.parse::<u64>().unwrap_or(0));
        Chan {
            name: g("Channel"),
            linkedid: g("Linkedid"),
            state: g("ChannelStateDesc"),
            app: g("Application"),
            con_num: clean(&g("ConnectedLineNum")),
            con_name: clean(&g("ConnectedLineName")),
            secs,
            context: g("Context"),
            exten: g("Exten"),
        }
    }

    /// Device (SIP username) of a PJSIP device channel.
    fn device(&self) -> Option<&str> {
        let rest = self.name.strip_prefix("PJSIP/")?;
        Some(rest.rsplit_once('-').map(|(e, _)| e).unwrap_or(rest))
    }

    /// Device of a delayed ringing leg "Local/<s>-<device>@ring-delay-…;1" (still waiting).
    fn delayed_device(&self) -> Option<&str> {
        let rest = self.name.strip_prefix("Local/")?;
        let (num, _) = rest.split_once("@ring-delay")?;
        if !self.name.ends_with(";1") {
            return None;
        }
        num.split_once('-').map(|(_, d)| d)
    }

    /// The device was dialled (incoming leg) – Application of a dialled channel is "AppDial".
    fn dialled(&self) -> bool {
        self.app == "AppDial"
    }

    /// A call without another party (mailbox, conference, echo test …): what the channel is
    /// connected to, as an English UI text (translated by the handler).
    fn service(&self) -> Option<&'static str> {
        Some(match (self.context.as_str(), self.app.as_str()) {
            ("vm-listen", _) | (_, "VoiceMailMain") => "Mailbox",
            (_, "ConfBridge") => "Conference",
            (_, "Echo") => "Echo test",
            (_, "MusicOnHold") => "Hold music",
            (_, "Read" | "WaitDigit" | "ControlPlayback") => "Voice menu",
            (_, "Queue") => "Queue",
            _ => return None,
        })
    }
}

// --- People and devices ----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Dev {
    pub user_id: i32,
    pub ext: i16,
    pub name: String,
    pub pickup: Vec<String>,
}

fn groups_of(pickup: &Option<String>) -> Vec<String> {
    pickup.as_deref().unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

async fn devices(db: &sqlx::PgPool) -> anyhow::Result<HashMap<String, Dev>> {
    let rows: Vec<(String, i32, i16, String, Option<String>)> =
        sqlx::query_as("SELECT sip_username, user_id, extension, display_name, pickup_group FROM active_devices").fetch_all(db).await?;
    Ok(rows.into_iter().map(|(s, u, e, n, p)| (s, Dev { user_id: u, ext: e, name: n, pickup: groups_of(&p) })).collect())
}

// --- View model (pure, unit-tested) -------------------------------------------------------------

#[derive(Serialize, Debug, PartialEq)]
pub struct CallView {
    /// Linkedid of the call (server checks every action against the person's own channels)
    id: String,
    /// ringing | active | calling
    state: &'static str,
    incoming: bool,
    name: String,
    number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    secs: u64,
    muted: bool,
    /// no other party: what the call is connected to (English key, translated in `live`)
    #[serde(skip)]
    service: Option<&'static str>,
    /// the person's channel (outgoing calls without a number: dialled number via Getvar)
    #[serde(skip)]
    chan: String,
    /// personal call to me: "reject" (busy) possible; group/queue/recall calls only "decline"
    can_reject: bool,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Offer {
    id: String,
    ext: i16,
    who: String,
    /// caller name/number, or empty when the viewer may only see "a call" (see `caller_visible`)
    caller: String,
    internal: bool,
    secs: u64,
}

pub struct Me<'a> {
    pub user_id: i32,
    pub ext: i16,
    /// call group numbers the person is a member of (caller details in pickup offers)
    pub groups: &'a HashSet<String>,
}

/// Labels for "via": DID e164 -> label, group number -> name, IVR number -> name
#[derive(Default)]
pub struct Labels {
    pub dids: HashMap<String, String>,
    pub groups: HashMap<String, String>,
}

fn via_label(v: &Option<Via>, l: &Labels) -> Option<String> {
    let v = v.as_ref()?;
    if v.kind == TargetKind::Group {
        if let Some(n) = l.groups.get(&v.number) {
            return Some(n.clone());
        }
    }
    v.did.as_ref().map(|d| l.dids.get(d).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| d.clone()))
}

pub fn my_calls(chans: &[Chan], devs: &HashMap<String, Dev>, me: &Me, via: &dyn Fn(&str) -> Option<Via>, l: &Labels, muted: &dyn Fn(&str) -> bool) -> Vec<CallView> {
    let mut by_call: Vec<(String, Vec<&Chan>)> = vec![];
    for c in chans {
        let mine = c.device().and_then(|d| devs.get(d)).is_some_and(|d| d.user_id == me.user_id);
        if !mine {
            continue;
        }
        match by_call.iter_mut().find(|(id, _)| *id == c.linkedid) {
            Some((_, v)) => v.push(c),
            None => by_call.push((c.linkedid.clone(), vec![c])),
        }
    }
    by_call
        .into_iter()
        .map(|(id, legs)| {
            let up = legs.iter().find(|c| c.state == "Up");
            let c = up.copied().unwrap_or(legs[0]);
            let incoming = c.dialled() || (c.app.is_empty() && c.state == "Ringing");
            let state = match (up.is_some(), incoming) {
                (true, _) => "active",
                (false, true) => "ringing",
                (false, false) => "calling",
            };
            let v = via(&id);
            let can_reject = state == "ringing" && v.as_ref().is_some_and(|v| v.kind == TargetKind::User && v.number == me.ext.to_string());
            CallView {
                via: if incoming { via_label(&v, l) } else { None },
                muted: up.is_some_and(|u| muted(&u.name)),
                id,
                state,
                incoming,
                name: if c.con_name != c.con_num { c.con_name.clone() } else { String::new() },
                number: if c.con_num.is_empty() && !c.dialled() && c.exten != "s" && c.exten.chars().all(|x| x.is_ascii_digit() || x == '*' || x == '#') {
                    c.exten.clone()
                } else {
                    c.con_num.clone()
                },
                service: if c.con_num.is_empty() && c.con_name.is_empty() { c.service() } else { None },
                chan: c.name.clone(),
                secs: c.secs,
                can_reject,
            }
        })
        .collect()
}

/// Colleagues' ringing calls the person may pick up: same rule as `*8<ext>` (ODBC_PICKUP_DIAL):
/// one of the person's devices shares a pickup group with the ringing device.
pub fn offers(chans: &[Chan], devs: &HashMap<String, Dev>, me: &Me, via: &dyn Fn(&str) -> Option<Via>) -> Vec<Offer> {
    let my_pickup: HashSet<&str> = devs.values().filter(|d| d.user_id == me.user_id).flat_map(|d| d.pickup.iter().map(String::as_str)).collect();
    let my_calls: HashSet<&str> =
        chans.iter().filter(|c| c.device().and_then(|d| devs.get(d)).is_some_and(|d| d.user_id == me.user_id)).map(|c| c.linkedid.as_str()).collect();
    let mut out: Vec<Offer> = vec![];
    for c in chans {
        if c.state != "Ringing" || !c.dialled() || my_calls.contains(c.linkedid.as_str()) {
            continue;
        }
        let Some(d) = c.device().and_then(|d| devs.get(d)) else { continue };
        if d.user_id == me.user_id || !d.pickup.iter().any(|g| my_pickup.contains(g.as_str())) {
            continue;
        }
        if out.iter().any(|o| o.id == c.linkedid && o.ext == d.ext) {
            continue;
        }
        let internal = c.con_num.len() <= 4;
        let visible = via(&c.linkedid).is_some_and(|v| v.kind == TargetKind::Group && me.groups.contains(&v.number));
        let caller = if visible {
            if c.con_name.is_empty() { c.con_num.clone() } else { format!("{} ({})", c.con_name, c.con_num) }
        } else {
            String::new()
        };
        out.push(Offer { id: c.linkedid.clone(), ext: d.ext, who: d.name.clone(), caller, internal, secs: c.secs });
    }
    out
}

// --- Widget texts ---------------------------------------------------------------------------------

const TEXTS: &[&str] = &[
    "Calls", "Ringing", "On the phone", "Calling", "via", "Decline", "Reject (busy)", "Hang up", "Mute", "Unmute",
    "Transfer", "Number or extension", "Transfer now", "Cancel", "Ringing at", "Pick up", "a call", "internal call",
    "external call", "Do not disturb", "My call groups", "Forwarded to", "Enable notifications", "No calls",
    "Last calls", "answered", "missed", "in", "out", "voicemails from this number", "Save as contact", "Incoming call",
    "Action failed.", "Phone system not reachable", "Pick up at your phone now.", "Muted", "Open", "Close",
    "Settings", "Decline: the call goes on as if you had not answered (e.g. to your mailbox); colleagues in a group keep ringing.",
    "Reject: the caller hears busy.", "Colleagues", "Call groups", "My contacts", "Nothing found", "Services",
    "Open contact", "Block this number for me", "Blocked for you", "Open in",
    "Block this number for you? Calls from it to you personally will hear busy; calls to your call groups still reach you.",
    "Notes", "Note", "Add note", "Edit", "Delete", "Save", "Visible to all colleagues", "Delete this note?",
];

pub fn i18n(t: &crate::i18n::Tr) -> String {
    let m: serde_json::Map<String, serde_json::Value> = TEXTS.iter().map(|k| (k.to_string(), serde_json::Value::String(t.s(k).to_string()))).collect();
    serde_json::Value::Object(m).to_string()
}

// --- HTTP ------------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct GroupToggle {
    id: i32,
    number: i16,
    name: String,
    on: bool,
}

#[derive(Serialize)]
pub struct LiveView {
    calls: Vec<CallView>,
    offers: Vec<Offer>,
    dnd: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    forward: Option<String>,
    groups: Vec<GroupToggle>,
    /// own active devices for "call from"
    devices: Vec<DeviceOpt>,
    /// Asterisk not reachable: no call list
    offline: bool,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct DeviceOpt {
    id: i32,
    label: String,
}

/// The person of a session with phone permission (widget and self-service endpoints of the
/// companion: blocklist, notes).
pub(crate) fn uid(u: &CurrentUser) -> Result<i32, AppError> {
    if !u.may_phone {
        return Err(AppError::Forbidden("No phone permission.".into()));
    }
    u.phone_uid()
}

async fn me_ext(app: &AppState, uid: i32) -> Result<(i16, bool), AppError> {
    Ok(sqlx::query_as("SELECT extension, dnd FROM users WHERE id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?)
}

async fn labels(app: &AppState) -> anyhow::Result<Labels> {
    let dids: Vec<(String, String)> = sqlx::query_as("SELECT e164, label FROM dids").fetch_all(&app.db).await?;
    let groups: Vec<(String, String)> = sqlx::query_as("SELECT number::text, name FROM groups WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    Ok(Labels { dids: dids.into_iter().collect(), groups: groups.into_iter().collect() })
}

struct Snapshot {
    chans: Arc<Vec<Chan>>,
    devs: HashMap<String, Dev>,
    ext: i16,
    groups: Vec<crate::selfservice::MyGroup>,
}

async fn snapshot(app: &AppState, uid: i32, fresh: bool) -> Result<Snapshot, AppError> {
    let (ext, _) = me_ext(app, uid).await?;
    let chans = app.live.channels(fresh).await.map_err(|e| AppError::Internal(e.context("Asterisk not reachable")))?;
    Ok(Snapshot { chans, devs: devices(&app.db).await?, ext, groups: crate::selfservice::my_groups(app, uid).await? })
}

/// GET /me/live
pub async fn live(State(app): State<AppState>, u: CurrentUser) -> Result<Json<LiveView>, AppError> {
    let uid = uid(&u)?;
    let (ext, dnd) = me_ext(&app, uid).await?;
    let groups = crate::selfservice::my_groups(&app, uid).await?;
    let forward = crate::selfservice::forward_text(&app, uid).await?;
    let toggles = groups.iter().map(|g| GroupToggle { id: g.id, number: g.number, name: g.name.clone(), on: g.logged_in }).collect();
    let my_devices: Vec<DeviceOpt> = sqlx::query_as("SELECT id, COALESCE(number::text || ' · ', '') || label AS label FROM active_devices WHERE user_id = $1 ORDER BY number, id")
        .bind(uid)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let chans = match app.live.channels(false).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("companion: {e:#}");
            return Ok(Json(LiveView { calls: vec![], offers: vec![], dnd, forward, groups: toggles, devices: my_devices, offline: true }));
        }
    };
    let devs = devices(&app.db).await?;
    let gnums: HashSet<String> = groups.iter().map(|g| g.number.to_string()).collect();
    let me = Me { user_id: uid, ext, groups: &gnums };
    let via = |id: &str| app.live.via(id);
    let labels = labels(&app).await?;
    let mut calls = my_calls(&chans, &devs, &me, &via, &labels, &|c| app.live.is_muted(c));
    let t = crate::i18n::Tr(u.lang);
    for c in calls.iter_mut() {
        if let Some(svc) = c.service {
            c.name = t.s(svc).to_string();
        } else if !c.incoming && c.number.is_empty() {
            // outgoing without connected line (IAX network, trunks before answer): the number the
            // phone dialled (the channel's extension is already "s" inside sub-outbound)
            if let Ok(r) = ami::action(&[("Action", "Getvar"), ("Channel", &c.chan), ("Variable", "CALLERID(dnid)")]).await {
                c.number = r.get("Value").cloned().unwrap_or_default().chars().filter(|x| x.is_ascii_digit() || matches!(x, '*' | '#' | '+')).take(32).collect();
            }
        }
    }
    // names from the own contacts / colleagues where the channel has none (outgoing calls)
    for c in calls.iter_mut().filter(|c| c.name.is_empty() && !c.number.is_empty()) {
        let n: Option<String> = sqlx::query_scalar(
            "SELECT COALESCE(contact_name($1, $2), (SELECT display_name FROM users WHERE extension::text = $2 AND deleted_at IS NULL))",
)
        .bind(ext.to_string())
        .bind(&c.number)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
        c.name = n.unwrap_or_default();
    }
    let offers = offers(&chans, &devs, &me, &via);
    Ok(Json(LiveView { calls, offers, dnd, forward, groups: toggles, devices: my_devices, offline: false }))
}

#[derive(Serialize)]
pub(crate) struct Reply {
    ok: bool,
    msg: String,
}

fn ok() -> Json<Reply> {
    Json(Reply { ok: true, msg: String::new() })
}

fn fail(msg: &str) -> Json<Reply> {
    Json(Reply { ok: false, msg: msg.into() })
}

/// Widget answer for a self-service change: refused input (BadRequest) becomes a failed reply with
/// its message (shown in the toast), other errors stay errors.
pub(crate) fn reply(r: Result<(), AppError>) -> Result<Json<Reply>, AppError> {
    match r {
        Ok(()) => Ok(ok()),
        Err(AppError::BadRequest(m)) => Ok(fail(&m)),
        Err(e) => Err(e),
    }
}

async fn ami_ok(fields: &[(&str, &str)]) -> anyhow::Result<bool> {
    let r = ami::action(fields).await?;
    if r.get("Response").map(String::as_str) != Some("Success") {
        tracing::info!("companion: AMI {} failed: {}", fields[0].1, r.get("Message").cloned().unwrap_or_default());
        return Ok(false);
    }
    Ok(true)
}

#[derive(Deserialize, Default)]
pub struct ActionForm {
    #[serde(default)]
    to: String,
}

/// POST /me/live/{id}/{op}: decline | reject | hangup | mute | unmute | transfer (form: to)
pub async fn action(
    State(app): State<AppState>,
    u: CurrentUser,
    Path((id, op)): Path<(String, String)>,
    Form(f): Form<ActionForm>,
) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    let s = snapshot(&app, uid, true).await?;
    let mine: Vec<&Chan> = s
        .chans
        .iter()
        .filter(|c| c.linkedid == id)
        .filter(|c| c.device().or(c.delayed_device()).and_then(|d| s.devs.get(d)).is_some_and(|d| d.user_id == uid))
        .collect();
    if mine.is_empty() {
        return Ok(fail("call ended"));
    }
    let up = mine.iter().find(|c| c.state == "Up" && c.device().is_some()).copied();
    let ringing: Vec<&Chan> = mine.iter().filter(|c| c.delayed_device().is_some() || (c.state == "Ringing" && c.dialled())).copied().collect();
    let audit = |action: &'static str, extra: serde_json::Value| {
        let app = app.clone();
        let sub = u.session.sub.clone();
        async move { db::audit(&app.db, &sub, action, &format!("user:{}", s.ext), extra).await }
    };
    let done = match op.as_str() {
        "decline" | "reject" => {
            if up.is_some() || ringing.is_empty() {
                return Ok(fail("already answered"));
            }
            if op == "reject" {
                let v = app.live.via(&id);
                if !v.is_some_and(|v| v.kind == TargetKind::User && v.number == s.ext.to_string()) {
                    return Ok(fail("only for personal calls"));
                }
                // sub-user reads and deletes the key after Dial() returns: busy instead of the no-answer rule
                let key = format!("{id}-{}", s.ext);
                if !ami_ok(&[("Action", "DBPut"), ("Family", "pbx_divert"), ("Key", &key), ("Val", "busy")]).await? {
                    return Ok(fail("failed"));
                }
            }
            for c in &ringing {
                ami_ok(&[("Action", "Hangup"), ("Channel", &c.name), ("Cause", "21")]).await?;
            }
            audit(if op == "reject" { "call.reject" } else { "call.decline" }, serde_json::json!({})).await?;
            true
        }
        "hangup" => {
            let Some(c) = up.or(mine.iter().find(|c| c.device().is_some()).copied()) else { return Ok(fail("call ended")) };
            let r = ami_ok(&[("Action", "Hangup"), ("Channel", &c.name), ("Cause", "16")]).await?;
            audit("call.hangup", serde_json::json!({})).await?;
            r
        }
        "mute" | "unmute" => {
            let Some(c) = up else { return Ok(fail("not connected")) };
            let on = op == "mute";
            let r = ami_ok(&[("Action", "MuteAudio"), ("Channel", &c.name), ("Direction", "in"), ("State", if on { "on" } else { "off" })]).await?;
            if r {
                if let Ok(mut m) = app.live.muted.lock() {
                    if on {
                        m.insert(c.name.clone());
                    } else {
                        m.remove(&c.name);
                    }
                }
            }
            r
        }
        "transfer" => {
            let Some(c) = up else { return Ok(fail("not connected")) };
            let to: String = f.to.chars().filter(|c| c.is_ascii_digit() || *c == '+' || *c == '*').collect();
            if to.is_empty() || to.len() > 20 || to != f.to.trim().replace(' ', "") {
                return Ok(fail("invalid number"));
            }
            // like a transfer on the phone (REFER): from-device with BLINDTRANSFER set, so the dial
            // rights of the transferring device and the transfer recall apply
            let r = ami_ok(&[("Action", "BlindTransfer"), ("Channel", &c.name), ("Context", "from-device"), ("Exten", &to)]).await?;
            audit("call.transfer", serde_json::json!({"to": to})).await?;
            r
        }
        _ => return Err(AppError::NotFound),
    };
    Ok(if done { ok() } else { fail("failed") })
}

#[derive(Deserialize)]
pub struct PickupForm {
    ext: i16,
}

/// POST /me/live/{id}/pickup (form: ext): rings the person's devices; on answer the colleague's
/// ringing call is picked up (PickupChan), with the same pickup group rule as `*8<ext>`.
pub async fn pickup(State(app): State<AppState>, u: CurrentUser, Path(id): Path<String>, Form(f): Form<PickupForm>) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    let s = snapshot(&app, uid, true).await?;
    let gnums: HashSet<String> = s.groups.iter().map(|g| g.number.to_string()).collect();
    let me = Me { user_id: uid, ext: s.ext, groups: &gnums };
    let via = |id: &str| app.live.via(id);
    let Some(o) = offers(&s.chans, &s.devs, &me, &via).into_iter().find(|o| o.id == id && o.ext == f.ext) else {
        return Ok(fail("call ended"));
    };
    let my_pickup: HashSet<&String> = s.devs.values().filter(|d| d.user_id == uid).flat_map(|d| d.pickup.iter()).collect();
    let targets: Vec<String> =
        s.devs.iter().filter(|(_, d)| d.ext == o.ext && d.pickup.iter().any(|g| my_pickup.contains(g))).map(|(sip, _)| format!("PJSIP/{sip}")).collect();
    if targets.is_empty() {
        return Ok(fail("not allowed"));
    }
    let shown = if o.caller.is_empty() { o.who.clone() } else { o.caller.clone() };
    let shown: String = shown.chars().filter(|c| !matches!(c, '"' | '<' | '>' | '\r' | '\n')).take(40).collect();
    let r = ami::originate_app(&format!("Local/{}@group-member", s.ext), "PickupChan", &format!("{},p", targets.join("&")), &format!("\"⤵ {shown}\" <{}>", o.ext))
        .await?;
    db::audit(&app.db, &u.session.sub, "call.pickup", &format!("user:{}", s.ext), serde_json::json!({"from": o.ext})).await?;
    Ok(if r { ok() } else { fail("failed") })
}

#[derive(Deserialize)]
pub struct OnForm {
    #[serde(default)]
    on: String,
}

/// POST /me/live/dnd (on=1|0)
pub async fn dnd(State(app): State<AppState>, u: CurrentUser, Form(f): Form<OnForm>) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    crate::selfservice::set_dnd(&app, uid, Some(f.on == "1")).await?;
    Ok(ok())
}

/// POST /me/live/group/{gid} (on=1|0)
pub async fn group(State(app): State<AppState>, u: CurrentUser, Path(gid): Path<i32>, Form(f): Form<OnForm>) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    if !crate::admin::set_logged_in(&app.db, &u.session.sub, gid, uid, f.on == "1").await? {
        return Err(AppError::NotFound);
    }
    Ok(ok())
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
}

#[derive(Serialize)]
pub struct Hit {
    label: String,
    number: String,
}

#[derive(Serialize, Clone)]
pub struct BookEntry {
    /// colleague | group | service | contact
    kind: &'static str,
    label: String,
    /// organisation / "mobile" / what a feature code does
    sub: String,
    number: String,
}

/// The phone book of the dial field: colleagues (as on the phone book page, PBX_PHONEBOOK), call
/// groups, services (voice menus, company and custom apps from `help::company_numbers` – same as
/// the help page and the CardDAV services –, own apps, feature codes one calls) and own contacts.
/// The popup shows it whole, the dial suggestions filter it.
async fn book_entries(app: &AppState, uid: i32, t: crate::i18n::Tr) -> Result<Vec<BookEntry>, AppError> {
    let db = |e: sqlx::Error| AppError::Internal(e.into());
    let colleagues: Vec<(String, String)> = sqlx::query_as(
        "SELECT display_name, extension::text FROM users
         WHERE deleted_at IS NULL AND enabled AND (in_phonebook OR $2) AND id <> $1 ORDER BY lower(display_name)",
)
    .bind(uid)
    .bind(app.cfg.phonebook_enforced)
    .fetch_all(&app.db)
    .await
    .map_err(db)?;
    let (groups, ivrs, company_apps) = crate::help::company_numbers(&app.db).await?;
    let own_apps: Vec<crate::help::Numbered> =
        sqlx::query_as("SELECT number::text AS number, name, kind FROM apps WHERE owner_user_id = $1 AND deleted_at IS NULL AND enabled ORDER BY number")
            .bind(uid)
            .fetch_all(&app.db)
            .await
            .map_err(db)?;
    let contacts: Vec<(String, Option<String>, Option<String>, Option<String>)> =
        // own contacts and the shared phone book
        // own contacts, the shared phone book and the external phone books
        sqlx::query_as(
            "SELECT name, organization, phone, mobile FROM (
               SELECT name, organization, phone, mobile, 0 AS src, id::text AS k FROM contacts
               WHERE user_id = $1 OR ($2 AND user_id IS NULL AND deleted_at IS NULL)
               UNION ALL
               SELECT name, organization, phone, mobile, 1, book || '/' || res FROM (
                 SELECT * FROM external_contacts WHERE phone IS NOT NULL OR mobile IS NOT NULL ORDER BY lower(name) LIMIT 5000
               ) e
             ) x ORDER BY lower(name), src, k",
        )
            .bind(uid)
            .bind(app.cfg.shared_contacts.on())
            .fetch_all(&app.db)
            .await
            .map_err(db)?;
    let e = |kind, label: String, sub: String, number: String| BookEntry { kind, label, sub, number };
    let mut out: Vec<BookEntry> = colleagues.into_iter().map(|(n, x)| e("colleague", n, String::new(), x)).collect();
    out.extend(groups.into_iter().map(|g| e("group", g.name, String::new(), g.number)));
    out.extend(ivrs.into_iter().map(|i| e("service", i.name, t.s("Voice menu").to_string(), i.number)));
    out.extend(company_apps.into_iter().chain(own_apps).map(|a| e("service", a.name, t.s(crate::apps::kind_label(&a.kind)).to_string(), a.number)));
    // feature codes one calls (mailbox, echo test, pickup); not the switches the web UI has
    // (DND on/off, forwarding off) and not emergency numbers
    out.extend(
        crate::help::FEATURE_CODES
            .iter()
            .filter(|f| f.arg.is_none() && !matches!(f.prefix, "*760" | "*761" | "#21#"))
            .map(|f| e("service", t.s(f.name).to_string(), t.s(f.help).to_string(), f.prefix.to_string())),
);
    for (name, org, phone, mobile) in contacts {
        let org = org.filter(|o| !o.is_empty());
        if let Some(p) = phone.filter(|p| !p.is_empty()) {
            out.push(e("contact", name.clone(), org.clone().unwrap_or_default(), p));
        }
        if let Some(m) = mobile.filter(|m| !m.is_empty()) {
            let sub = [org.as_deref(), Some(t.s("Mobile"))].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            out.push(e("contact", name, sub, m));
        }
    }
    Ok(out)
}

/// Dial suggestions: entries whose name or description contains the text, or whose number
/// contains its digits (at least two); at most 10.
fn search_book(book: &[BookEntry], q: &str) -> Vec<Hit> {
    let ql = q.to_lowercase();
    let digits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
    book.iter()
        .filter(|b| {
            b.label.to_lowercase().contains(&ql)
                || (b.kind != "service" && b.sub.to_lowercase().contains(&ql))
                || (digits.len() >= 2 && b.number.chars().filter(|c| c.is_ascii_digit()).collect::<String>().contains(&digits))
        })
        .take(10)
        .map(|b| Hit { label: if b.kind == "contact" && !b.sub.is_empty() { format!("{} · {}", b.label, b.sub) } else { b.label.clone() }, number: b.number.clone() })
        .collect()
}

/// GET /me/live/search?q=…: dial suggestions from the phone book (`book_entries`).
pub async fn search(State(app): State<AppState>, u: CurrentUser, Query(q): Query<SearchQuery>) -> Result<Json<Vec<Hit>>, AppError> {
    let uid = uid(&u)?;
    let q: String = q.q.trim().chars().filter(|c| !c.is_control()).take(40).collect();
    if q.chars().count() < 2 {
        return Ok(Json(vec![]));
    }
    let book = book_entries(&app, uid, crate::i18n::Tr(u.lang)).await?;
    Ok(Json(search_book(&book, &q)))
}

/// GET /me/live/book: the phone book popup of the dial field (`book_entries`), filtered in the browser.
pub async fn book(State(app): State<AppState>, u: CurrentUser) -> Result<Json<Vec<BookEntry>>, AppError> {
    let uid = uid(&u)?;
    Ok(Json(book_entries(&app, uid, crate::i18n::Tr(u.lang)).await?))
}

#[derive(Deserialize)]
pub struct ContextQuery {
    number: String,
}

#[derive(Serialize)]
pub struct PastCall {
    at: String,
    incoming: bool,
    answered: bool,
    secs: i32,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ContactRef {
    id: i32,
    name: String,
}

#[derive(Serialize)]
pub struct ExtRef {
    name: String,
    book: String,
    url: Option<String>,
}

#[derive(Serialize, Default)]
pub struct Context {
    calls: Vec<PastCall>,
    voicemails: i64,
    /// own contact with this number ("open contact" instead of "save as contact")
    contact: Option<ContactRef>,
    /// entry of an external phone book with this number (name, book, link to the other system)
    external: Option<ExtRef>,
    /// the number as E.164 (prefill of a new contact); None for internal / invalid numbers, which
    /// get no blocklist entry and no notes
    e164: Option<String>,
    /// on the person's own blocklist
    blocked: bool,
    notes: Vec<crate::notes::Note>,
    /// PBX_SHARED_NOTES: the "visible to all colleagues" option is offered
    shared_notes: bool,
}

/// GET /me/live/context?number=…: the person's last calls with this number, voicemails from it
/// (own and call group mailboxes), the own contact, personal blocklist state and call notes.
pub async fn context(State(app): State<AppState>, u: CurrentUser, Query(q): Query<ContextQuery>) -> Result<Json<Context>, AppError> {
    let uid = uid(&u)?;
    let number: String = q.number.chars().filter(|c| c.is_ascii_digit() || *c == '+').take(20).collect();
    if number.len() < 2 {
        return Ok(Json(Context::default()));
    }
    let all = crate::contacts::calls_of(&app.db, uid, 500).await?;
    let nums: Vec<String> = all.iter().map(|c| c.number.clone()).chain(std::iter::once(number.clone())).collect();
    let keys: HashMap<String, Option<String>> = sqlx::query_as::<_, (String, Option<String>)>("SELECT n, caller_e164(n) FROM unnest($1::text[]) AS n")
        .bind(&nums)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .into_iter()
        .collect();
    let key = keys.get(&number).cloned().flatten();
    let calls: Vec<PastCall> = all
        .into_iter()
        .filter(|c| c.number == number || (key.is_some() && keys.get(&c.number).cloned().flatten() == key))
        .take(5)
        .map(|c| PastCall { at: c.at, incoming: c.direction == "in", answered: c.answered, secs: c.billsec })
        .collect();
    let voicemails: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM voicemails v WHERE v.deleted_at IS NULL AND v.caller_e164 = caller_e164($2)
           AND ((v.owner_type = 'user' AND v.owner_id = $1)
             OR (v.owner_type = 'group' AND v.owner_id IN (SELECT group_id FROM group_members WHERE user_id = $1 AND enabled)))",
)
    .bind(uid)
    .bind(&number)
    .fetch_one(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    // same matching as contact_name() : as written or as E.164
    let contact: Option<ContactRef> = sqlx::query_as(
        "SELECT id, name FROM contacts WHERE (user_id = $1 OR ($3 AND user_id IS NULL AND deleted_at IS NULL)) AND length($2) > 4
           AND (phone = $2 OR mobile = $2 OR phone_e164 = caller_e164($2) OR mobile_e164 = caller_e164($2))
         ORDER BY user_id IS NULL, id LIMIT 1",
)
    .bind(uid)
    .bind(&number)
    .bind(app.cfg.shared_contacts.on())
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let e164 = crate::blocklist::full_number(&number, &app.cfg.country_code);
    let (blocked, notes) = match &e164 {
        Some(e) => (crate::blocklist::personal_blocked(&app, uid, e).await?, crate::notes::notes_for(&app, uid, e).await?),
        None => (false, vec![]),
    };
    // external phone books: same matching; the book's display name from the configuration
    let ext: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT book, name, url FROM external_contacts WHERE length($1) > 4
           AND (phone_e164 = caller_e164($1) OR mobile_e164 = caller_e164($1))
         ORDER BY book, res LIMIT 1",
    )
    .bind(&number)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let external = ext.map(|(book, name, url)| ExtRef {
        book: app.cfg.external_books.iter().find(|b| b.id == book).map(|b| b.name.clone()).unwrap_or(book),
        name,
        url,
    });
    Ok(Json(Context { calls, voicemails, contact, external, e164, blocked, notes, shared_notes: app.cfg.shared_notes }))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ch(name: &str, lid: &str, state: &str, app: &str, num: &str, cname: &str) -> Chan {
        Chan { name: name.into(), linkedid: lid.into(), state: state.into(), app: app.into(), con_num: num.into(), con_name: cname.into(), secs: 7, ..Default::default() }
    }

    fn devs() -> HashMap<String, Dev> {
        let d = |u, e, n: &str, p: &[&str]| Dev { user_id: u, ext: e, name: n.into(), pickup: p.iter().map(|s| s.to_string()).collect() };
        [("me1", d(1, 1020, "Me", &["a"])), ("me2", d(1, 1020, "Me", &["a"])), ("tob", d(2, 1010, "Grace", &["a"])), ("ext", d(3, 1030, "Other", &["b"]))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn ringing_on_two_devices_is_one_call() {
        let chans = vec![
            ch("PJSIP/provider-01", "L1", "Up", "Queue", "0451123", ""),
            ch("PJSIP/me1-02", "L1", "Ringing", "AppDial", "0451123", "Müller"),
            ch("PJSIP/me2-03", "L1", "Ringing", "AppDial", "0451123", "Müller"),
        ];
        let g = HashSet::new();
        let me = Me { user_id: 1, ext: 1020, groups: &g };
        let via = |_: &str| Some(Via { did: Some("+4940".into()), kind: TargetKind::User, number: "1020".into() });
        let l = Labels { dids: [("+4940".to_string(), "Zentrale".to_string())].into(), ..Default::default() };
        let v = my_calls(&chans, &devs(), &me, &via, &l, &|_| false);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].state, "ringing");
        assert!(v[0].can_reject && v[0].incoming);
        assert_eq!(v[0].via.as_deref(), Some("Zentrale"));
        assert_eq!(v[0].name, "Müller");
    }

    #[test]
    fn group_call_cannot_be_rejected_and_answered_wins() {
        let chans = vec![ch("PJSIP/me1-02", "L1", "Up", "AppDial", "0451", ""), ch("PJSIP/me2-03", "L1", "Ringing", "AppDial", "0451", "")];
        let g = HashSet::new();
        let me = Me { user_id: 1, ext: 1020, groups: &g };
        let via = |_: &str| Some(Via { did: None, kind: TargetKind::Group, number: "5020".into() });
        let v = my_calls(&chans, &devs(), &me, &via, &Labels::default(), &|n| n == "PJSIP/me1-02");
        assert_eq!(v[0].state, "active");
        assert!(!v[0].can_reject);
        assert!(v[0].muted);
    }

    #[test]
    fn outgoing_is_calling() {
        let chans = vec![ch("PJSIP/me1-02", "L2", "Ring", "Dial", "0451999", "")];
        let g = HashSet::new();
        let me = Me { user_id: 1, ext: 1020, groups: &g };
        let v = my_calls(&chans, &devs(), &me, &|_| None, &Labels::default(), &|_| false);
        assert_eq!((v[0].state, v[0].incoming), ("calling", false));
    }

    #[test]
    fn pickup_offers_follow_pickup_groups_and_privacy() {
        let chans = vec![
            ch("PJSIP/tob-05", "L3", "Ringing", "AppDial", "0451777", "Kunde"),
            ch("PJSIP/ext-06", "L4", "Ringing", "AppDial", "0451888", "Fremd"),
        ];
        let mut g = HashSet::new();
        let me = Me { user_id: 1, ext: 1020, groups: &g };
        let personal = |_: &str| Some(Via { did: None, kind: TargetKind::User, number: "1010".into() });
        let o = offers(&chans, &devs(), &me, &personal);
        assert_eq!(o.len(), 1, "other pickup group is not offered");
        assert_eq!((o[0].ext, o[0].caller.as_str()), (1010, ""), "personal call: no caller details");
        g.insert("5020".to_string());
        let me = Me { user_id: 1, ext: 1020, groups: &g };
        let group = |_: &str| Some(Via { did: None, kind: TargetKind::Group, number: "5020".into() });
        assert_eq!(offers(&chans, &devs(), &me, &group)[0].caller, "Kunde (0451777)");
    }

    #[test]
    fn dial_suggestions() {
        let e = |kind, label: &str, sub: &str, number: &str| BookEntry { kind, label: label.into(), sub: sub.into(), number: number.into() };
        let book = vec![
            e("colleague", "Tom Kilburn", "", "1018"),
            e("group", "Sales DE", "", "5120"),
            e("service", "Mailbox", "Mailbox: listen to your messages", "*97"),
            e("contact", "Müller GmbH", "Mobile", "0176 1234567"),
        ];
        let labels = |q| search_book(&book, q).into_iter().map(|h| h.label).collect::<Vec<_>>();
        assert_eq!(labels("tom"), ["Tom Kilburn"]);
        assert_eq!(labels("mail"), ["Mailbox"]);
        assert_eq!(labels("51"), ["Sales DE"]);
        assert_eq!(labels("1234"), ["Müller GmbH · Mobile"]);
        assert_eq!(labels("mobile"), ["Müller GmbH · Mobile"], "contacts match their organisation / mobile");
    }

    #[test]
    fn mailbox_call_has_a_name() {
        let mut c = ch("PJSIP/me1-09", "L9", "Up", "Read", "", "");
        c.context = "vm-listen".into();
        c.exten = "s".into();
        let g = HashSet::new();
        let me = Me { user_id: 1, ext: 1020, groups: &g };
        let v = my_calls(&[c], &devs(), &me, &|_| None, &Labels::default(), &|_| false);
        assert_eq!((v[0].service, v[0].number.as_str(), v[0].state), (Some("Mailbox"), "", "active"));
    }

    #[test]
    fn widget_texts_are_translated() {
        let de = crate::i18n::Tr(crate::i18n::Lang::De);
        let missing: Vec<&str> = TEXTS.iter().copied().filter(|k| de.s(k) == *k).collect();
        assert!(missing.is_empty(), "no German text: {missing:?}");
    }

    #[test]
    fn delayed_leg_device() {
        let c = ch("Local/5-me2@ring-delay-0000001a;1", "L1", "Up", "Wait", "", "");
        assert_eq!(c.delayed_device(), Some("me2"));
        assert_eq!(ch("Local/5-me2@ring-delay-0000001a;2", "L1", "Up", "", "", "").delayed_device(), None);
    }
}
