// SPDX-License-Identifier: GPL-3.0-or-later
//! Admin call log as flows: one line per call (Linkedid), e.g.
//! `1020 Tim · Gigaset (2004) → via 053 (shows 040…) → 0176…` or
//! `0176… → 040… → IVR 7000 → 1015 Andreas · iPhone (2012) ✓`.
//!
//! Source: call_flows (recorded by calltrack.rs while patchbay-web runs: number dialled, targets,
//! every Dial leg, who answered). Calls without a recorded flow (older, or patchbay-web was down)
//! are reconstructed from the CDR rows of the call: calling device or trunk, trunk used, number
//! dialled, answering device; the path through IVRs and groups is not in the CDR.

use std::collections::HashMap;

use sqlx::{FromRow, PgPool};

use crate::{
    calltrack::{Flow, FlowDial},
    i18n::Tr,
};

/// One element of a flow line.
pub struct Hop {
    pub text: String,
    /// muted addition: device, state, caller ID sent
    pub detail: String,
    /// external number: shown with click-to-dial and a block link
    pub num: Option<String>,
}

pub struct CallLine {
    pub when: String,
    pub hops: Vec<Hop>,
    /// English UI key: "answered", "not answered", "voicemail", "busy"
    pub result: &'static str,
    pub ok: bool,
    pub talk_s: i32,
    /// "recorded" (call_flows) or "reconstructed" (CDR only)
    pub recorded: bool,
}

/// Names for endpoints and internal numbers (devices including deleted ones, trunks, number registry).
#[derive(Default)]
pub struct Names {
    /// sip_username → (device number, device label, extension, person)
    devices: HashMap<String, (Option<i16>, String, i16, String)>,
    /// asterisk_ref → trunk name
    trunks: HashMap<String, String>,
    /// internal number → name
    numbers: HashMap<String, String>,
}

impl Names {
    pub async fn load(db: &PgPool) -> anyhow::Result<Names> {
        let devices: Vec<(String, Option<i16>, String, i16, String)> = sqlx::query_as(
            "SELECT d.sip_username, d.number, d.label, u.extension, u.display_name FROM devices d JOIN users u ON u.id = d.user_id",
        )
        .fetch_all(db)
        .await?;
        let trunks: Vec<(String, String)> = sqlx::query_as("SELECT asterisk_ref, name FROM trunks").fetch_all(db).await?;
        let numbers: Vec<(String, String)> = sqlx::query_as(
            "SELECT n.number::text, COALESCE(
                CASE n.kind
                  WHEN 'person' THEN (SELECT display_name FROM users WHERE id = n.ref_id)
                  WHEN 'device' THEN (SELECT u.display_name || ' · ' || d.label FROM devices d JOIN users u ON u.id = d.user_id WHERE d.id = n.ref_id)
                  WHEN 'group' THEN (SELECT name FROM groups WHERE id = n.ref_id)
                  WHEN 'ivr' THEN (SELECT name FROM ivrs WHERE id = n.ref_id)
                  WHEN 'custom_app' THEN (SELECT name FROM custom_apps WHERE id = n.ref_id)
                  ELSE (SELECT name FROM apps WHERE id = n.ref_id)
                END, '') FROM numbers n",
        )
        .fetch_all(db)
        .await?;
        Ok(Names {
            devices: devices.into_iter().map(|(s, n, l, e, p)| (s, (n, l, e, p))).collect(),
            trunks: trunks.into_iter().collect(),
            numbers: numbers.into_iter().collect(),
        })
    }

    fn device(&self, ep: &str) -> Option<Hop> {
        let (num, label, ext, person) = self.devices.get(ep)?;
        let detail = match num {
            Some(n) => format!("{label} ({n})"),
            None => label.clone(),
        };
        Some(Hop { text: format!("{ext} {person}"), detail, num: None })
    }

    /// Trunk name for a PJSIP endpoint or "IAX2/<peer>"
    fn trunk(&self, ep: &str) -> Option<String> {
        self.trunks.get(ep.strip_prefix("IAX2/").unwrap_or(ep)).cloned().or_else(|| ep.starts_with("IAX2/").then(|| ep[5..].to_string()))
    }

    fn internal(&self, number: &str) -> String {
        match self.numbers.get(number) {
            Some(n) if !n.is_empty() => format!("{number} {n}"),
            _ => number.to_string(),
        }
    }
}

