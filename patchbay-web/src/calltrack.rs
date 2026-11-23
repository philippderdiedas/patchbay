// SPDX-License-Identifier: GPL-3.0-or-later
//! Call tracking: combines Asterisk Manager events into business-level calls and produces the
//! domain events of patchbay-notify (call.incoming/answered/missed/ended, presence.changed).
//!
//! A call is everything with the same `Linkedid`. The dialplan marks the business steps with
//! `UserEvent`s (sub-user, sub-group, sub-ivr → `PbxTarget`; sub-mailbox → `PbxVoicemail`); the
//! channel events tell which devices rang (`DialBegin`), who answered (`DialEnd` ANSWER on a
//! PJSIP device) and when the call is over (last channel hung up).
//!
//! The tracker itself is a pure state machine (unit tests below replay recorded event sequences);
//! `spawn` connects it to AMI with reconnects and hands the events to `events::Events`.

use std::collections::{HashMap, HashSet};

use patchbay_notify::event::{Answerer, MissReason, Party, PbxEvent, Presence, Target, TargetKind};

use crate::{AppState, ami};

pub type Msg = HashMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ringing,
    Answered,
    Missed,
    /// IVR left for another target
    Passed,
}

#[derive(Debug)]
struct Tgt {
    t: Target,
    state: State,
    /// SIP endpoints dialled while this target was current
    rang: Vec<String>,
    /// person with an immediate forward / DND (never rang)
    fwd: bool,
    dnd: bool,
}

#[derive(Debug)]
struct Call {
    id: String,
    started: f64,
    last: f64,
    caller: Party,
    did: Option<String>,
    targets: Vec<Tgt>,
    answered: Option<f64>,
    chans: HashSet<String>,
    /// call flow for the admin call log (callflow.rs)
    origin: FlowOrigin,
    dials: Vec<FlowDial>,
    voicemail: bool,
    answered_by: Option<String>,
}

/// Where a call started: the first channel (a device, a trunk, an originate).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FlowOrigin {
    /// PJSIP endpoint of the first channel ("" for Local/IAX2/originate)
    #[serde(default)]
    pub ep: String,
    /// channel technology and peer for non-PJSIP origins, e.g. "IAX2/site124"
    #[serde(default)]
    pub chan: String,
    /// number dialled on the first channel (outbound/internal calls)
    #[serde(default)]
    pub exten: String,
    #[serde(default)]
    pub num: String,
    #[serde(default)]
    pub name: String,
}

/// One Dial() leg to a PJSIP endpoint (device or trunk) or IAX2 peer.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FlowDial {
    /// endpoint (PJSIP) or "IAX2/<peer>"
    pub ep: String,
    /// dialled number on a trunk ("0176…@sip-5" → "0176…"), empty for devices
    #[serde(default)]
    pub to: String,
    /// caller ID sent on this leg
    #[serde(default)]
    pub clip: String,
    /// DialEnd status (ANSWER, BUSY, NOANSWER, …); empty while unknown
    #[serde(default)]
    pub status: String,
    #[serde(skip)]
    pub(crate) chan: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FlowStep {
    /// user | group | ivr
    pub kind: String,
    pub number: String,
    /// answered | missed | passed | ringing
    pub state: String,
    #[serde(default)]
    pub fwd: bool,
    #[serde(default)]
    pub dnd: bool,
}

/// A finished call as stored in call_flows (written by `spawn`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Flow {
    #[serde(skip)]
    pub linkedid: String,
    #[serde(skip)]
    pub started: f64,
    #[serde(skip)]
    pub duration_s: u64,
    #[serde(skip)]
    pub talk_s: Option<u64>,
    pub origin: FlowOrigin,
    #[serde(default)]
    pub did: Option<String>,
    #[serde(default)]
    pub steps: Vec<FlowStep>,
    #[serde(default)]
    pub dials: Vec<FlowDial>,
    #[serde(default)]
    pub voicemail: bool,
    #[serde(default)]
    pub answered_by: Option<String>,
}

#[derive(Default)]
pub struct Tracker {
    calls: HashMap<String, Call>,
    /// channel uniqueid → call id
    chan_call: HashMap<String, String>,
    /// finished calls not yet stored (drained by `take_flows`)
    flows: Vec<Flow>,
}

/// "PJSIP/abc-0000002a" → ("abc", ""), "IAX2/site124-1234" → ("", "IAX2/site124"), Local → ("", "")
fn origin_of(chan: &str) -> (String, String) {
    if let Some(ep) = pjsip_endpoint(chan) {
        return (ep.to_string(), String::new());
    }
    match chan.strip_prefix("IAX2/") {
        Some(rest) => (String::new(), format!("IAX2/{}", rest.rsplit_once('-').map(|(p, _)| p).unwrap_or(rest))),
        None => (String::new(), String::new()),
    }
}

fn get<'a>(m: &'a Msg, k: &str) -> &'a str {
    m.get(k).map(String::as_str).unwrap_or("")
}

/// "PJSIP/abc123-0000002a" → "abc123"
fn pjsip_endpoint(chan: &str) -> Option<&str> {
    let rest = chan.strip_prefix("PJSIP/")?;
    Some(rest.rsplit_once('-').map(|(e, _)| e).unwrap_or(rest))
}

fn party(m: &Msg) -> Party {
    let num = get(m, "CallerIDNum");
    let name = get(m, "CallerIDName");
    let clean = |s: &str| Some(s.to_string()).filter(|s| !s.is_empty() && s != "<unknown>");
    Party { number: clean(num).unwrap_or_default(), name: clean(name).filter(|n| n != num) }
}

pub fn presence_of(status: &str) -> Presence {
    match status {
        "Idle" => Presence::Available,
        "Ringing" => Presence::Ringing,
        "Unavailable" | "" => Presence::Offline,
        _ => Presence::Busy, // InUse, Busy, OnHold, InUse&Ringing
    }
}

impl Tracker {
    pub fn open_calls(&self) -> usize {
        self.calls.len()
    }

    /// Per open call (Linkedid): the number it came in on and its current target (companion widget).
    pub fn via(&self) -> HashMap<String, crate::companion::Via> {
        self.calls
            .values()
            .filter_map(|c| {
                let t = c.targets.last()?;
                Some((c.id.clone(), crate::companion::Via { did: c.did.clone(), kind: t.t.kind, number: t.t.number.clone() }))
            })
            .collect()
    }