fn external(n: &str, detail: String) -> Hop {
    let clean = n.trim();
    let dialable = !clean.is_empty() && clean.chars().all(|c| c.is_ascii_digit() || c == '+');
    Hop { text: if clean.is_empty() { "?".into() } else { clean.to_string() }, detail, num: dialable.then(|| clean.to_string()) }
}

/// Trunk leg: "via <trunk>" (caller ID sent, result unless answered) and the number dialled.
fn trunk_hops(t: Tr, d: &FlowDial, trunk: String, out: &mut Vec<Hop>) {
    let mut detail = if d.clip.is_empty() { String::new() } else { format!("{} {}", t.s("shows"), d.clip) };
    if !d.status.is_empty() && d.status != "ANSWER" {
        detail = [detail, d.status.to_lowercase()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
    }
    out.push(Hop { text: format!("{} {trunk}", t.s("via")), detail, num: None });
    if !d.to.is_empty() {
        // an internal number over an IAX network: no click-to-dial (another site)
        out.push(if d.ep.starts_with("IAX2/") { Hop { text: d.to.clone(), detail: String::new(), num: None } } else { external(&d.to, String::new()) });
    }
}

/// Flow line from a recorded flow.
pub fn from_flow(t: Tr, names: &Names, when: String, f: &Flow) -> CallLine {
    let mut hops = vec![];
    // origin
    if let Some(h) = names.device(&f.origin.ep) {
        hops.push(h);
    } else if let Some(tr) = names.trunk(&f.origin.ep).or_else(|| (!f.origin.chan.is_empty()).then(|| names.trunk(&f.origin.chan)).flatten()) {
        let name = if f.origin.name.is_empty() || f.origin.name == f.origin.num { String::new() } else { f.origin.name.clone() };
        let detail = [name, format!("{} {tr}", t.s("via"))].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        hops.push(external(&f.origin.num, detail));
    } else if !f.origin.c2c.is_empty() {
        hops.push(Hop { text: names.internal(&f.origin.c2c), detail: t.s("click-to-call").into(), num: None });
    } else if !f.origin.num.is_empty() {
        hops.push(Hop { text: names.internal(&f.origin.num), detail: String::new(), num: None });
    }
    if let Some(did) = &f.did {
        hops.push(external(did, t.s("number dialled").into()));
    }
    for s in &f.steps {
        let state = match (s.state.as_str(), s.fwd, s.dnd) {
            (_, true, _) => "forwarded",
            (_, _, true) => "do not disturb",
            ("answered", ..) => "answered",
            ("missed", ..) => "not answered",
            _ => "",
        };
        let kind = match s.kind.as_str() {
            "group" => "group ",
            "ivr" => "IVR ",
            _ => "",
        };
        hops.push(Hop { text: format!("{}{}", t.s(kind), names.internal(&s.number)), detail: t.s(state).into(), num: None });
    }
    // trunk legs (outbound, forwards to external numbers, mobile phones)
    for d in &f.dials {
        if !d.is_trunk() || names.devices.contains_key(&d.ep) {
            continue;
        }
        if let Some(tr) = names.trunk(&d.ep) {
            trunk_hops(t, d, tr, &mut hops);
        }
    }
    // the device that answered: on the person's step if there is one, otherwise as its own hop
    if let Some(h) = f.answered_by.as_deref().and_then(|ep| names.device(ep)) {
        let answered = format!("{} ✓", h.detail);
        if let Some(step) = hops.iter_mut().skip(1).find(|s| s.text == h.text) {
            step.detail = answered;
        } else if hops.first().map(|o| o.text != h.text || o.detail != h.detail).unwrap_or(true) {
            hops.push(Hop { detail: answered, ..h });
        }
    }
    if f.voicemail {
        hops.push(Hop { text: t.s("voicemail").into(), detail: String::new(), num: None });
    }
    let answered = f.talk_s.is_some();
    // busy: a direct call (no person/group/IVR) whose trunk leg was busy; in a group one busy member
    // does not make the call busy
    let busy = !answered && f.steps.is_empty() && f.dials.iter().any(|d| d.is_trunk() && d.status == "BUSY");
    CallLine {
        when,
        hops,
        result: if answered { "answered" } else if f.voicemail { "voicemail" } else if busy { "busy" } else { "not answered" },
        ok: answered,
        talk_s: f.talk_s.unwrap_or(0) as i32,
        recorded: true,
    }
}

/// CDR rows of one call, aggregated in SQL (see `lines`).
#[derive(FromRow)]
pub struct CdrCall {
    linkedid: String,
    when: String,
    channel: Option<String>,
    src: Option<String>,
    dst: Option<String>,
    /// (dstchannel, disposition) of every row
    legs: Vec<String>,
    answered: bool,
    voicemail: bool,
    billsec: i32,
    flow: Option<serde_json::Value>,
    duration_s: Option<i32>,
    talk_s: Option<i32>,
}

fn endpoint(chan: &str) -> String {
    // "PJSIP/abc-0000002a" → "abc"; "IAX2/site124-12" → "IAX2/site124"; Local → ""
    if let Some(rest) = chan.strip_prefix("PJSIP/") {
        return rest.rsplit_once('-').map(|(e, _)| e).unwrap_or(rest).to_string();
    }
    if let Some(rest) = chan.strip_prefix("IAX2/") {
        return format!("IAX2/{}", rest.rsplit_once('-').map(|(e, _)| e).unwrap_or(rest));
    }
    String::new()
}

/// Flow line reconstructed from the CDR (no recorded flow).
pub fn from_cdr(t: Tr, names: &Names, c: &CdrCall) -> CallLine {
    let mut hops = vec![];
    let origin = endpoint(c.channel.as_deref().unwrap_or(""));
    let src = c.src.clone().unwrap_or_default();
    let dst = c.dst.clone().unwrap_or_default();
    let mut answered_dev = None;
    let mut trunk_leg = None;
    for l in &c.legs {
        let (ch, disp) = l.split_once('|').unwrap_or((l, ""));
        let ep = endpoint(ch);
        if ep.is_empty() {
            continue;
        }
        if names.devices.contains_key(&ep) {
            if disp == "ANSWERED" {
                answered_dev = Some(ep);
            }
        } else if let Some(tr) = names.trunk(&ep) {
            trunk_leg = Some((ep, tr, disp.to_string()));
        }
    }
    if let Some(h) = names.device(&origin) {
        hops.push(h);
        match trunk_leg {
            // outbound: src is the caller ID that was sent
            Some((ep, tr, disp)) => {
                let d = FlowDial { ep, to: dst, clip: src, status: if disp == "ANSWERED" { "ANSWER".into() } else { disp.replace(' ', "") }, ..Default::default() };
                trunk_hops(t, &d, tr, &mut hops);
            }
            None if !dst.is_empty() => hops.push(Hop { text: names.internal(&dst), detail: String::new(), num: None }),
            None => {}
        }
    } else {
        let via = names.trunk(&origin).map(|tr| format!("{} {tr}", t.s("via"))).unwrap_or_default();
        hops.push(external(&src, via));
        if !dst.is_empty() && dst != "did" && dst != "s" {
            hops.push(if dst.len() <= 4 { Hop { text: names.internal(&dst), detail: String::new(), num: None } } else { external(&dst, t.s("number dialled").into()) });
        }
    }
    if let Some(h) = answered_dev.and_then(|ep| names.device(&ep)) {
        let answered = format!("{} ✓", h.detail);
        if let Some(step) = hops.iter_mut().skip(1).find(|s| s.text == h.text) {
            step.detail = answered;
        } else if hops.first().map(|o| o.text != h.text || o.detail != h.detail).unwrap_or(true) {
            hops.push(Hop { detail: answered, ..h });
        }
    }
    if c.voicemail {
        hops.push(Hop { text: t.s("voicemail").into(), detail: String::new(), num: None });
    }
    CallLine {
        when: c.when.clone(),
        hops,
        result: if c.answered { "answered" } else if c.voicemail { "voicemail" } else { "not answered" },
        ok: c.answered,
        talk_s: c.billsec,
        recorded: false,
    }
}

/// The last `limit` calls, newest first: recorded flows where present, otherwise from the CDR.
pub async fn lines(t: Tr, db: &PgPool, limit: i64) -> anyhow::Result<Vec<CallLine>> {
    let names = Names::load(db).await?;
    let calls: Vec<CdrCall> = sqlx::query_as(
        "WITH c AS (
           SELECT linkedid, min(id) AS first, to_char(min(calldate), 'DD.MM.YY HH24:MI') AS when,
                  COALESCE((array_agg(channel ORDER BY id) FILTER (WHERE channel NOT LIKE 'Local/%'))[1], (array_agg(channel ORDER BY id))[1]) AS channel,
                  COALESCE((array_agg(src ORDER BY id) FILTER (WHERE channel NOT LIKE 'Local/%'))[1], (array_agg(src ORDER BY id))[1]) AS src,
                  COALESCE((array_agg(dst ORDER BY id) FILTER (WHERE channel NOT LIKE 'Local/%'))[1], (array_agg(dst ORDER BY id))[1]) AS dst,
                  array_agg(COALESCE(dstchannel, '') || '|' || COALESCE(disposition, '') ORDER BY id) AS legs,
                  -- answered: a real Dial leg (not a Local helper) was answered; announcements, voice
                  -- menus and the mailbox answer the caller's channel too but are not a conversation
                  bool_or(disposition = 'ANSWERED' AND COALESCE(dstchannel, '') <> '' AND dstchannel NOT LIKE 'Local/%') AS answered,
                  bool_or(lastapp = 'VoiceMail') AS voicemail,
                  COALESCE(max(billsec) FILTER (WHERE disposition = 'ANSWERED' AND COALESCE(dstchannel, '') <> '' AND dstchannel NOT LIKE 'Local/%'), 0) AS billsec
           FROM cdr
           -- only the newest rows: grouping the whole table (a year of calls) on every page load is slow
           WHERE id > (SELECT COALESCE(max(id), 0) FROM cdr) - $1 * 20
             AND dcontext NOT LIKE 'test-%' AND linkedid IS NOT NULL
           GROUP BY linkedid ORDER BY min(id) DESC LIMIT $1)
         SELECT c.linkedid, c.when, c.channel, c.src, c.dst, c.legs, COALESCE(c.answered, false) AS answered,
                COALESCE(c.voicemail, false) AS voicemail, c.billsec, f.flow, f.duration_s, f.talk_s
         FROM c LEFT JOIN call_flows f ON f.linkedid = c.linkedid ORDER BY c.first DESC",
    )
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(calls
        .iter()
        .map(|c| match c.flow.clone().and_then(|v| serde_json::from_value::<Flow>(v).ok()) {
            Some(mut f) => {
                f.linkedid = c.linkedid.clone();
                f.duration_s = c.duration_s.unwrap_or(0) as u64;
                f.talk_s = c.talk_s.map(|t| t as u64);
                from_flow(t, &names, c.when.clone(), &f)
            }
            None => from_cdr(t, &names, c),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calltrack::{FlowOrigin, FlowStep};
    const EN: Tr = Tr(crate::i18n::Lang::En);

    fn names() -> Names {
        let mut n = Names::default();
        n.devices.insert("dev1020".into(), (Some(2004), "Gigaset".into(), 1020, "Tim".into()));
        n.devices.insert("dev1015".into(), (Some(2012), "iPhone".into(), 1015, "Andreas".into()));
        n.trunks.insert("sip-5".into(), "053".into());
        n.trunks.insert("site124".into(), "FabLab".into());
        n.numbers.insert("7000".into(), "Start".into());
        n.numbers.insert("1015".into(), "Andreas".into());
        n
    }

    fn texts(l: &CallLine) -> Vec<String> {
        l.hops.iter().map(|h| if h.detail.is_empty() { h.text.clone() } else { format!("{} [{}]", h.text, h.detail) }).collect()
    }

    #[test]
    fn outbound_flow() {
        let f = Flow {
            linkedid: "1".into(),
            started: 0.0,
            duration_s: 30,
            talk_s: Some(21),
            origin: FlowOrigin { ep: "dev1020".into(), exten: "017612345678".into(), num: "1020".into(), ..Default::default() },
            did: None,
            steps: vec![],
            dials: vec![FlowDial { ep: "sip-5".into(), to: "017612345678".into(), clip: "04012345".into(), status: "ANSWER".into(), ..Default::default() }],
            voicemail: false,
            answered_by: Some("sip-5".into()),
        };
        let l = from_flow(EN, &names(), "t".into(), &f);
        assert_eq!(texts(&l), ["1020 Tim [Gigaset (2004)]", "via 053 [shows 04012345]", "017612345678"]);
        assert_eq!(l.result, "answered");
        assert_eq!(l.hops[2].num.as_deref(), Some("017612345678"));
    }

    #[test]
    fn inbound_flow_through_ivr() {
        let f = Flow {
            linkedid: "2".into(),
            started: 0.0,
            duration_s: 40,
            talk_s: Some(30),
            origin: FlowOrigin { ep: "sip-5".into(), num: "+4917612345678".into(), ..Default::default() },
            did: Some("+494012345".into()),
            steps: vec![
                FlowStep { kind: "ivr".into(), number: "7000".into(), state: "passed".into(), fwd: false, dnd: false },
                FlowStep { kind: "user".into(), number: "1015".into(), state: "answered".into(), fwd: false, dnd: false },
            ],
            dials: vec![FlowDial { ep: "dev1015".into(), status: "ANSWER".into(), ..Default::default() }],
            voicemail: false,
            answered_by: Some("dev1015".into()),
        };
        let l = from_flow(EN, &names(), "t".into(), &f);
        assert_eq!(
            texts(&l),
            ["+4917612345678 [via 053]", "+494012345 [number dialled]", "IVR 7000 Start", "1015 Andreas [iPhone (2012) ✓]"]
        );
    }

    #[test]
    fn outbound_from_cdr() {
        let c = CdrCall {
            linkedid: "3".into(),
            when: "t".into(),
            channel: Some("PJSIP/dev1020-0000001a".into()),
            src: Some("04012345".into()),
            dst: Some("017612345678".into()),
            legs: vec!["PJSIP/sip-5-0000001b|NO ANSWER".into()],
            answered: false,
            voicemail: false,
            billsec: 0,
            flow: None,
            duration_s: None,
            talk_s: None,
        };
        let l = from_cdr(EN, &names(), &c);
        assert_eq!(texts(&l), ["1020 Tim [Gigaset (2004)]", "via 053 [shows 04012345, noanswer]", "017612345678"]);
        // internal call to another site over IAX
        let c = CdrCall { legs: vec!["IAX2/site124-77|ANSWERED".into()], dst: Some("1241003".into()), answered: true, ..c };
        assert_eq!(texts(&from_cdr(EN, &names(), &c))[1..], ["via FabLab [shows 04012345]", "1241003"]);
    }
}