    /// Feeds one AMI event; `now` in seconds (Unix time).
    pub fn handle(&mut self, m: &Msg, now: f64) -> Vec<PbxEvent> {
        let mut out = vec![];
        match get(m, "Event") {
            "Newchannel" => {
                let (uid, lid) = (get(m, "Uniqueid"), get(m, "Linkedid"));
                if uid.is_empty() || lid.is_empty() {
                    return out;
                }
                self.chan_call.insert(uid.to_string(), lid.to_string());
                let c = self.calls.entry(lid.to_string()).or_insert_with(|| {
                    let (ep, chan) = origin_of(get(m, "Channel"));
                    let p = party(m);
                    Call {
                        id: lid.to_string(),
                        started: now,
                        last: now,
                        caller: p.clone(),
                        did: None,
                        targets: vec![],
                        answered: None,
                        chans: HashSet::new(),
                        origin: FlowOrigin { ep, chan, exten: get(m, "Exten").to_string(), num: p.number, name: p.name.unwrap_or_default() },
                        dials: vec![],
                        voicemail: false,
                        answered_by: None,
                    }
                });
                c.chans.insert(uid.to_string());
                c.last = now;
            }
            "UserEvent" => {
                let Some(c) = self.call_mut(m, now) else { return out };
                match get(m, "UserEvent") {
                    "PbxTarget" => {
                        let kind = match get(m, "Kind") {
                            "user" => TargetKind::User,
                            "group" => TargetKind::Group,
                            "ivr" => TargetKind::Ivr,
                            _ => return out,
                        };
                        let number = get(m, "Number");
                        if number.is_empty() {
                            return out;
                        }
                        let p = party(m);
                        if !p.number.is_empty() {
                            c.caller = p;
                        }
                        let did = get(m, "Did");
                        if !did.is_empty() {
                            c.did = Some(did.to_string());
                        }
                        Self::close_open(c, None, &mut out);
                        let t = Target::new(kind, number);
                        out.push(PbxEvent::CallIncoming { call_id: c.id.clone(), caller: c.caller.clone(), did: c.did.clone(), target: t.clone() });
                        c.targets.push(Tgt {
                            t,
                            state: State::Ringing,
                            rang: vec![],
                            fwd: !matches!(get(m, "Fwd"), "" | "0"),
                            dnd: get(m, "Dnd") == "1",
                        });
                    }
                    "PbxVoicemail" => {
                        c.voicemail = true;
                        Self::close_open(c, Some(MissReason::Voicemail), &mut out)
                    }
                    // ring time over (or DND) without an answer; forwarded/DND targets keep their reason
                    "PbxNoAnswer" => Self::close_open(c, None, &mut out),
                    _ => {}
                }
            }
            "DialBegin" => {
                let Some(c) = self.call_mut(m, now) else { return out };
                let dest = get(m, "DestChannel");
                let (dep, dchan) = origin_of(dest);
                if !dep.is_empty() || !dchan.is_empty() {
                    // trunk legs: "0176…@sip-5" / "site124/1241003" → the dialled number
                    let ds = get(m, "DialString");
                    let to = ds.split_once('@').map(|(n, _)| n).or_else(|| ds.split_once('/').map(|(_, n)| n)).unwrap_or("");
                    c.dials.push(FlowDial {
                        ep: if dep.is_empty() { dchan } else { dep },
                        to: to.to_string(),
                        clip: get(m, "CallerIDNum").replace("<unknown>", ""),
                        status: String::new(),
                        chan: dest.to_string(),
                    });
                }
                if let Some(ep) = pjsip_endpoint(dest) {
                    if let Some(t) = c.targets.iter_mut().rev().find(|t| t.state == State::Ringing) {
                        if !t.rang.iter().any(|r| r == ep) {
                            t.rang.push(ep.to_string());
                        }
                    }
                }
            }
            "DialEnd" => {
                let status = get(m, "DialStatus");
                if let Some(c) = self.call_mut(m, now) {
                    let dest = get(m, "DestChannel");
                    if let Some(d) = c.dials.iter_mut().rev().find(|d| d.chan == dest) {
                        d.status = status.to_string();
                    }
                }
                if status != "ANSWER" {
                    return out;
                }
                let Some(ep) = pjsip_endpoint(get(m, "DestChannel")).map(String::from) else { return out };
                let Some(c) = self.call_mut(m, now) else { return out };
                // before the first target (click-to-call: the own device answers) nothing counts
                if c.answered.is_some() || c.targets.is_empty() {
                    return out;
                }
                c.answered = Some(now);
                c.answered_by = Some(ep.clone());
                let target = c.targets.iter().rposition(|t| t.state == State::Ringing).map(|i| {
                    c.targets[i].state = State::Answered;
                    c.targets[i].t.clone()
                });
                for t in c.targets.iter_mut().filter(|t| t.state == State::Ringing) {
                    t.state = State::Passed;
                }
                out.push(PbxEvent::CallAnswered { call_id: c.id.clone(), caller: c.caller.clone(), target, by: Answerer { endpoint: ep, ..Default::default() } });
            }
            "Hangup" => {
                let uid = get(m, "Uniqueid");
                if let Some(lid) = self.chan_call.remove(uid) {
                    let done = match self.calls.get_mut(&lid) {
                        Some(c) => {
                            c.chans.remove(uid);
                            c.last = now;
                            c.chans.is_empty()
                        }
                        None => false,
                    };
                    if done {
                        if let Some(c) = self.calls.remove(&lid) {
                            self.finish(c, now, &mut out);
                        }
                    }
                }
            }
            "ExtensionStatus" => {
                if get(m, "Context") == "hints" && !get(m, "Exten").is_empty() {
                    out.push(PbxEvent::PresenceChanged { ext: get(m, "Exten").to_string(), state: presence_of(get(m, "StatusText")) });
                }
            }
            _ => {}
        }
        out
    }

    fn call_mut(&mut self, m: &Msg, now: f64) -> Option<&mut Call> {
        let lid = get(m, "Linkedid");
        let lid = if lid.is_empty() { self.chan_call.get(get(m, "Uniqueid"))?.as_str() } else { lid };
        let c = self.calls.get_mut(lid)?;
        c.last = now;
        Some(c)
    }

    /// The call moved on (new target, voicemail, end): targets still ringing were missed.
    fn close_open(c: &mut Call, reason: Option<MissReason>, out: &mut Vec<PbxEvent>) {
        for t in c.targets.iter_mut().filter(|t| t.state == State::Ringing) {
            if t.t.kind == TargetKind::Ivr {
                t.state = State::Passed;
                continue;
            }
            t.state = State::Missed;
            let reason = reason.unwrap_or(if t.fwd {
                MissReason::Forwarded
            } else if t.dnd {
                MissReason::Dnd
            } else {
                MissReason::NoAnswer
            });
            out.push(PbxEvent::CallMissed { call_id: c.id.clone(), caller: c.caller.clone(), did: c.did.clone(), target: t.t.clone(), reason, rang: t.rang.clone() });
        }
    }

    /// The stored flow of a finished call: calls that reached a target or dialled somewhere.
    fn flow(c: &Call, now: f64) -> Option<Flow> {
        if c.targets.is_empty() && c.dials.is_empty() {
            return None;
        }
        // answered without a target (outbound/internal): the answering leg, if any
        let answered = c.answered.or_else(|| c.dials.iter().any(|d| d.status == "ANSWER").then_some(c.started));
        Some(Flow {
            linkedid: c.id.clone(),
            started: c.started,
            duration_s: (now - c.started).max(0.0).round() as u64,
            talk_s: answered.map(|a| (now - a).max(0.0).round() as u64),
            origin: c.origin.clone(),
            did: c.did.clone(),
            steps: c
                .targets
                .iter()
                .map(|t| FlowStep {
                    kind: match t.t.kind {
                        TargetKind::User => "user",
                        TargetKind::Group => "group",
                        TargetKind::Ivr => "ivr",
                    }
                    .into(),
                    number: t.t.number.clone(),
                    state: match t.state {
                        State::Answered => "answered",
                        State::Missed => "missed",
                        State::Passed => "passed",
                        State::Ringing => "ringing",
                    }
                    .into(),
                    fwd: t.fwd,
                    dnd: t.dnd,
                })
                .collect(),
            dials: c.dials.clone(),
            voicemail: c.voicemail,
            answered_by: c.answered_by.clone().or_else(|| c.dials.iter().find(|d| d.status == "ANSWER").map(|d| d.ep.clone())),
        })
    }

    /// Finished calls since the last call (for call_flows).
    pub fn take_flows(&mut self) -> Vec<Flow> {
        std::mem::take(&mut self.flows)
    }

    fn finish(&mut self, mut c: Call, now: f64, out: &mut Vec<PbxEvent>) {
        Self::finish_events(&mut c, now, out);
        if let Some(f) = Self::flow(&c, now) {
            self.flows.push(f);
        }
    }

    fn finish_events(c: &mut Call, now: f64, out: &mut Vec<PbxEvent>) {
        if c.targets.is_empty() {
            return;
        }
        let dnd_or_fwd = |t: &Tgt| if t.fwd { MissReason::Forwarded } else if t.dnd { MissReason::Dnd } else { MissReason::Hangup };
        // still ringing at the end: the caller gave up (or DND/forward ended in an announcement)
        let reasons: Vec<MissReason> = c.targets.iter().map(dnd_or_fwd).collect();
        for (t, r) in c.targets.iter_mut().zip(reasons) {
            if t.state == State::Ringing && t.t.kind != TargetKind::Ivr {
                t.state = State::Missed;
                out.push(PbxEvent::CallMissed { call_id: c.id.clone(), caller: c.caller.clone(), did: c.did.clone(), target: t.t.clone(), reason: r, rang: t.rang.clone() });
            }
        }
        out.push(PbxEvent::CallEnded {
            call_id: c.id.clone(),
            caller: c.caller.clone(),
            answered: c.answered.is_some(),
            duration_s: (now - c.started).max(0.0).round() as u64,
            talk_s: c.answered.map(|a| (now - a).max(0.0).round() as u64),
        });
    }

    /// After a reconnect: channels that are gone were missed; finish their calls.
    pub fn reconcile(&mut self, live: &HashSet<String>, now: f64) -> Vec<PbxEvent> {
        let gone: Vec<String> = self.chan_call.keys().filter(|u| !live.contains(*u)).cloned().collect();
        let mut out = vec![];
        for uid in gone {
            let mut m = Msg::new();
            m.insert("Event".into(), "Hangup".into());
            m.insert("Uniqueid".into(), uid);
            out.extend(self.handle(&m, now));
        }
        // calls without any channel (should not exist) and calls idle for 6 hours
        let stale: Vec<String> = self.calls.values().filter(|c| c.chans.is_empty() || now - c.last > 6.0 * 3600.0).map(|c| c.id.clone()).collect();
        for id in stale {
            if let Some(c) = self.calls.remove(&id) {
                self.chan_call.retain(|_, l| *l != id);
                self.finish(c, now, &mut out);
            }
        }
        out
    }
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Runs the tracker on an AMI event connection; reconnects with backoff.
pub fn spawn(app: AppState) {
    tokio::spawn(async move {
        let mut tracker = Tracker::default();
        let mut backoff = 1;
        loop {
            let started = std::time::Instant::now();
            if let Err(e) = run(&app, &mut tracker).await {
                if started.elapsed().as_secs() > 60 {
                    backoff = 1;
                }
                tracing::warn!("call tracking: {e:#}; reconnecting in {backoff} s");
            }
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}

/// Writes finished call flows (best effort, in the background: the event loop never waits on the DB).
fn store_flows(app: &AppState, flows: Vec<Flow>) {
    for f in flows {
        let db = app.db.clone();
        tokio::spawn(async move {
            let r = sqlx::query(
                "INSERT INTO call_flows (linkedid, started_at, duration_s, talk_s, flow)
                 VALUES ($1, to_timestamp($2), $3, $4, $5) ON CONFLICT (linkedid) DO NOTHING",
            )
            .bind(&f.linkedid)
            .bind(f.started)
            .bind(f.duration_s.min(i32::MAX as u64) as i32)
            .bind(f.talk_s.map(|t| t.min(i32::MAX as u64) as i32))
            .bind(serde_json::to_value(&f).unwrap_or_default())
            .execute(&db)
            .await;
            if let Err(e) = r {
                tracing::warn!("call flow {}: {e:#}", f.linkedid);
            }
        });
    }
}

async fn run(app: &AppState, tracker: &mut Tracker) -> anyhow::Result<()> {
    // security: failed logins for /monitor/sip-auth (secwatch.rs)
    let mut conn = ami::EventConn::open("call,user,security").await?;
    // channels that ended while we were not connected
    let live: HashSet<String> = ami::list("CoreShowChannels", "CoreShowChannel").await?.iter().filter_map(|c| c.get("Uniqueid").cloned()).collect();
    for ev in tracker.reconcile(&live, now()) {
        app.events.publish(ev);
    }
    store_flows(app, tracker.take_flows());
    app.live.set_via(tracker.via());
    tracing::info!("call tracking connected ({} open calls)", tracker.open_calls());
    loop {
        let m = conn.next().await?;
        crate::secwatch::observe(&m);
        for ev in tracker.handle(&m, now()) {
            app.events.publish(ev);
        }
        store_flows(app, tracker.take_flows());
        if matches!(m.get("Event").map(String::as_str), Some("UserEvent" | "Hangup" | "Newchannel")) {
            app.live.set_via(tracker.via());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses recorded AMI traffic ("Key: Value" lines, blank line between events). A line
    /// "#t <seconds>" sets the clock for the following events.
    fn replay(raw: &str) -> Vec<PbxEvent> {
        replay_flows(raw).0
    }

    fn replay_flows(raw: &str) -> (Vec<PbxEvent>, Vec<Flow>) {
        let mut tr = Tracker::default();
        let mut out = vec![];
        let mut t = 0.0;
        let mut m = Msg::new();
        for line in raw.lines().chain(std::iter::once("")) {
            let line = line.trim();
            if let Some(s) = line.strip_prefix("#t ") {
                t = s.parse().unwrap();
            } else if line.starts_with('#') {
                continue;
            } else if line.is_empty() {
                if !m.is_empty() {
                    out.extend(tr.handle(&m, t));
                    m.clear();
                }
            } else if let Some((k, v)) = line.split_once(": ") {
                m.insert(k.to_string(), v.to_string());
            }
        }
        assert_eq!(tr.open_calls(), 0, "calls left open");
        (out, tr.take_flows())
    }

    #[test]
    fn flow_of_group_call_and_outbound_call() {
        let (_, flows) = replay_flows(include_str!("calltrack_fixtures/group_answered.ami"));
        assert_eq!(flows.len(), 1);
        let f = &flows[0];
        assert_eq!(f.steps.first().map(|s| s.kind.as_str()), Some("group"));
        assert!(f.steps.iter().any(|s| s.state == "answered"));
        assert!(f.answered_by.is_some());
        assert!(f.dials.iter().any(|d| d.status == "ANSWER"));
        assert!(f.talk_s.is_some());

        // outbound: device dials over a trunk; no PbxTarget, still a flow
        let raw = "Event: Newchannel\nChannel: PJSIP/dev1020-00000001\nCallerIDNum: 1020\nCallerIDName: Tim\nExten: 017612345678\nUniqueid: 9.1\nLinkedid: 9.1\n\n\
#t 1\nEvent: Newchannel\nChannel: PJSIP/sip-5-00000002\nUniqueid: 9.2\nLinkedid: 9.1\n\n\
Event: DialBegin\nChannel: PJSIP/dev1020-00000001\nCallerIDNum: 04012345\nDestChannel: PJSIP/sip-5-00000002\nDialString: 017612345678@sip-5\nUniqueid: 9.1\nLinkedid: 9.1\n\n\
#t 5\nEvent: DialEnd\nChannel: PJSIP/dev1020-00000001\nDestChannel: PJSIP/sip-5-00000002\nDialStatus: ANSWER\nUniqueid: 9.1\nLinkedid: 9.1\n\n\
#t 65\nEvent: Hangup\nUniqueid: 9.2\n\nEvent: Hangup\nUniqueid: 9.1\n";
        let (evs, flows) = replay_flows(raw);
        assert!(evs.is_empty(), "outbound calls produce no notify events");
        let f = &flows[0];
        assert_eq!(f.origin.ep, "dev1020");
        assert_eq!(f.origin.exten, "017612345678");
        assert_eq!(f.dials, vec![FlowDial { ep: "sip-5".into(), to: "017612345678".into(), clip: "04012345".into(), status: "ANSWER".into(), chan: "PJSIP/sip-5-00000002".into() }]);
        assert_eq!(f.answered_by.as_deref(), Some("sip-5"));
        assert_eq!(f.duration_s, 65);
        // stored JSON round trip (without the internal channel name)
        let back: Flow = serde_json::from_value(serde_json::to_value(f).unwrap()).unwrap();
        assert_eq!(back.dials[0].to, "017612345678");
        assert_eq!(origin_of("IAX2/site124-1234"), (String::new(), "IAX2/site124".to_string()));
    }

    fn names(evs: &[PbxEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.name()).collect()
    }

    #[test]
    fn group_call_answered_by_member() {
        let evs = replay(include_str!("calltrack_fixtures/group_answered.ami"));
        assert_eq!(names(&evs), ["call.incoming", "call.answered", "call.ended"]);
        let PbxEvent::CallIncoming { caller, did, target, .. } = &evs[0] else { panic!() };
        assert_eq!(target, &Target::new(TargetKind::Group, "5110"));
        assert_eq!(caller, &Party { number: "+4940123456".into(), name: Some("Kunde GmbH".into()) });
        assert_eq!(did.as_deref(), Some("+494012345054"));
        let PbxEvent::CallAnswered { by, target, .. } = &evs[1] else { panic!() };
        assert_eq!(by.endpoint, "dev1010a");
        assert_eq!(target.as_ref().unwrap().number, "5110");
        let PbxEvent::CallEnded { answered, duration_s, talk_s, .. } = &evs[2] else { panic!() };
        assert!(answered);
        assert_eq!((*duration_s, *talk_s), (40, Some(30)));
    }

    #[test]
    fn group_unanswered_then_voicemail() {
        let evs = replay(include_str!("calltrack_fixtures/group_voicemail.ami"));
        assert_eq!(names(&evs), ["call.incoming", "call.missed", "call.ended"]);
        let PbxEvent::CallMissed { reason, rang, .. } = &evs[1] else { panic!() };
        assert_eq!(*reason, MissReason::Voicemail);
        assert_eq!(rang, &vec!["dev1010a".to_string(), "dev1015".to_string()]);
        let PbxEvent::CallEnded { answered, talk_s, .. } = &evs[2] else { panic!() };
        assert!(!answered);
        assert_eq!(*talk_s, None);
    }

    #[test]
    fn ivr_to_person_forwarded_to_group_caller_hangs_up() {
        let evs = replay(include_str!("calltrack_fixtures/ivr_forward_hangup.ami"));
        assert_eq!(names(&evs), ["call.incoming", "call.incoming", "call.missed", "call.incoming", "call.missed", "call.ended"]);
        let kinds: Vec<_> = evs.iter().filter_map(|e| if let PbxEvent::CallIncoming { target, .. } = e { Some(target.kind) } else { None }).collect();
        assert_eq!(kinds, [TargetKind::Ivr, TargetKind::User, TargetKind::Group]);
        let PbxEvent::CallMissed { target, reason, .. } = &evs[2] else { panic!() };
        assert_eq!((target.number.as_str(), *reason), ("1010", MissReason::Forwarded));
        let PbxEvent::CallMissed { target, reason, .. } = &evs[4] else { panic!() };
        assert_eq!((target.number.as_str(), *reason), ("5110", MissReason::Hangup));
    }

    #[test]
    fn click_to_call_own_device_does_not_count() {
        let evs = replay(include_str!("calltrack_fixtures/click2call.ami"));
        assert_eq!(names(&evs), ["call.incoming", "call.answered", "call.ended"]);
        let PbxEvent::CallIncoming { caller, target, .. } = &evs[0] else { panic!() };
        assert_eq!(caller.number, "1020");
        assert_eq!(target.number, "1010");
        let PbxEvent::CallAnswered { by, .. } = &evs[1] else { panic!() };
        assert_eq!(by.endpoint, "dev1010a");
    }

    #[test]
    fn recorded_on_the_live_system() {
        let evs = replay(include_str!("calltrack_fixtures/live_recorded.ami"));
        // the voicemail recording of call 1 was still running when call 2 started
        assert_eq!(names(&evs), ["call.incoming", "call.missed", "call.incoming", "call.ended", "call.missed", "call.ended"]);
        let PbxEvent::CallIncoming { target, caller, did, call_id } = &evs[0] else { panic!() };
        assert_eq!((target, caller.number.as_str(), did), (&Target::new(TargetKind::User, "1090"), "", &None));
        assert_eq!(evs[1].call_id(), Some(call_id.as_str()));
        let PbxEvent::CallMissed { reason, .. } = &evs[1] else { panic!() };
        assert_eq!(*reason, MissReason::Voicemail);
        let PbxEvent::CallIncoming { target, .. } = &evs[2] else { panic!() };
        assert_eq!(target, &Target::new(TargetKind::Group, "5110"));
        let PbxEvent::CallEnded { answered, duration_s, call_id: ended, .. } = &evs[3] else { panic!() };
        assert_eq!(ended, call_id);
        assert!(!answered);
        assert!((25..=40).contains(duration_s), "{duration_s}");
        let PbxEvent::CallMissed { reason, .. } = &evs[4] else { panic!() };
        assert_eq!(*reason, MissReason::Hangup);
    }

    #[test]
    fn ring_timeout_without_target_is_no_answer() {
        let evs = replay(include_str!("calltrack_fixtures/user_noanswer.ami"));
        assert_eq!(names(&evs), ["call.incoming", "call.missed", "call.ended"]);
        let PbxEvent::CallMissed { reason, .. } = &evs[1] else { panic!() };
        assert_eq!(*reason, MissReason::NoAnswer);
    }

    #[test]
    fn calls_without_target_produce_nothing() {
        assert!(replay(include_str!("calltrack_fixtures/app_only.ami")).is_empty());
    }

    #[test]
    fn reconnect_finishes_lost_calls() {
        let mut tr = Tracker::default();
        let ev = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Msg>();
        tr.handle(&ev(&[("Event", "Newchannel"), ("Uniqueid", "1.1"), ("Linkedid", "1.1"), ("CallerIDNum", "+491")]), 0.0);
        let inc = tr.handle(&ev(&[("Event", "UserEvent"), ("UserEvent", "PbxTarget"), ("Kind", "user"), ("Number", "1010"), ("Linkedid", "1.1"), ("Uniqueid", "1.1")]), 1.0);
        assert_eq!(names(&inc), ["call.incoming"]);
        let evs = tr.reconcile(&HashSet::new(), 20.0);
        assert_eq!(names(&evs), ["call.missed", "call.ended"]);
        assert_eq!(tr.open_calls(), 0);
    }

    #[test]
    fn presence() {
        let mut tr = Tracker::default();
        let m: Msg = [("Event", "ExtensionStatus"), ("Context", "hints"), ("Exten", "1010"), ("StatusText", "InUse")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(tr.handle(&m, 0.0), vec![PbxEvent::PresenceChanged { ext: "1010".into(), state: Presence::Busy }]);
        assert_eq!(presence_of("Idle"), Presence::Available);
        assert_eq!(presence_of("Unavailable"), Presence::Offline);
    }

    #[test]
    fn endpoints() {
        assert_eq!(pjsip_endpoint("PJSIP/dev1010a-0000002a"), Some("dev1010a"));
        assert_eq!(pjsip_endpoint("PJSIP/sip-6-0000002a"), Some("sip-6"));
        assert_eq!(pjsip_endpoint("Local/1010@group-member-00000001;2"), None);
    }
}
